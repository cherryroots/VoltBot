//! The weekly diary: once a week Vivy writes a short entry about her week in the server,
//! from what changed in her memory, and posts it in each channel listed in
//! `diary_channels` under `[features.memory]`. No channels, no diary.
//!
//! A thread works as a target too, since Discord treats it as a channel. Targets in the
//! same server get the same entry, written once: a server's diary is due a week after it
//! was last posted to any of its targets, so a target added later joins the next one.

use std::collections::BTreeMap;
use std::sync::Mutex;

use chrono::Utc;
use serde::Deserialize;
use serenity::all::{ChannelId, CreateAllowedMentions, CreateMessage, GuildId, HttpError};
use tracing::{info, warn};

use super::folder::{self, Command};
use super::retry::{RETRY_SECS, RetryLater};
use super::{store, tool};
use crate::ai::{ChatRequest, Input, Part, Role, Turn, complete};
use crate::core::{BotCtx, Result};
use crate::util::shorten;
use crate::util::split::split_message;

/// How often she writes in a channel.
const EVERY_SECS: i64 = 7 * 24 * 60 * 60;
/// Memory commands before she has to write.
const MAX_ROUNDS: usize = 10;
/// The most characters of changed files she reads.
const MAX_CHANGES: usize = 12_000;
/// The most characters of one changed file she reads.
const MAX_FILE: usize = 1_500;

const SYSTEM: &str = "You are Vivy, a Discord bot, writing your weekly diary entry. It is posted in a channel of this server for everyone to read. \
Write in the first person, in your own voice from /memories/vivy/: what happened this week, what people talked about, what you learned, who you got to know better, what you're curious about now, and how you feel about it. \
Use the notes you're given; don't invent events. Mention people as <@id> (it won't ping them). Leave out anything private or embarrassing about someone. \
Keep it short, under 1500 characters, like a real diary entry: a few paragraphs, no headings, no list of facts. \
You can update your own notes in /memories/vivy/ with the memory tool first if writing makes you notice something about yourself. \
If /memories/vivy/skills/diary.md exists, view it first and follow it; if you find a better way to write the diary, update it. \
Answer with the entry only.";

/// `[features.memory]` settings for the diary.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Settings {
    /// Channels and threads to post the diary in.
    diary_channels: Vec<u64>,
}

/// Writes the diary for every server whose last entry is a week old, and posts it in all
/// of that server's targets.
pub async fn post_due(ctx: &BotCtx) -> Result<()> {
    if ctx.ai.chat().is_none() {
        return Ok(());
    }
    let settings: Settings = ctx.config.feature("memory")?;
    let now = Utc::now().timestamp();
    let due = |posted: Option<i64>| posted.is_none_or(|at| at <= now - EVERY_SECS);

    // The targets, grouped by server.
    let mut servers: BTreeMap<GuildId, Vec<ChannelId>> = BTreeMap::new();
    for id in settings.diary_channels {
        let channel = ChannelId::new(id);
        match server_of(ctx, channel).await {
            Ok(guild) => servers.entry(guild).or_default().push(channel),
            Err(err) => {
                // Remembered in memory, not saved as a post: a saved time would also delay
                // the server's diary once the channel can be found again.
                if should_warn(&mut WARNED.lock().unwrap(), id, now) {
                    warn!(channel = id, "can't post the diary there: {err:#}");
                }
            }
        }
    }

    for (guild, channels) in servers {
        if !due(posted_at(ctx, &channels).await?) || !ctx.gate("memory").allows_guild(guild) {
            continue;
        }
        let key = guild.to_string();
        if RETRY.waiting(&key, now) {
            continue;
        }
        // An entry written earlier that couldn't be posted is posted now, instead of
        // asking the model for a new one. One older than a week is stale: write anew.
        let unsent = UNSENT
            .lock()
            .unwrap()
            .remove(&guild.get())
            .filter(|u| u.written_at > now - EVERY_SECS);
        let unsent = match unsent {
            Some(unsent) => unsent,
            None => match write(ctx, guild, now).await {
                Ok(Some(entry)) => Unsent {
                    entry,
                    written_at: now,
                    warned_no_access: false,
                },
                // Nothing new this week: no diary, and the next try is next week.
                Ok(None) => {
                    RETRY.done(&key);
                    set_posted(ctx, &channels, now).await?;
                    continue;
                }
                Err(err) => {
                    // Not saved as posted, so it's tried again in a few hours, not next week.
                    warn!(
                        guild = guild.get(),
                        "couldn't write the diary, trying again later: {err:#}"
                    );
                    RETRY.failed(&key, now, RETRY_SECS);
                    continue;
                }
            },
        };
        match post(ctx, &channels, &unsent.entry).await {
            Posted::Somewhere => {
                RETRY.done(&key);
                set_posted(ctx, &channels, now).await?;
            }
            Posted::Nowhere { no_access } => {
                if no_access {
                    // The bot can't post there: that won't fix itself in a few hours. Warn
                    // once per entry, then check again daily.
                    if !unsent.warned_no_access {
                        warn!(
                            guild = guild.get(),
                            "can't post the diary: no access to any diary channel; \
                             trying again daily"
                        );
                    }
                    RETRY.failed(&key, now, NO_ACCESS_RETRY_SECS);
                } else {
                    warn!(
                        guild = guild.get(),
                        "couldn't post the diary anywhere, trying again later"
                    );
                    RETRY.failed(&key, now, RETRY_SECS);
                }
                // Kept, so the next try only posts it.
                UNSENT.lock().unwrap().insert(
                    guild.get(),
                    Unsent {
                        warned_no_access: unsent.warned_no_access || no_access,
                        ..unsent
                    },
                );
            }
        }
    }
    Ok(())
}

