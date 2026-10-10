//! The status message: one picture, drawn again and posted over the old one every minute.
//!
//! Its ID is saved in `control_panel_state`, so after a restart the bot edits the same
//! message instead of posting a new one. The "Updated" line above the picture uses a
//! Discord timestamp (`<t:...:R>`), which Discord keeps counting up on its own: if the bot
//! dies without saying goodbye, the message visibly goes stale.
//!
//! Every 15 minutes the numbers are also saved for the picture's graphs (`history.rs`).
//! If the picture can't be drawn, the message shows the same information as an embed.

use std::time::Duration;

use chrono::Utc;
use rusqlite::OptionalExtension;
use serenity::all::{
    ChannelId, CreateAttachment, CreateEmbed, CreateMessage, EditMessage, HttpError, MessageId,
};
use tracing::warn;

use super::history::{self, History};
use super::render::{self, AiView, Dashboard, FeatureView, SpendView, human_duration};
use crate::ai::{AdminKey, display_name};
use crate::core::logging::error_stats;
use crate::core::{BotCtx, GIT_COMMIT, Result, VERSION};
use crate::util::{shorten, svg};

pub const MIGRATIONS: &[&str] = &[
    // 1
    "CREATE TABLE control_panel_state (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );",
    // 2: numbers over time for the status picture's graphs, see `history.rs`.
    "CREATE TABLE control_panel_samples (
        name TEXT NOT NULL,
        at INTEGER NOT NULL,
        value REAL NOT NULL,
        PRIMARY KEY (name, at)
    );",
];

const GREEN: u32 = 0x2ecc71;
const RED: u32 = 0xe74c3c;

/// What the status message shows: a picture, or the embed when drawing it failed.
/// `content` is the text above it.
struct Status {
    content: String,
    picture: Option<CreateAttachment>,
    embed: Option<CreateEmbed>,
}

