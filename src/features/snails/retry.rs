//! Pictures that failed to download get tried again later, with a growing wait (see
//! `store::retry_delay`) up to `store::MAX_ATTEMPTS` tries. After the last one the picture
//! is dropped and counted for its host, shown in `/snail_backfill status`.
//!
//! Discord's attachment links expire, so a retry reads the message again for fresh links
//! instead of keeping the old one. A message that was deleted in the meantime is forgotten.

use std::time::Duration;

use chrono::Utc;
use serenity::all::{ChannelId, GuildId, MessageId};
use tracing::{debug, info, warn};

use super::store::{self, DueFailure};
use super::{backfill, check, collect, index};
use crate::core::{BotCtx, Result};

/// How often the list is checked for pictures due another try.
const CHECK_EVERY: Duration = Duration::from_secs(5 * 60);
/// Pictures tried per check, so a provider that is down for a day doesn't flood Discord
/// with message reads all at once.
const BATCH: i64 = 25;
/// How long a picture in a channel where snails is turned off waits before it's looked at
/// again. It isn't counted as a try.
const OFF_WAIT: i64 = 60 * 60;

/// Starts the retry loop. It stops when the bot shuts down.
pub fn spawn(ctx: &BotCtx) {
    let ctx = ctx.clone();
    ctx.tasks.clone().spawn(async move {
        let timer = ctx.timers.add("Snail picture retries", "every 5m");
        loop {
            timer.sleeping(CHECK_EVERY);
            tokio::select! {
                _ = ctx.shutdown.cancelled() => return,
                _ = tokio::time::sleep(CHECK_EVERY) => {}
            }
            timer.running();
            if let Err(err) = retry_due(&ctx).await {
                warn!("retrying failed snail pictures: {err:#}");
            }
        }
    });
}

/// Drops the pictures that failed their last try, then tries every picture that is due,
/// one message at a time.
async fn retry_due(ctx: &BotCtx) -> Result<()> {
    ctx.db
        .call(|conn| {
            let tx = conn.transaction()?;
            store::drop_given_up(&tx)?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    let now = Utc::now().timestamp();
    let mut due = ctx
        .db
        .call(move |conn| Ok(store::due_failures(conn, now, BATCH)?))
        .await?;
    // One read per message, even when several of its pictures failed.
    due.sort_by_key(|f| f.message);
    let mut start = 0;
    while start < due.len() {
        if ctx.shutdown.is_cancelled() {
            return Ok(());
        }
        let message = due[start].message;
        let end = start
            + due[start..]
                .iter()
                .take_while(|f| f.message == message)
                .count();
        let failures = &due[start..end];
        start = end;
        let first = &failures[0];
        // Turned off here since it failed: leave it on the list for when it's back on, but
        // move it back in the queue. Otherwise these would stay the longest waiting, fill
        // every batch, and no other picture would ever be retried.
        if !ctx.allows(
            "snails",
            Some(GuildId::new(first.guild)),
            ChannelId::new(first.channel),
        ) {
            let until = now + OFF_WAIT;
            ctx.db
                .call(move |conn| Ok(store::postpone_failures(conn, message, until)?))
                .await?;
            continue;
        }
        retry_message(ctx, failures).await?;
    }
    Ok(())
}

/// Reads one message again and tries its failed pictures.
async fn retry_message(ctx: &BotCtx, failures: &[DueFailure]) -> Result<()> {
    let first = &failures[0];
    let id = first.message;
    let now = Utc::now().timestamp();
    let msg = match ChannelId::new(first.channel)
        .message(&ctx.http, MessageId::new(id))
        .await
    {
        Ok(msg) => msg,
        Err(err) if backfill::http_status(&err) == Some(404) => {
            debug!(message = id, "a message with failed pictures was deleted");
            return ctx.db.call(move |conn| Ok(store::forget(conn, id)?)).await;
        }
        Err(err) => {
            // No access right now, or Discord trouble: counts as a failed try.
            let error = format!("reading the message: {err}");
            let sources: Vec<String> = failures.iter().map(|f| f.source.clone()).collect();
            return ctx
                .db
                .call(move |conn| {
                    for source in &sources {
                        store::retry_failed(conn, id, source, &error, now)?;
                    }
                    Ok(())
                })
                .await;
        }
    };

    let pictures = collect::pictures(&msg);
    for failure in failures {
        let found = pictures
            .iter()
            .enumerate()
            .find(|(_, p)| p.source() == failure.source);
        let Some((position, picture)) = found else {
            // An edit removed it.
            let (message, source) = (failure.message, failure.source.clone());
            ctx.db
                .call(move |conn| Ok(store::drop_failure(conn, message, &source)?))
                .await?;
            continue;
        };
        match index::load_one(position, picture).await {
            Ok(loaded) => {
                info!(
                    message = id,
                    source = failure.source,
                    "a failed snail picture downloaded on retry"
                );
                let (failure, stored) = (failure.clone(), loaded.stored());
                let guild = GuildId::new(failure.guild);
                // Saved only if the failure is still listed (one check and the save in the
                // same transaction), so a message deleted meanwhile stays deleted.
                let saved = ctx
                    .db
                    .call(move |conn| {
                        let tx = conn.transaction()?;
                        let saved = store::retry_succeeded(&tx, &failure, &stored)?;
                        tx.commit()?;
                        Ok(saved)
                    })
                    .await?;
                if saved {
                    // Checked like a new picture: it may be a snail.
                    check::on_retried_picture(ctx, guild, &msg, loaded).await?;
                }
            }
            Err(failed) => {
                debug!(
                    message = id,
                    source = failure.source,
                    "snail picture failed again: {}",
                    failed.error
                );
                let source = failure.source.clone();
                ctx.db
                    .call(move |conn| {
                        Ok(store::retry_failed(conn, id, &source, &failed.error, now)?)
                    })
                    .await?;
            }
        }
    }
    Ok(())
}
