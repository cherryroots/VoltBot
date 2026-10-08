//! The one background task that delivers reminders.
//!
//! It sends whatever is due, then sleeps until the next reminder (at most an hour). Adding or
//! deleting a reminder wakes it early through `wake`, so it never sleeps past a new one.
//! A reminder is only marked sent after Discord accepted it. If the send with images fails,
//! it's sent again without them; if that fails too, it's retried later with a growing delay.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use chrono::Utc;
use serenity::all::{ChannelId, UserId};
use tokio::sync::Notify;
use tracing::{Instrument as _, error, info, info_span, warn};

use super::store::{self, Reminder};
use super::ui;
use crate::core::{BotCtx, BotEvent, Result};

/// Failed sends before a reminder is dropped (about 8 hours of retrying).
const MAX_ATTEMPTS: u32 = 10;
/// How long delivered reminders are kept, so their snooze buttons keep working.
const KEEP_SENT_SECS: i64 = 7 * 24 * 60 * 60;

pub fn spawn(ctx: &BotCtx, wake: Arc<Notify>) {
    let ctx = ctx.clone();
    let span = info_span!("scheduler", feature = "reminders");
    ctx.tasks.clone().spawn(run(ctx, wake).instrument(span));
}

async fn run(ctx: BotCtx, wake: Arc<Notify>) {
    loop {
        if let Err(err) = send_due(&ctx).await {
            error!("sending due reminders: {err:#}");
        }
        let sleep = match ctx.db.call(|conn| Ok(store::next_try_at(conn)?)).await {
            Ok(next) => sleep_duration(next, Utc::now().timestamp()),
            Err(err) => {
                error!("finding the next reminder: {err:#}");
                Duration::from_secs(60)
            }
        };
        tokio::select! {
            () = tokio::time::sleep(sleep) => {}
            () = wake.notified() => {}
            () = ctx.shutdown.cancelled() => break,
        }
    }
}

async fn send_due(ctx: &BotCtx) -> Result<()> {
    let now = Utc::now().timestamp();
    let due = ctx
        .db
        .call(move |conn| {
            store::purge_sent(conn, now - KEEP_SENT_SECS)?;
            Ok(store::due(conn, now)?)
        })
        .await?;
    for reminder in due {
        let id = reminder.id;
        deliver(ctx, reminder)
            .await
            .with_context(|| format!("reminder {id}"))?;
    }
    Ok(())
}

async fn deliver(ctx: &BotCtx, reminder: Reminder) -> Result<()> {
    let now = Utc::now().timestamp();
    let id = reminder.id;
    let channel = ChannelId::new(reminder.channel_id);

    let mut sent = channel
        .send_message(&ctx.http, ui::fired_message(&reminder, now, true))
        .await;
    // If the images were the problem (too big for the server now, say), send it without
    // them. The reminder links to the original message, which still has them.
    if let Err(err) = &sent
        && !reminder.images.is_empty()
    {
        warn!(
            reminder = id,
            "couldn't send a reminder with its images, sending it without: {err}"
        );
        sent = channel
            .send_message(&ctx.http, ui::fired_message(&reminder, now, false))
            .await;
    }

    match sent {
        Ok(_) => {
            ctx.db
                .call(move |conn| Ok(store::mark_sent(conn, id, now)?))
                .await?;
            info!(reminder = id, "reminder delivered");
            ctx.publish(BotEvent::ReminderFired {
                reminder_id: id,
                user_id: UserId::new(reminder.user_id),
                channel_id: channel,
            });
        }
        Err(err) => {
            let attempts = reminder.attempts + 1;
            if attempts >= MAX_ATTEMPTS {
                error!(
                    reminder = id,
                    user = reminder.user_id,
                    channel = reminder.channel_id,
                    "gave up on a reminder after {attempts} failed sends ({err}). It said: {}",
                    reminder.message
                );
                ctx.db
                    .call(move |conn| Ok(store::delete(conn, id)?))
                    .await?;
            } else {
                let next = now + retry_delay(attempts);
                warn!(
                    reminder = id,
                    channel = reminder.channel_id,
                    "couldn't send a reminder (attempt {attempts}), retrying <t:{next}:R>: {err}"
                );
                ctx.db
                    .call(move |conn| Ok(store::mark_failed(conn, id, next)?))
                    .await?;
            }
        }
    }
    Ok(())
}

/// How long to sleep when the next reminder is at `next`. At most an hour, so delivered
/// reminders get purged and a changed system clock is noticed.
fn sleep_duration(next: Option<i64>, now: i64) -> Duration {
    let secs = next.map_or(3600, |next| (next - now).clamp(1, 3600));
    Duration::from_secs(secs.unsigned_abs())
}

/// Seconds to wait after the `attempts`-th failed send: 1 min, 2, 4, 8, ... up to 6 hours.
fn retry_delay(attempts: u32) -> i64 {
    let doubled = 60_i64 << attempts.saturating_sub(1).min(16);
    doubled.min(6 * 60 * 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleeps() {
        assert_eq!(sleep_duration(None, 0), Duration::from_secs(3600));
        assert_eq!(sleep_duration(Some(90), 30), Duration::from_secs(60));
        assert_eq!(sleep_duration(Some(10), 30), Duration::from_secs(1));
        assert_eq!(sleep_duration(Some(99_999), 0), Duration::from_secs(3600));
    }

    #[test]
    fn retry_delays_grow() {
        assert_eq!(retry_delay(1), 60);
        assert_eq!(retry_delay(2), 120);
        assert_eq!(retry_delay(5), 960);
        assert_eq!(retry_delay(30), 6 * 60 * 60);
        let total: i64 = (1..MAX_ATTEMPTS).map(retry_delay).sum();
        assert!(
            total > 6 * 60 * 60,
            "retries should cover several hours, got {total}s"
        );
    }
}