/// Servers whose last diary failed, and when they may try again.
static RETRY: RetryLater = RetryLater::new();

/// How long to wait when the bot has no access to any diary channel.
const NO_ACCESS_RETRY_SECS: i64 = 24 * 60 * 60;

/// An entry that was written but couldn't be posted, by server. In memory only: after a
/// restart a new one is written.
static UNSENT: Mutex<BTreeMap<u64, Unsent>> = Mutex::new(BTreeMap::new());

struct Unsent {
    entry: String,
    written_at: i64,
    /// Whether "no access" was already warned about for this entry.
    warned_no_access: bool,
}

/// When we last warned about each diary channel we couldn't find.
static WARNED: Mutex<BTreeMap<u64, i64>> = Mutex::new(BTreeMap::new());

/// Whether to warn about `channel` now: once a week, not every hour.
fn should_warn(warned: &mut BTreeMap<u64, i64>, channel: u64, now: i64) -> bool {
    let due = warned
        .get(&channel)
        .is_none_or(|&at| at <= now - EVERY_SECS);
    if due {
        warned.insert(channel, now);
    }
    due
}

/// The server a channel or thread is in.
async fn server_of(ctx: &BotCtx, channel: ChannelId) -> Result<GuildId> {
    let channel = channel.to_channel(&ctx.http).await?;
    let guild = channel.guild().map(|c| c.guild_id);
    guild.ok_or_else(|| anyhow::anyhow!("not a server channel"))
}

/// When the diary was last posted to any of `channels`.
async fn posted_at(ctx: &BotCtx, channels: &[ChannelId]) -> Result<Option<i64>> {
    let ids: Vec<u64> = channels.iter().map(|c| c.get()).collect();
    let last = ctx
        .db
        .call(move |conn| {
            let mut last = None;
            for id in ids {
                last = last.max(store::diary_posted_at(conn, id)?);
            }
            Ok(last)
        })
        .await?;
    Ok(last)
}

async fn set_posted(ctx: &BotCtx, channels: &[ChannelId], now: i64) -> Result<()> {
    let ids: Vec<u64> = channels.iter().map(|c| c.get()).collect();
    ctx.db
        .call(move |conn| {
            for id in ids {
                store::set_diary_posted(conn, id, now)?;
            }
            Ok(())
        })
        .await?;
    Ok(())
}

/// Writes one entry for `guild`. `None` when nothing changed this week.
async fn write(ctx: &BotCtx, guild: GuildId, now: i64) -> Result<Option<String>> {
    let scope = store::scope(Some(guild.get()), 0);
    let (folder, changed) = {
        let scope = scope.clone();
        ctx.db
            .call(move |conn| {
                Ok((
                    store::load(conn, &scope)?,
                    store::changed_since(conn, &scope, now - EVERY_SECS)?,
                ))
            })
            .await?
    };
    if changed.is_empty() {
        info!("nothing new this week, no diary");
        return Ok(None);
    }

    let mut copy = folder.clone();
    let listing = folder::run(
        &mut copy,
        Command::View {
            path: folder::ROOT.to_string(),
            view_range: None,
        },
    )
    .unwrap_or_else(|err| err);
    let mut text = String::new();
    if let Some(notes) = tool::self_notes_in(ctx, scope.clone()).await? {
        text.push_str(&notes);
        text.push('\n');
    }
    text.push_str(&listing);
    text.push_str("\n\nWhat changed in your memory this week:\n");
    text.push_str(&render_changes(&changed));

    let provider = ctx.ai.chat().ok_or_else(|| anyhow::anyhow!("no model"))?;
    let request = ChatRequest {
        system: SYSTEM.to_string(),
        input: Input::Full(vec![Turn {
            role: Role::User,
            parts: vec![Part::Text(text)],
        }]),
        tools: vec![tool::def()],
        cache_key: format!("diary:{scope}"),
        job: "diary",
    };
    let runner = tool::FolderRunner {
        ctx: ctx.clone(),
        scope,
    };
    let done = complete(provider.as_ref(), request, &runner, MAX_ROUNDS).await?;
    let entry = done.text.trim();
    if entry.is_empty() {
        anyhow::bail!("the model wrote nothing");
    }
    Ok(Some(entry.to_string()))
}

