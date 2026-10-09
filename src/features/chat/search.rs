//! `search_messages`: Discord's message search across the server, limited to the channels
//! the asker can read.
//!
//! serenity has no call for this endpoint yet, so it goes through `reqwest` with the bot
//! token. Discord answers 202 while it is still indexing a server; the search then waits
//! once and tries again.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{NaiveDate, TimeZone as _};
use chrono_tz::Tz;
use reqwest::StatusCode;
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildId, Message};
use tracing::warn;

use super::tools::{can_read, find_member, format_message, home_zone, message_link};
use crate::ai::ToolDef;
use crate::core::{Asker, BotCtx, Result, user_error};

/// Most results one search returns (Discord's own limit).
const MAX_RESULTS: u64 = 25;
/// The attachment and content types Discord can filter by.
const HAS: &[&str] = &[
    "image", "video", "file", "link", "embed", "sound", "sticker", "poll",
];

pub fn def() -> ToolDef {
    ToolDef {
        name: "search_messages",
        description: "Searches the messages of the whole server, like Discord's search bar. Only returns messages from channels the asker can read. Give at least one filter.",
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Words to find in the message text."},
                "from": {"type": "string", "description": "Only messages by this member: a mention, user ID or name."},
                "channel": {"type": "string", "description": "Only this channel: a mention, ID or name. \"here\" for the current one."},
                "has": {
                    "type": "array",
                    "items": {"type": "string", "enum": HAS},
                    "description": "Only messages with all of these."
                },
                "after": {"type": "string", "description": "Only messages on or after this date, YYYY-MM-DD, in the asker's timezone."},
                "before": {"type": "string", "description": "Only messages before this date, YYYY-MM-DD, in the asker's timezone."},
                "sort": {"type": "string", "enum": ["newest", "oldest", "relevance"], "description": "Default newest."},
                "limit": {"type": "integer", "description": "How many, 1 to 25. Default 10."}
            },
        }),
    }
}

pub async fn run(ctx: &BotCtx, asker: &Asker, args: &Value) -> Result<String> {
    let Some(guild) = asker.guild else {
        return Err(user_error("Search only works in a server."));
    };
    let text = |key: &str| args[key].as_str().map(str::trim).filter(|s| !s.is_empty());
    let zone = home_zone(ctx, asker.user).await;

    let author = match text("from") {
        Some(query) => Some(find_member(ctx, guild, query).await?.user.id.get()),
        None => None,
    };
    let channel = match text("channel") {
        Some(name) => {
            let id = find_channel(ctx, asker, guild, name)
                .ok_or_else(|| user_error(format!("No channel matches \"{name}\".")))?;
            if !can_read(ctx, asker, Some(guild.get()), id).await? {
                return Err(user_error("The asker can't read that channel."));
            }
            Some(id.get())
        }
        None => None,
    };
    let has: Vec<&str> = args["has"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let filters = Filters {
        query: text("query"),
        author,
        channel,
        has,
        after: text("after"),
        before: text("before"),
        sort: text("sort").unwrap_or("newest"),
        limit: args["limit"].as_u64().unwrap_or(10).clamp(1, MAX_RESULTS),
    };
    let pairs = query_pairs(&filters, zone).map_err(user_error)?;

    let body = fetch(ctx, guild, &pairs).await?;
    let found: Vec<Message> = body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        // Each hit is a list holding the message (once with context around it).
        .filter_map(|hit| match hit {
            Value::Array(list) => list.first().cloned(),
            message => Some(message.clone()),
        })
        .filter_map(|message| match serde_json::from_value(message) {
            Ok(message) => Some(message),
            Err(err) => {
                warn!("couldn't read a search result: {err}");
                None
            }
        })
        .collect();

    // Discord searched everything the bot can see; keep what the asker can see.
    let mut readable: HashMap<ChannelId, bool> = HashMap::new();
    let mut lines = Vec::new();
    for message in &found {
        let channel = message.channel_id;
        let allowed = match readable.get(&channel) {
            Some(allowed) => *allowed,
            None => {
                let allowed = can_read(ctx, asker, Some(guild.get()), channel)
                    .await
                    .unwrap_or(false);
                readable.insert(channel, allowed);
                allowed
            }
        };
        if allowed {
            lines.push(format!(
                "#{} {} {}",
                channel_name(ctx, guild, channel),
                format_message(ctx, message, zone),
                message_link(Some(guild), channel, message.id)
            ));
        }
    }
    if lines.is_empty() {
        return Ok("No messages found.".to_string());
    }
    Ok(lines.join("\n"))
}

/// The search filters, as the model gave them.
struct Filters<'a> {
    query: Option<&'a str>,
    author: Option<u64>,
    channel: Option<u64>,
    has: Vec<&'a str>,
    after: Option<&'a str>,
    before: Option<&'a str>,
    sort: &'a str,
    limit: u64,
}

