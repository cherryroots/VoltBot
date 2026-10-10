//! `/snail_backfill`: reads the server's history so older posts count as snails too.
//!
//! Nothing starts on its own: an admin runs `/snail_backfill start`. The crawl goes channel by
//! channel, 100 messages at a time from the newest back, and saves its place after every page,
//! so pausing, restarting the bot or a network failure never loses work. A crawl an admin
//! started (and didn't pause) carries on when the bot restarts.
//!
//! On every start, channels the backfill has read before are caught up: what was posted
//! there while the bot was offline is read (from the newest message read or saved, forward)
//! and checked like new messages. That's not a backfill start; channels never crawled stay
//! untouched until an admin starts one.

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
use super::store::{self, CrawlStatus, HostFailures};
use super::{check, collect};
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
            let (status, failures) = bot
                .db
                .call(move |conn| {
                    Ok((
                        store::crawl_status(conn, g)?,
                        store::failures_by_host(conn, g)?,
                    ))
                })
                .await?;
            let mut text = describe(&status);
            text.push_str(&describe_failures(&failures));
            text
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

/// Hosts shown in the failed pictures list.
const MAX_HOSTS: usize = 10;

/// The failed pictures grouped by host, so a provider with trouble stands out. Empty when
/// nothing failed.
fn describe_failures(hosts: &[HostFailures]) -> String {
    if hosts.is_empty() {
        return String::new();
    }
    let waiting: u64 = hosts.iter().map(|h| h.waiting).sum();
    let given_up: u64 = hosts.iter().map(|h| h.given_up).sum();
    let mut text = format!(
        "\n\n**Pictures that failed to download:** {waiting} waiting to retry, \
         {given_up} given up"
    );
    for h in hosts.iter().take(MAX_HOSTS) {
        let mut error: String = h.error.chars().take(80).collect();
        if error.len() < h.error.len() {
            error.push('…');
        }
        text.push_str(&format!(
            "\n- `{}`: {}{} (last error: {error})",
            h.host,
            h.waiting + h.given_up,
            if h.given_up > 0 {
                format!(", {} given up", h.given_up)
            } else {
                String::new()
            }
        ));
    }
    if hosts.len() > MAX_HOSTS {
        text.push_str(&format!("\n…and {} more hosts.", hosts.len() - MAX_HOSTS));
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
            return Err(if is_no_access(&err) {
                PageError::NoAccess(err.to_string())
            } else {
                PageError::Retry(err.into())
            });
        }
    };
    // Newest first, so the last one is where the next page starts.
    let oldest = messages.last().map(|m| m.id.get());
    let newest = messages.first().map(|m| m.id.get());
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
            if let Some(newest) = newest {
                // Where the startup catch-up starts.
                store::crawl_newest(conn, id, newest)?;
            }
            Ok(store::crawl_progress(
                conn, id, oldest, count, pictures, finished,
            )?)
        })
        .await
        .map_err(PageError::Retry)?;
    Ok(())
}

// ---- Startup catch-up ----

/// Catches up every server with backfill history, one after another, in the background.
pub fn spawn_catch_up(ctx: &BotCtx, guilds: Vec<GuildId>) {
    let ctx = ctx.clone();
    ctx.tasks.clone().spawn(async move {
        for guild in guilds {
            if let Err(err) = catch_up(&ctx, guild).await {
                warn!(guild = guild.get(), "snail catch-up stopped: {err:#}");
            }
        }
    });
}

/// Reads what was posted while the bot was offline in a server's crawled channels.
async fn catch_up(ctx: &BotCtx, guild: GuildId) -> Result<()> {
    let g = guild.get();
    let channels = ctx
        .db
        .call(move |conn| Ok(store::catch_up_channels(conn, g)?))
        .await?;
    let mut total = 0;
    for (channel, after) in channels {
        if ctx.shutdown.is_cancelled() {
            return Ok(());
        }
        let channel = ChannelId::new(channel);
        if !ctx.allows("snails", Some(guild), channel) {
            continue;
        }
        match catch_up_channel(ctx, guild, channel, after).await {
            Ok(read) => total += read,
            Err(err) => match err.downcast_ref::<serenity::Error>() {
                // Deleted, or the bot lost access: nothing to catch up.
                Some(e) if is_no_access(e) => {
                    debug!(channel = channel.get(), "snail catch-up skipped: {err}")
                }
                _ => warn!(channel = channel.get(), "snail catch-up failed: {err:#}"),
            },
        }
    }
    if total > 0 {
        info!(
            guild = g,
            messages = total,
            "snail catch-up read the missed messages"
        );
    }
    Ok(())
}

/// Reads one channel forward from `after`, oldest first so a repost inside the missed
/// stretch still finds its original. Saves its place after every page. Returns how many
/// messages it read.
async fn catch_up_channel(
    ctx: &BotCtx,
    guild: GuildId,
    channel: ChannelId,
    mut after: u64,
) -> anyhow::Result<usize> {
    let mut read = 0;
    loop {
        let request = GetMessages::new().after(after).limit(PAGE);
        let mut page = channel.messages(&ctx.http, request).await?;
        // Discord sends the page newest first.
        page.sort_by_key(|m| m.id);
        let Some(last) = page.last().map(|m| m.id.get()) else {
            return Ok(read);
        };
        for msg in &page {
            if ctx.shutdown.is_cancelled() {
                return Ok(read);
            }
            if msg.author.bot || !collect::has_content(msg) {
                continue;
            }
            // Missed messages are new messages, so they're checked for snails too.
            if let Err(err) = check::on_new_message(ctx, guild, msg).await {
                debug!(message = msg.id.get(), "couldn't index: {err:#}");
            }
        }
        read += page.len();
        after = last;
        let id = channel.get();
        ctx.db
            .call(move |conn| Ok(store::crawl_newest(conn, id, last)?))
            .await?;
        if page.len() < PAGE as usize {
            return Ok(read);
        }
    }
}

pub fn http_status(err: &serenity::Error) -> Option<u16> {
    match err {
        serenity::Error::Http(HttpError::UnsuccessfulRequest(r)) => Some(r.status_code.as_u16()),
        _ => None,
    }
}

/// Whether Discord refused the request for good: any 4xx except 429 (slow down). Trying
/// again won't help, unlike a network error or a 5xx.
pub fn is_no_access(err: &serenity::Error) -> bool {
    http_status(err).is_some_and(|s| (400..500).contains(&s) && s != 429)
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

    #[test]
    fn failures_text() {
        assert_eq!(describe_failures(&[]), "");
        let hosts = vec![
            HostFailures {
                host: "pbs.twimg.com".into(),
                waiting: 3,
                given_up: 2,
                error: "HTTP status client error (403 Forbidden)".into(),
            },
            HostFailures {
                host: "cdn.discordapp.com".into(),
                waiting: 1,
                given_up: 0,
                error: "x".repeat(100),
            },
        ];
        let text = describe_failures(&hosts);
        assert!(text.contains("4 waiting to retry, 2 given up"), "{text}");
        assert!(text.contains("`pbs.twimg.com`: 5, 2 given up (last error: HTTP"));
        assert!(text.contains("`cdn.discordapp.com`: 1 (last error: "));
        assert!(text.contains('…'));
    }
}