pub async fn run(ctx: BotCtx, channel: ChannelId, interval_secs: u64) {
    let mut message = match load_message_id(&ctx, channel).await {
        Ok(message) => message,
        Err(err) => {
            warn!("couldn't read the status message ID: {err:#}");
            None
        }
    };
    let mut tick = tokio::time::interval(Duration::from_secs(interval_secs));
    tokio::task::spawn_blocking(svg::load_fonts);
    // The 15-minute slot of the last saved sample.
    let mut sampled = None;

    loop {
        let stopping = tokio::select! {
            _ = tick.tick() => false,
            _ = ctx.shutdown.cancelled() => true,
        };
        let status = if stopping {
            offline(&ctx).await
        } else {
            let dashboard = gather(&ctx).await;
            let slot = dashboard.now.timestamp() / history::EVERY_SECS;
            if sampled != Some(slot) {
                sampled = Some(slot);
                if let Err(err) = save_samples(&ctx, &dashboard).await {
                    warn!("couldn't save the status graphs' numbers: {err:#}");
                }
            }
            online(dashboard).await
        };
        match show(&ctx, channel, message, status).await {
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
    status: Status,
) -> Result<MessageId> {
    if let Some(id) = existing {
        // Replaces whatever the message had: the old picture, or the embed.
        let mut edit = EditMessage::new()
            .content(status.content.clone())
            .embeds(status.embed.clone().into_iter().collect())
            .remove_all_attachments();
        if let Some(picture) = status.picture.clone() {
            edit = edit.new_attachment(picture);
        }
        match channel.edit_message(&ctx.http, id, edit).await {
            Ok(_) => return Ok(id),
            Err(err) if is_unknown_message(&err) => {} // deleted: post a new one below
            Err(err) => return Err(err.into()),
        }
    }
    let mut create = CreateMessage::new()
        .content(status.content)
        .embeds(status.embed.into_iter().collect());
    if let Some(picture) = status.picture {
        create = create.add_file(picture);
    }
    let message = channel.send_message(&ctx.http, create).await?;
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

/// The status picture. "Updated <t:…:R>" stays as text above it: Discord keeps counting
/// that up on its own, so a crashed bot is obvious even though it can't edit the message.
async fn online(dashboard: Dashboard) -> Status {
    let content = format!("-# Updated <t:{}:R>", dashboard.now.timestamp());
    // Drawing takes some CPU and can read font files, so it runs on tokio's blocking threads.
    let drawn = tokio::task::spawn_blocking(move || {
        let png = svg::to_png(|| render::online_svg(&dashboard));
        (png, dashboard)
    })
    .await;
    match drawn {
        Ok((Ok(png), _)) => Status {
            content,
            picture: Some(CreateAttachment::bytes(png, "status.png")),
            embed: None,
        },
        Ok((Err(err), dashboard)) => {
            warn!("couldn't draw the status picture, showing the embed: {err:#}");
            Status {
                content,
                picture: None,
                embed: Some(online_embed(&dashboard)),
            }
        }
        Err(err) => {
            warn!("drawing the status picture failed: {err}");
            Status {
                content,
                picture: None,
                embed: None,
            }
        }
    }
}

async fn offline(ctx: &BotCtx) -> Status {
    let now = Utc::now();
    let (name, uptime) = (bot_name(ctx), now - ctx.started_at);
    let content = format!("-# Stopped <t:{}:f>", now.timestamp());
    let drawn = tokio::task::spawn_blocking(move || {
        svg::to_png(|| render::offline_svg(&name, &version(), now, uptime))
    })
    .await;
    match drawn {
        Ok(Ok(png)) => Status {
            content,
            picture: Some(CreateAttachment::bytes(png, "status.png")),
            embed: None,
        },
        _ => Status {
            content,
            picture: None,
            embed: Some(offline_embed(ctx)),
        },
    }
}

/// Everything the status shows, read now.
async fn gather(ctx: &BotCtx) -> Dashboard {
    let now = Utc::now();
    let latency = {
        let runners = ctx.shard_manager.runners.lock().await;
        runners.values().filter_map(|runner| runner.latency).max()
    };
    let mut features = Vec::new();
    for feature in ctx.features.iter() {
        if !ctx.gate(feature.name()).enabled {
            continue;
        }
        features.push(FeatureView {
            name: feature.name(),
            stats: feature.stats(ctx).await.map_err(|err| format!("{err:#}")),
        });
    }
    // Enough for the spend graph (this month) and the stats' graphs (7 days).
    let since = (now.timestamp() - 32 * 86_400).max(0);
    let history = ctx
        .db
        .call(move |conn| Ok(history::load(conn, since)?))
        .await
        .unwrap_or_else(|err| {
            warn!("couldn't read the status graphs' numbers: {err:#}");
            History::new()
        });
    Dashboard {
        now,
        bot_name: bot_name(ctx),
        version: version(),
        uptime: now - ctx.started_at,
        servers: ctx.cache.guild_count(),
        latency_ms: latency.map(|l| l.as_secs_f64() * 1000.0),
        memory_mb: memory_used().map(megabytes),
        database_mb: megabytes(database_size(ctx)),
        errors: error_stats(),
        ai: ai_view(ctx),
        spend: spend_view(ctx).await,
        features,
        history,
    }
}

/// Saves this minute's numbers for the graphs: the system's, Claude's spend, and every
/// feature stat that is a plain number.
async fn save_samples(ctx: &BotCtx, d: &Dashboard) -> Result<()> {
    let mut values = vec![(history::DATABASE.to_string(), d.database_mb)];
    values.extend(d.latency_ms.map(|v| (history::LATENCY.to_string(), v)));
    values.extend(d.memory_mb.map(|v| (history::MEMORY.to_string(), v)));
    if let Some(Ok(spend)) = &d.spend {
        values.push((history::SPEND.to_string(), spend.usd));
    }
    for feature in &d.features {
        for stat in feature.stats.iter().flatten() {
            if let Ok(number) = stat.value.parse::<f64>() {
                values.push((history::stat_key(feature.name, &stat.name), number));
            }
        }
    }
    // Every sample of a slot gets the slot's start time, so they line up.
    let at = d.now.timestamp() / history::EVERY_SECS * history::EVERY_SECS;
    ctx.db
        .call(move |conn| Ok(history::record(conn, at, &values)?))
        .await
}

fn bot_name(ctx: &BotCtx) -> String {
    ctx.cache.current_user().name.clone()
}

/// "0.1.0 · abc1234"
fn version() -> String {
    format!("{VERSION} · {GIT_COMMIT}")
}

/// The provider and model chat uses now, and whether it fell back from the main one.
fn ai_view(ctx: &BotCtx) -> Option<AiView> {
    let chat = ctx.ai.chat()?;
    Some(AiView {
        provider: display_name(chat.name()).to_string(),
        model: chat.model().to_string(),
        fallback: ctx.ai.switched().map(|(main, switched)| {
            format!(
                "{} {}; trying again <t:{}:R>",
                display_name(main),
                switched.outage.describe(),
                switched.retry_at.timestamp()
            )
        }),
    })
}

/// What Claude cost this month: Anthropic's bill (with an Admin API key) or the estimate,
/// and, when turned on under `[ai.claude]`, the cache hit rate and the cost per job.
/// `None` when Claude isn't set up.
async fn spend_view(ctx: &BotCtx) -> Option<std::result::Result<SpendView, String>> {
    let spend = ctx.ai.spend.as_ref()?;
    let config = &ctx.config.ai.claude;
    let month = match spend.this_month("claude").await {
        Ok(month) => month,
        Err(err) => return Some(Err(format!("{err:#}"))),
    };
    let admin_key = spend.admin_key();
    let billed = admin_key == AdminKey::Working;
    let admin_key_failing = matches!(admin_key, AdminKey::Failing(_));
    let admin_key = match admin_key {
        AdminKey::Off => "Admin key: off".to_string(),
        AdminKey::Unused => "Admin key: set, but `billed_spend` is off".to_string(),
        AdminKey::Starting => "Admin key: on, reading the bill".to_string(),
        AdminKey::Working => "Admin key: on".to_string(),
        AdminKey::Failing(why) => format!("Admin key: failing ({})", shorten(&why, 120)),
    };
    Some(Ok(SpendView {
        usd: month.usd,
        budget: spend.budget(),
        billed,
        admin_key,
        admin_key_failing,
        last_read: spend.last_read().map(|read| {
            format!(
                "Last read <t:{}:R>: ${:.2} billed",
                read.at.timestamp(),
                read.usd
            )
        }),
        cache_hits: month.cache_hits.filter(|_| config.show_cache_hits),
        jobs: config.show_job_costs.then_some(month.jobs),
    }))
}

/// The same as the picture, as an embed: shown when the picture can't be drawn.
fn online_embed(d: &Dashboard) -> CreateEmbed {
    let errors = &d.errors;
    let mut embed = CreateEmbed::new()
        .title("🟢 Online")
        .colour(GREEN)
        .field("Uptime", human_duration(d.uptime), true)
        .field("Version", format!("{VERSION} (`{GIT_COMMIT}`)"), true)
        .field(
            "Latency",
            d.latency_ms
                .map_or("–".to_string(), |ms| format!("{ms:.0} ms")),
            true,
        )
        .field("Servers", d.servers.to_string(), true)
        .field(
            "Memory",
            d.memory_mb
                .map_or("–".to_string(), |mb| format!("{mb:.1} MB")),
            true,
        )
        .field("Database", format!("{:.1} MB", d.database_mb), true)
        .field("AI", ai_text(d.ai.as_ref()), true)
        .field(
            "Errors",
            format!(
                "{} in the last hour\n{} today",
                errors.last_hour, errors.last_day
            ),
            true,
        );
    if let Some(spend) = &d.spend {
        embed = embed.field("Claude spend", spend_text(spend), true);
    }
    if let Some((at, text)) = &errors.last {
        embed = embed.field(
            "Last error",
            format!("<t:{}:R>\n{}", at.timestamp(), shorten(text, 900)),
            false,
        );
    }

    // One block per feature, from its `stats()`.
    for feature in &d.features {
        let value = match &feature.stats {
            Ok(stats) if stats.is_empty() => continue,
            Ok(stats) => stats
                .iter()
                .map(|stat| format!("{}: {}", stat.name, stat.value))
                .collect::<Vec<_>>()
                .join("\n"),
            Err(err) => format!("couldn't load stats: {err}"),
        };
        embed = embed.field(feature.name, shorten(&value, 1000), true);
    }
    embed
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

fn ai_text(ai: Option<&AiView>) -> String {
    let Some(ai) = ai else {
        return "Off (no key)".to_string();
    };
    let mut lines = vec![ai.provider.clone(), format!("`{}`", ai.model)];
    lines.extend(ai.fallback.clone());
    lines.join("\n")
}

fn spend_text(spend: &std::result::Result<SpendView, String>) -> String {
    let spend = match spend {
        Ok(spend) => spend,
        Err(err) => return format!("couldn't read it: {err}"),
    };
    let budget = match spend.budget {
        budget if budget > 0.0 => format!(" of ${budget:.0}"),
        _ => String::new(),
    };
    let source = if spend.billed { "billed" } else { "estimate" };
    let mut lines = vec![
        format!("${:.2}{budget} this month ({source})", spend.usd),
        spend.admin_key.clone(),
    ];
    lines.extend(spend.last_read.clone());
    if let Some(hits) = spend.cache_hits {
        lines.push(format!("Cache hits: {:.0}% of input", hits * 100.0));
    }
    for (job, usd) in spend.jobs.iter().flatten() {
        lines.push(format!("{job}: ${usd:.2}"));
    }
    lines.join("\n")
}

fn megabytes(bytes: u64) -> f64 {
    bytes as f64 / 1_048_576.0
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
    fn saved_location() {
        let channel = ChannelId::new(5);
        assert_eq!(parse_location("5/9", channel), Some(MessageId::new(9)));
        assert_eq!(parse_location("6/9", channel), None);
        assert_eq!(parse_location("junk", channel), None);
    }
}
