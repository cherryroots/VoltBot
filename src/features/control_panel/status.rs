//! The status message: one message, edited every minute.
//!
//! Its ID is saved in `control_panel_state`, so after a restart the bot edits the same
//! message instead of posting a new one. The "Updated" line uses a Discord timestamp
//! (`<t:...:R>`), which Discord keeps counting up on its own: if the bot dies without
//! saying goodbye, the message visibly goes stale.

use std::time::Duration;

use chrono::{TimeDelta, Utc};
use rusqlite::OptionalExtension;
use serenity::all::{ChannelId, CreateEmbed, CreateMessage, EditMessage, HttpError, MessageId};
use tracing::warn;

use crate::ai::display_name;
use crate::core::logging::error_stats;
use crate::core::{BotCtx, GIT_COMMIT, Result, VERSION};
use crate::util::shorten;

pub const MIGRATIONS: &[&str] = &[
    // 1
    "CREATE TABLE control_panel_state (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );",
];

const GREEN: u32 = 0x2ecc71;
const RED: u32 = 0xe74c3c;

pub async fn run(ctx: BotCtx, channel: ChannelId, interval_secs: u64) {
    let mut message = match load_message_id(&ctx, channel).await {
        Ok(message) => message,
        Err(err) => {
            warn!("couldn't read the status message ID: {err:#}");
            None
        }
    };
    let mut tick = tokio::time::interval(Duration::from_secs(interval_secs));

    loop {
        let stopping = tokio::select! {
            _ = tick.tick() => false,
            _ = ctx.shutdown.cancelled() => true,
        };
        let embed = if stopping {
            offline_embed(&ctx)
        } else {
            online_embed(&ctx).await
        };
        match show(&ctx, channel, message, embed).await {
            Ok(id) if Some(id) != message => {
                message = Some(id);
                if let Err(err) = save_message_id(&ctx, channel, id).await {
                    warn!("couldn't save the status message ID: {err:#}");
                }
            }
            Ok(_) => {}
            Err(err) => warn!("couldn't update the status message: {err:#}"),
        }
        if stopping {
            break;
        }
    }
}

/// Edits the existing message, or posts a new one if there is none (or it was deleted).
async fn show(
    ctx: &BotCtx,
    channel: ChannelId,
    existing: Option<MessageId>,
    embed: CreateEmbed,
) -> Result<MessageId> {
    if let Some(id) = existing {
        let edit = EditMessage::new().embed(embed.clone());
        match channel.edit_message(&ctx.http, id, edit).await {
            Ok(_) => return Ok(id),
            Err(err) if is_unknown_message(&err) => {} // deleted: post a new one below
            Err(err) => return Err(err.into()),
        }
    }
    let message = channel
        .send_message(&ctx.http, CreateMessage::new().embed(embed))
        .await?;
    Ok(message.id)
}

fn is_unknown_message(err: &serenity::Error) -> bool {
    const UNKNOWN_MESSAGE: isize = 10008;
    matches!(
        err,
        serenity::Error::Http(HttpError::UnsuccessfulRequest(response))
            if response.error.code == UNKNOWN_MESSAGE
    )
}

async fn online_embed(ctx: &BotCtx) -> CreateEmbed {
    let now = Utc::now();
    let errors = error_stats();
    let latency = {
        let runners = ctx.shard_manager.runners.lock().await;
        runners.values().filter_map(|runner| runner.latency).max()
    };

    let mut embed = CreateEmbed::new()
        .title("🟢 Online")
        .colour(GREEN)
        .field("Uptime", human_duration(now - ctx.started_at), true)
        .field("Version", format!("{VERSION} (`{GIT_COMMIT}`)"), true)
        .field(
            "Latency",
            latency.map_or("–".to_string(), |l| format!("{} ms", l.as_millis())),
            true,
        )
        .field("Servers", ctx.cache.guild_count().to_string(), true)
        .field(
            "Memory",
            memory_used().map_or("–".to_string(), megabytes),
            true,
        )
        .field("Database", megabytes(database_size(ctx)), true)
        .field("AI", ai_provider(ctx).await, true)
        .field(
            "Errors",
            format!(
                "{} in the last hour\n{} today",
                errors.last_hour, errors.last_day
            ),
            true,
        );
    if let Some(spend) = claude_spend(ctx).await {
        embed = embed.field("Claude spend", spend, true);
    }
    if let Some((at, text)) = errors.last {
        embed = embed.field(
            "Last error",
            format!("<t:{}:R>\n{}", at.timestamp(), shorten(&text, 900)),
            false,
        );
    }

    // One block per feature, from its `stats()`.
    for feature in ctx.features.iter() {
        if !ctx.gate(feature.name()).enabled {
            continue;
        }
        let value = match feature.stats(ctx).await {
            Ok(stats) if stats.is_empty() => continue,
            Ok(stats) => stats
                .iter()
                .map(|stat| format!("{}: {}", stat.name, stat.value))
                .collect::<Vec<_>>()
                .join("\n"),
            Err(err) => format!("couldn't load stats: {err}"),
        };
        embed = embed.field(feature.name(), shorten(&value, 1000), true);
    }

    embed.field("Updated", format!("<t:{}:R>", now.timestamp()), false)
}

