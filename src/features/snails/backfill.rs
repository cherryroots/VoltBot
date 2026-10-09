//! `/snail_backfill`: reads the server's history so older posts count as snails too.
//!
//! Nothing starts on its own: an admin runs `/snail_backfill start`. The crawl goes channel by
//! channel, 100 messages at a time from the newest back, and saves its place after every page,
//! so pausing, restarting the bot or a network failure never loses work. A crawl an admin
//! started (and didn't pause) carries on when the bot restarts.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use serenity::all::{
    ChannelId, ChannelType, GetMessages, GuildChannel, GuildId, HttpError, LightMethod, Request,
    Route, ThreadsData,
};
use serenity::futures::stream::{self, StreamExt};
use tracing::{debug, info, warn};

use super::index;
use super::store::{self, CrawlStatus};
use crate::core::{BotCtx, Context, Result, user_error};

/// Messages indexed at the same time. Each downloads its pictures from Discord's media proxy.
const PARALLEL: usize = 8;
/// Discord's largest page of messages.
const PAGE: u8 = 100;
/// Wait after a failed page before trying again.
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// Servers with a crawl running in this process, so a second start doesn't run two.
static WORKERS: LazyLock<Mutex<HashSet<u64>>> = LazyLock::new(Default::default);

#[derive(Debug, poise::ChoiceParameter)]
pub enum Action {
    #[name = "start"]
    Start,
    #[name = "pause"]
    Pause,
    #[name = "status"]
    Status,
}

/// Hash the server's older messages for Check Snail (admins)
#[poise::command(slash_command, guild_only, ephemeral)]
pub async fn snail_backfill(
    ctx: Context<'_>,
    #[description = "start (or continue), pause, or show progress"] action: Action,
) -> Result<()> {
    if !ctx.data().is_admin(ctx.author().id) {
        return Err(user_error("Only admins can use this command."));
    }
    let guild = ctx
        .guild_id()
        .context("guild_only command without a server")?;
    let bot = ctx.data();
    let g = guild.get();
    let text = match action {
        Action::Start => {
            // Listing every thread takes a few requests.
            ctx.defer_ephemeral().await?;
            let channels = list_channels(bot, guild).await?;
            let total = channels.len();
            let added = bot
                .db
                .call(move |conn| {
                    let tx = conn.transaction()?;
                    let added = store::add_crawl_channels(&tx, g, &channels)?;
                    store::set_running(&tx, g, true)?;
                    tx.commit()?;
                    Ok(added)
                })
                .await?;
            info!(guild = g, channels = total, added, "snail backfill started");
            spawn_worker(bot, guild);
            format!(
                "Started. Found {total} channels and threads ({added} new to the backfill). \
                 `/snail_backfill status` shows how far it is."
            )
        }
        Action::Pause => {
            bot.db
                .call(move |conn| Ok(store::set_running(conn, g, false)?))
                .await?;
            info!(guild = g, "snail backfill paused");
            "Paused after the current page. `/snail_backfill start` continues where it stopped."
                .to_string()
        }
        Action::Status => {
            let status = bot
                .db
                .call(move |conn| Ok(store::crawl_status(conn, g)?))
                .await?;
            describe(&status)
        }
    };
    ctx.say(text).await?;
    Ok(())
}

fn describe(s: &CrawlStatus) -> String {
    let state = if s.channels == 0 {
        "not started"
    } else if s.running {
        "running"
    } else if s.finished == s.channels {
        "finished"
    } else {
        "paused"
    };
    let mut text = format!(
        "**Snail backfill: {state}**\nChannels and threads: {} of {} done\n\
         Messages read: {}, pictures hashed: {}",
        s.finished, s.channels, s.messages, s.pictures
    );
    if s.failed > 0 {
        text.push_str(&format!("\n{} couldn't be read (no access).", s.failed));
    }
    text
}

/// Every channel and thread with messages: text, news, voice and stage chats, plus active
/// and archived threads (forum posts are threads too).
async fn list_channels(ctx: &BotCtx, guild: GuildId) -> Result<Vec<(u64, String)>> {
    let channels = guild
        .channels(&ctx.http)
        .await
        .context("listing the server's channels")?;
    let mut found = Vec::new();
    for channel in channels.values() {
        if matches!(
            channel.kind,
            ChannelType::Text | ChannelType::News | ChannelType::Voice | ChannelType::Stage
        ) {
            found.push((channel.id.get(), channel.name.clone()));
        }
        if matches!(
            channel.kind,
            ChannelType::Text | ChannelType::News | ChannelType::Forum
        ) {
            for private in [false, true] {
                match archived_threads(ctx, channel.id, private).await {
                    Ok(threads) => found.extend(threads.iter().map(thread_entry)),
                    // Private archived threads need Manage Threads; without it they're skipped.
                    Err(err) => debug!("no archived threads for #{}: {err}", channel.name),
                }
            }
        }
    }
    let active = guild
        .get_active_threads(&ctx.http)
        .await
        .context("listing active threads")?;
    found.extend(active.threads.iter().map(thread_entry));
    found.sort();
    found.dedup_by_key(|(id, _)| *id);
    Ok(found)
}

fn thread_entry(thread: &GuildChannel) -> (u64, String) {
    (thread.id.get(), thread.name.clone())
}