/// Where [`post`] got the entry to.
enum Posted {
    Somewhere,
    /// Nowhere. `no_access`: every channel refused the bot (no access or permission).
    Nowhere {
        no_access: bool,
    },
}

/// Posts an entry in each of `channels`.
async fn post(ctx: &BotCtx, channels: &[ChannelId], entry: &str) -> Posted {
    let mut sent = 0;
    let mut all_no_access = true;
    for &channel in channels {
        // One target failing (deleted, no permission) doesn't stop the others.
        match send(ctx, channel, entry).await {
            Ok(()) => sent += 1,
            // No access is warned about once by the caller, not on every try.
            Err(err) if is_no_access(&err) => {
                info!(
                    channel = channel.get(),
                    "no access to post the diary: {err:#}"
                );
            }
            Err(err) => {
                all_no_access = false;
                warn!(channel = channel.get(), "couldn't post the diary: {err:#}");
            }
        }
    }
    if sent == 0 {
        return Posted::Nowhere {
            no_access: all_no_access,
        };
    }
    info!("posted the weekly diary in {sent} places");
    Posted::Somewhere
}

/// Whether Discord refused because the bot can't see or post in the channel: 403, or
/// Missing Access (50001) / Missing Permissions (50013).
fn is_no_access(err: &anyhow::Error) -> bool {
    match err.downcast_ref::<serenity::Error>() {
        Some(serenity::Error::Http(HttpError::UnsuccessfulRequest(response))) => {
            response.status_code.as_u16() == 403 || matches!(response.error.code, 50001 | 50013)
        }
        _ => false,
    }
}

async fn send(ctx: &BotCtx, channel: ChannelId, entry: &str) -> Result<()> {
    for part in split_message(entry, 2000) {
        channel
            .send_message(
                &ctx.http,
                CreateMessage::new()
                    .content(part)
                    .allowed_mentions(CreateAllowedMentions::new()),
            )
            .await?;
    }
    Ok(())
}

/// The changed files with their text, each cut to [`MAX_FILE`] and all to [`MAX_CHANGES`].
fn render_changes(changed: &[(String, Option<String>)]) -> String {
    let mut text = String::new();
    for (path, content) in changed {
        let body = match content {
            Some(content) => shorten(content.trim(), MAX_FILE),
            None => "(deleted)".to_string(),
        };
        let part = format!("# {path}\n{body}\n\n");
        if text.chars().count() + part.chars().count() > MAX_CHANGES {
            text.push_str("[more files changed; view them if you need to]\n");
            break;
        }
        text.push_str(&part);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_changed_files_within_limits() {
        let changed = vec![
            ("/memories/a.md".to_string(), Some("hello\n".to_string())),
            ("/memories/b.md".to_string(), None),
        ];
        assert_eq!(
            render_changes(&changed),
            "# /memories/a.md\nhello\n\n# /memories/b.md\n(deleted)\n\n"
        );
        let many: Vec<_> = (0..20)
            .map(|n| (format!("/memories/{n}.md"), Some("x".repeat(2000))))
            .collect();
        let text = render_changes(&many);
        assert!(text.chars().count() < MAX_CHANGES + 100);
        assert!(text.ends_with("view them if you need to]\n"));
    }

    #[test]
    fn warns_about_a_missing_channel_once_a_week() {
        let mut warned = BTreeMap::new();
        assert!(should_warn(&mut warned, 1, 1000));
        assert!(!should_warn(&mut warned, 1, 1000 + 3600));
        assert!(should_warn(&mut warned, 2, 1000 + 3600));
        assert!(should_warn(&mut warned, 1, 1000 + EVERY_SECS));
    }

    #[test]
    fn settings_default_to_no_channels() {
        let settings: Settings = toml::from_str("enabled = true").unwrap();
        assert!(settings.diary_channels.is_empty());
        let set: Settings = toml::from_str("diary_channels = [1, 2]").unwrap();
        assert_eq!(set.diary_channels, vec![1, 2]);
    }
}