fn offline_embed(ctx: &BotCtx) -> CreateEmbed {
    let now = Utc::now();
    CreateEmbed::new()
        .title("🔴 Offline")
        .colour(RED)
        .field("Stopped", format!("<t:{}:f>", now.timestamp()), true)
        .field("Was up for", human_duration(now - ctx.started_at), true)
        .field("Version", format!("{VERSION} (`{GIT_COMMIT}`)"), true)
}

/// The provider and model chat uses now, and whether it fell back from the main one.
async fn ai_provider(ctx: &BotCtx) -> String {
    let Some(chat) = ctx.ai.chat() else {
        return "Off (no key)".to_string();
    };
    let mut lines = vec![
        display_name(chat.name()).to_string(),
        format!("`{}`", chat.model()),
    ];
    if let Some((main, switched)) = ctx.ai.switched() {
        lines.push(format!(
            "{} {}; trying again <t:{}:R>",
            display_name(main),
            switched.outage.describe(),
            switched.retry_at.timestamp()
        ));
    }
    lines.join("\n")
}

/// What Claude cost this month: Anthropic's bill (with an Admin API key) or the estimate,
/// and, when turned on under `[ai.claude]`, the cache hit rate and the cost per job.
/// `None` when Claude isn't set up.
async fn claude_spend(ctx: &BotCtx) -> Option<String> {
    let spend = ctx.ai.spend.as_ref()?;
    let config = &ctx.config.ai.claude;
    let month = match spend.this_month("claude").await {
        Ok(month) => month,
        Err(err) => return Some(format!("couldn't read it: {err}")),
    };
    let budget = match spend.budget() {
        budget if budget > 0.0 => format!(" of ${budget:.0}"),
        _ => String::new(),
    };
    let source = match spend.billed_at() {
        Some(at) => format!("billed, read <t:{}:R>", at.timestamp()),
        None => "estimate".to_string(),
    };
    let mut lines = vec![format!("${:.2}{budget} this month ({source})", month.usd)];
    if config.show_cache_hits
        && let Some(hits) = month.cache_hits
    {
        lines.push(format!("Cache hits: {:.0}% of input", hits * 100.0));
    }
    if config.show_job_costs {
        for (job, usd) in &month.jobs {
            lines.push(format!("{job}: ${usd:.2}"));
        }
    }
    Some(lines.join("\n"))
}

/// "3d 4h 12m", "4h 12m" or "12m".
fn human_duration(duration: TimeDelta) -> String {
    let minutes = duration.num_minutes().max(0);
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours) {
        (0, 0) => format!("{minutes}m"),
        (0, _) => format!("{hours}h {minutes}m"),
        _ => format!("{days}d {hours}h {minutes}m"),
    }
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

/// Memory used by this process.
fn memory_used() -> Option<u64> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, get_current_pid};
    let pid = get_current_pid().ok()?;
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    Some(system.process(pid)?.memory())
}

/// The database file plus its write-ahead log.
fn database_size(ctx: &BotCtx) -> u64 {
    let path = &ctx.config.database;
    [path.clone(), format!("{path}-wal")]
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum()
}

/// The saved status message, if it's in the configured channel.
async fn load_message_id(ctx: &BotCtx, channel: ChannelId) -> Result<Option<MessageId>> {
    let saved: Option<String> = ctx
        .db
        .call(|conn| {
            Ok(conn
                .query_row(
                    "SELECT value FROM control_panel_state WHERE key = 'status_message'",
                    [],
                    |row| row.get(0),
                )
                .optional()?)
        })
        .await?;
    Ok(saved.and_then(|value| parse_location(&value, channel)))
}

async fn save_message_id(ctx: &BotCtx, channel: ChannelId, message: MessageId) -> Result<()> {
    let value = format!("{channel}/{message}");
    ctx.db
        .call(move |conn| {
            conn.execute(
                "INSERT INTO control_panel_state (key, value) VALUES ('status_message', ?1)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                [value],
            )?;
            Ok(())
        })
        .await
}

/// Reads "channel/message", ignoring it when the channel was changed in the config.
fn parse_location(value: &str, channel: ChannelId) -> Option<MessageId> {
    let (saved_channel, message) = value.split_once('/')?;
    if saved_channel != channel.to_string() {
        return None;
    }
    Some(MessageId::new(message.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(human_duration(TimeDelta::minutes(12)), "12m");
        assert_eq!(human_duration(TimeDelta::minutes(4 * 60 + 12)), "4h 12m");
        assert_eq!(
            human_duration(TimeDelta::minutes(3 * 1440 + 4 * 60 + 12)),
            "3d 4h 12m"
        );
        assert_eq!(human_duration(TimeDelta::seconds(-5)), "0m");
    }

    #[test]
    fn saved_location() {
        let channel = ChannelId::new(5);
        assert_eq!(parse_location("5/9", channel), Some(MessageId::new(9)));
        assert_eq!(parse_location("6/9", channel), None);
        assert_eq!(parse_location("junk", channel), None);
    }
}