/// All archived threads of a channel. Discord pages them by archive time, which serenity's
/// helper sends as a number instead of the timestamp Discord expects, so the request is built
/// here.
async fn archived_threads(
    ctx: &BotCtx,
    channel: ChannelId,
    private: bool,
) -> serenity::Result<Vec<GuildChannel>> {
    let mut threads: Vec<GuildChannel> = Vec::new();
    let mut before: Option<String> = None;
    loop {
        let route = if private {
            Route::ChannelArchivedPrivateThreads {
                channel_id: channel,
            }
        } else {
            Route::ChannelArchivedPublicThreads {
                channel_id: channel,
            }
        };
        let mut params = vec![("limit", "100".to_string())];
        if let Some(before) = &before {
            params.push(("before", before.clone()));
        }
        let page: ThreadsData = ctx
            .http
            .fire(Request::new(route, LightMethod::Get).params(Some(params)))
            .await?;
        let oldest = page
            .threads
            .last()
            .and_then(|t| t.thread_metadata?.archive_timestamp?.to_rfc3339());
        let more = page.has_more;
        threads.extend(page.threads);
        match oldest {
            Some(time) if more => before = Some(time),
            _ => return Ok(threads),
        }
    }
}

/// Starts the crawl for a server unless it's already running here.
pub fn spawn_worker(ctx: &BotCtx, guild: GuildId) {
    if !WORKERS.lock().unwrap().insert(guild.get()) {
        return;
    }
    let ctx = ctx.clone();
    ctx.tasks.clone().spawn(async move {
        if let Err(err) = crawl(&ctx, guild).await {
            warn!(guild = guild.get(), "snail backfill stopped: {err:#}");
            WORKERS.lock().unwrap().remove(&guild.get());
        }
    });
}

/// Why a page couldn't be read.
enum PageError {
    /// The bot can't read this channel; skip it.
    NoAccess(String),
    /// Anything else (network, Discord trouble): try again later.
    Retry(anyhow::Error),
}

async fn crawl(ctx: &BotCtx, guild: GuildId) -> Result<()> {
    let g = guild.get();
    loop {
        if ctx.shutdown.is_cancelled() {
            WORKERS.lock().unwrap().remove(&g);
            return Ok(());
        }
        let (running, next) = ctx
            .db
            .call(move |conn| {
                Ok((
                    store::is_running(conn, g)?,
                    store::next_crawl_channel(conn, g)?,
                ))
            })
            .await?;
        let next = if running { next } else { None };
        let Some((channel, before)) = next else {
            if running {
                ctx.db
                    .call(move |conn| Ok(store::set_running(conn, g, false)?))
                    .await?;
                info!(guild = g, "snail backfill finished");
            }
            WORKERS.lock().unwrap().remove(&g);
            // An admin may have pressed start again just before this; then keep going.
            let again = ctx
                .db
                .call(move |conn| Ok(store::is_running(conn, g)?))
                .await?;
            if again && WORKERS.lock().unwrap().insert(g) {
                continue;
            }
            return Ok(());
        };
        match read_page(ctx, guild, ChannelId::new(channel), before).await {
            Ok(()) => {}
            Err(PageError::NoAccess(error)) => {
                debug!(channel, "snail backfill can't read this channel: {error}");
                ctx.db
                    .call(move |conn| Ok(store::crawl_failed(conn, channel, &error)?))
                    .await?;
            }
            Err(PageError::Retry(err)) => {
                warn!(
                    channel,
                    "snail backfill page failed, retrying in a minute: {err:#}"
                );
                tokio::select! {
                    _ = ctx.shutdown.cancelled() => {}
                    _ = tokio::time::sleep(RETRY_AFTER) => {}
                }
            }
        }
    }
}

/// Reads and indexes one page of a channel, then saves the place.
async fn read_page(
    ctx: &BotCtx,
    guild: GuildId,
    channel: ChannelId,
    before: Option<u64>,
) -> std::result::Result<(), PageError> {
    let mut request = GetMessages::new().limit(PAGE);
    if let Some(before) = before {
        request = request.before(before);
    }
    let messages = match channel.messages(&ctx.http, request).await {
        Ok(messages) => messages,
        Err(err) => {
            return Err(match http_status(&err) {
                Some(403 | 404) => PageError::NoAccess(err.to_string()),
                _ => PageError::Retry(err.into()),
            });
        }
    };
    // Newest first, so the last one is where the next page starts.
    let oldest = messages.last().map(|m| m.id.get());
    let finished = messages.len() < PAGE as usize;
    let jobs: Vec<_> = messages
        .iter()
        .filter(|m| !m.author.bot)
        .map(|msg| async move {
            match index::index_message(ctx, guild, msg).await {
                Ok(count) => count,
                Err(err) => {
                    debug!(message = msg.id.get(), "couldn't index: {err:#}");
                    0
                }
            }
        })
        .collect();
    let pictures: usize = stream::iter(jobs)
        .buffer_unordered(PARALLEL)
        .collect::<Vec<usize>>()
        .await
        .into_iter()
        .sum();
    let (id, count) = (channel.get(), messages.len());
    ctx.db
        .call(move |conn| {
            Ok(store::crawl_progress(
                conn, id, oldest, count, pictures, finished,
            )?)
        })
        .await
        .map_err(PageError::Retry)?;
    Ok(())
}

pub fn http_status(err: &serenity::Error) -> Option<u16> {
    match err {
        serenity::Error::Http(HttpError::UnsuccessfulRequest(r)) => Some(r.status_code.as_u16()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_text() {
        let mut s = CrawlStatus::default();
        assert!(describe(&s).contains("not started"));
        s.channels = 4;
        s.finished = 1;
        s.running = true;
        assert!(describe(&s).contains("running"));
        s.running = false;
        assert!(describe(&s).contains("paused"));
        s.finished = 4;
        s.failed = 1;
        let text = describe(&s);
        assert!(text.contains("finished") && text.contains("1 couldn't be read"));
    }
}