/// The query string for Discord's search endpoint. Errors are messages for the model.
fn query_pairs(filters: &Filters, zone: Tz) -> std::result::Result<Vec<(String, String)>, String> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut add = |key: &str, value: String| pairs.push((key.to_string(), value));
    if filters.query.is_none()
        && filters.author.is_none()
        && filters.channel.is_none()
        && filters.has.is_empty()
    {
        return Err("Give at least `query`, `from`, `channel` or `has`.".to_string());
    }
    if let Some(query) = filters.query {
        add("content", query.chars().take(1024).collect());
    }
    if let Some(author) = filters.author {
        add("author_id", author.to_string());
    }
    if let Some(channel) = filters.channel {
        add("channel_id", channel.to_string());
    }
    for has in &filters.has {
        if !HAS.contains(has) {
            return Err(format!("`has` can't be {has}."));
        }
        add("has", has.to_string());
    }
    if let Some(after) = filters.after {
        add("min_id", snowflake_at_date(after, zone)?.to_string());
    }
    if let Some(before) = filters.before {
        add("max_id", snowflake_at_date(before, zone)?.to_string());
    }
    match filters.sort {
        "newest" => {}
        "oldest" => add("sort_order", "asc".to_string()),
        "relevance" => add("sort_by", "relevance".to_string()),
        other => return Err(format!("`sort` can't be {other}.")),
    }
    add("limit", filters.limit.to_string());
    Ok(pairs)
}

/// The smallest message ID Discord could give a message sent at midnight on `date`.
fn snowflake_at_date(date: &str, zone: Tz) -> std::result::Result<u64, String> {
    let day = NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map_err(|_| format!("{date} isn't a date like 2026-10-09."))?;
    let midnight = day.and_hms_opt(0, 0, 0).unwrap_or_default();
    let time = zone
        .from_local_datetime(&midnight)
        .earliest()
        .ok_or_else(|| format!("{date} has no midnight in {}.", zone.name()))?;
    // Discord IDs count milliseconds since 2015 in their top bits.
    const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
    let ms = (time.timestamp_millis() - DISCORD_EPOCH_MS).max(0);
    Ok((ms as u64) << 22)
}

/// Runs the search. Waits once if Discord is still indexing the server.
async fn fetch(ctx: &BotCtx, guild: GuildId, pairs: &[(String, String)]) -> Result<Value> {
    let url = format!("https://discord.com/api/v10/guilds/{guild}/messages/search");
    let mut waited = false;
    loop {
        let response = ctx
            .web
            .get(&url)
            .header("Authorization", ctx.http.token())
            .query(pairs)
            .send()
            .await?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or_default();
        if status == StatusCode::ACCEPTED {
            if waited {
                return Err(user_error(
                    "Discord is still indexing this server for search. Try again in a minute.",
                ));
            }
            let seconds = body["retry_after"].as_f64().unwrap_or(2.0).clamp(0.5, 5.0);
            tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
            waited = true;
            continue;
        }
        if !status.is_success() {
            let reason = body["message"].as_str().unwrap_or("no reason given");
            return Err(anyhow::anyhow!(
                "Discord search failed ({status}): {reason}"
            ));
        }
        return Ok(body);
    }
}

/// A channel from a mention, ID, name or "here".
fn find_channel(ctx: &BotCtx, asker: &Asker, guild: GuildId, text: &str) -> Option<ChannelId> {
    if matches!(text, "here" | "this" | "this channel") {
        return Some(asker.channel);
    }
    let digits = text.trim_start_matches("<#").trim_end_matches('>');
    if let Ok(id) = digits.parse::<u64>()
        && id > 0
    {
        return Some(ChannelId::new(id));
    }
    let name = text.trim_start_matches('#').to_lowercase();
    let guild = ctx.cache.guild(guild)?;
    guild
        .channels
        .values()
        .chain(guild.threads.iter())
        .find(|c| c.name.to_lowercase() == name)
        .map(|c| c.id)
}

/// A channel's or thread's name, from the cache.
fn channel_name(ctx: &BotCtx, guild: GuildId, channel: ChannelId) -> String {
    ctx.cache
        .guild(guild)
        .and_then(|g| {
            g.channels
                .get(&channel)
                .or_else(|| g.threads.iter().find(|t| t.id == channel))
                .map(|c| c.name.clone())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filters() -> Filters<'static> {
        Filters {
            query: None,
            author: None,
            channel: None,
            has: Vec::new(),
            after: None,
            before: None,
            sort: "newest",
            limit: 10,
        }
    }

    #[test]
    fn builds_the_query() {
        let mut f = filters();
        f.query = Some("movie night");
        f.author = Some(7);
        f.has = vec!["link", "image"];
        f.sort = "oldest";
        let pairs = query_pairs(&f, Tz::UTC).unwrap();
        let pairs: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("content", "movie night"),
                ("author_id", "7"),
                ("has", "link"),
                ("has", "image"),
                ("sort_order", "asc"),
                ("limit", "10"),
            ]
        );
    }

    #[test]
    fn needs_a_filter_and_valid_values() {
        assert!(query_pairs(&filters(), Tz::UTC).is_err());
        let mut f = filters();
        f.has = vec!["gif"];
        assert!(query_pairs(&f, Tz::UTC).is_err());
        let mut f = filters();
        f.query = Some("x");
        f.after = Some("yesterday");
        assert!(query_pairs(&f, Tz::UTC).is_err());
    }

    #[test]
    fn dates_become_message_ids() {
        // 2015-01-01 UTC is Discord's epoch, so the ID is 0.
        assert_eq!(snowflake_at_date("2015-01-01", Tz::UTC), Ok(0));
        // A day later: 86 400 000 ms in the top bits.
        assert_eq!(
            snowflake_at_date("2015-01-02", Tz::UTC),
            Ok(86_400_000 << 22)
        );
        // Midnight in Oslo is an hour earlier than in UTC in winter.
        assert_eq!(
            snowflake_at_date("2015-01-02", Tz::Europe__Oslo),
            Ok((86_400_000 - 3_600_000) << 22)
        );
    }
}
