//! Chat's own tools: what the model can look up about Discord and the time.
//!
//! Each tool runs as the person who asked ([`Asker`]): it only reads channels that person
//! can read. Results are plain text, which is what the model reads best.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde_json::{Value, json};
use serenity::all::{
    ChannelId, ChannelType, ContentSafeOptions, GetMessages, GuildChannel, Message, MessageId,
    UserId, content_safe,
};

use crate::ai::ToolDef;
use crate::core::{Asker, BotCtx, Result, settings, user_error};
use crate::util::shorten;

/// Most messages `read_recent_messages` returns.
const MAX_RECENT: u64 = 50;
/// Each message is cut to this many characters in tool results.
const MAX_MESSAGE_CHARS: usize = 600;

pub fn defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "get_current_time",
            description: "The current date and time. Uses the asker's timezone (set with /timezone) unless one is given.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "timezone": {"type": "string", "description": "An IANA timezone like Europe/Oslo. Optional."}
                },
            }),
        },
        ToolDef {
            name: "get_channel_info",
            description: "The current channel's name, topic and type, its parent channel or category, and the server name.",
            parameters: json!({"type": "object", "properties": {}}),
        },
        ToolDef {
            name: "get_user_info",
            description: "A server member's names, ID, roles, join date and timezone. Without `user`, the asker.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "user": {"type": "string", "description": "A mention, user ID or name. Optional."}
                },
            }),
        },
        ToolDef {
            name: "read_recent_messages",
            description: "The messages in this channel just before the one you're answering, oldest first.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "count": {"type": "integer", "description": "How many, 1 to 50. Default 20."}
                },
            }),
        },
        ToolDef {
            name: "get_message",
            description: "One Discord message from its link (https://discord.com/channels/...).",
            parameters: json!({
                "type": "object",
                "properties": {"link": {"type": "string"}},
                "required": ["link"],
            }),
        },
    ]
}

pub async fn run(ctx: &BotCtx, asker: &Asker, name: &str, args: &Value) -> Result<String> {
    let text = |key: &str| args[key].as_str().map(str::trim).filter(|s| !s.is_empty());
    match name {
        "get_current_time" => current_time(ctx, asker, text("timezone")).await,
        "get_channel_info" => channel_info(ctx, asker).await,
        "get_user_info" => user_info(ctx, asker, text("user")).await,
        "read_recent_messages" => {
            let count = args["count"].as_u64().unwrap_or(20).clamp(1, MAX_RECENT);
            recent_messages(ctx, asker, count).await
        }
        "get_message" => {
            let link = text("link").ok_or_else(|| user_error("`link` is required."))?;
            message_from_link(ctx, asker, link).await
        }
        _ => Err(user_error(format!("chat has no tool named {name}"))),
    }
}

/// The asker's timezone, or UTC.
async fn home_zone(ctx: &BotCtx, user: UserId) -> Tz {
    settings::timezone(&ctx.db, user)
        .await
        .ok()
        .flatten()
        .unwrap_or(Tz::UTC)
}

async fn current_time(ctx: &BotCtx, asker: &Asker, zone: Option<&str>) -> Result<String> {
    let zone = match zone {
        Some(name) => Tz::from_str_insensitive(name)
            .map_err(|_| user_error(format!("{name} isn't a timezone I know.")))?,
        None => home_zone(ctx, asker.user).await,
    };
    Ok(format_time(Utc::now(), zone))
}

fn format_time(now: DateTime<Utc>, zone: Tz) -> String {
    let local = now.with_timezone(&zone);
    format!(
        "{} in {} (UTC{}). Unix time {}.",
        local.format("%A %Y-%m-%d %H:%M"),
        zone.name(),
        local.format("%:z"),
        now.timestamp()
    )
}

async fn channel_info(ctx: &BotCtx, asker: &Asker) -> Result<String> {
    let channel = asker.channel.to_channel(&ctx.http).await?;
    let Some(channel) = channel.guild() else {
        return Ok("A direct message conversation.".to_string());
    };
    let mut lines = vec![format!(
        "Channel: #{} ({})",
        channel.name,
        kind_name(channel.kind)
    )];
    if let Some(topic) = channel.topic.as_deref().filter(|t| !t.is_empty()) {
        lines.push(format!("Topic: {topic}"));
    }
    if let Some(parent) = channel.parent_id
        && let Ok(parent) = parent.to_channel(&ctx.http).await
        && let Some(parent) = parent.guild()
    {
        let what = if is_thread(&channel) {
            "In channel"
        } else {
            "In category"
        };
        lines.push(format!("{what}: {}", parent.name));
    }
    if let Some(guild) = ctx.cache.guild(channel.guild_id) {
        lines.push(format!("Server: {}", guild.name));
    }
    Ok(lines.join("\n"))
}

fn kind_name(kind: ChannelType) -> &'static str {
    match kind {
        ChannelType::Text => "text channel",
        ChannelType::Voice => "voice channel",
        ChannelType::News => "announcement channel",
        ChannelType::Stage => "stage channel",
        ChannelType::Forum => "forum",
        ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread => {
            "thread"
        }
        _ => "channel",
    }
}

fn is_thread(channel: &GuildChannel) -> bool {
    matches!(
        channel.kind,
        ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread
    )
}

async fn user_info(ctx: &BotCtx, asker: &Asker, user: Option<&str>) -> Result<String> {
    let Some(guild_id) = asker.guild else {
        let user = asker.user.to_user(&ctx.http).await?;
        return Ok(format!(
            "{} (@{}, ID {})",
            user.display_name(),
            user.name,
            user.id
        ));
    };

    let member = match user {
        None => guild_id.member(&ctx.http, asker.user).await?,
        Some(query) => match parse_user_id(query) {
            Some(id) => guild_id
                .member(&ctx.http, id)
                .await
                .map_err(|_| user_error(format!("Nobody with ID {id} is in this server.")))?,
            None => {
                let found = guild_id.search_members(&ctx.http, query, Some(5)).await?;
                found
                    .into_iter()
                    .next()
                    .ok_or_else(|| user_error(format!("No member matches \"{query}\".")))?
            }
        },
    };

    let roles: Vec<String> = match ctx.cache.guild(guild_id) {
        Some(guild) => member
            .roles
            .iter()
            .filter_map(|id| guild.roles.get(id).map(|r| r.name.clone()))
            .collect(),
        None => Vec::new(),
    };
    let zone = settings::timezone(&ctx.db, member.user.id).await?;
    let mut lines = vec![format!(
        "{} (@{}, ID {})",
        member.display_name(),
        member.user.name,
        member.user.id
    )];
    if let Some(joined) = member.joined_at {
        lines.push(format!("Joined: {}", &joined.to_string()[..10]));
    }
    if !roles.is_empty() {
        lines.push(format!("Roles: {}", roles.join(", ")));
    }
    lines.push(match zone {
        Some(zone) => format!("Timezone: {}", zone.name()),
        None => "Timezone: not set".to_string(),
    });
    Ok(lines.join("\n"))
}

/// "<@123>", "<@!123>" or "123" → the ID.
fn parse_user_id(text: &str) -> Option<UserId> {
    let digits = text
        .trim_start_matches("<@")
        .trim_start_matches('!')
        .trim_end_matches('>');
    digits
        .parse::<u64>()
        .ok()
        .filter(|&id| id > 0)
        .map(UserId::new)
}

async fn recent_messages(ctx: &BotCtx, asker: &Asker, count: u64) -> Result<String> {
    let mut messages = asker
        .channel
        .messages(
            &ctx.http,
            GetMessages::new().before(asker.message).limit(count as u8),
        )
        .await?;
    if messages.is_empty() {
        return Ok("There are no earlier messages.".to_string());
    }
    // Discord returns newest first.
    messages.reverse();
    let zone = home_zone(ctx, asker.user).await;
    let lines: Vec<String> = messages
        .iter()
        .map(|m| format_message(ctx, m, zone))
        .collect();
    Ok(lines.join("\n"))
}

/// "[2026-10-09 14:02] Rene: text (2 attachments)"
fn format_message(ctx: &BotCtx, msg: &Message, zone: Tz) -> String {
    let time = DateTime::from_timestamp(msg.timestamp.unix_timestamp(), 0)
        .unwrap_or_default()
        .with_timezone(&zone)
        .format("%Y-%m-%d %H:%M");
    let name = msg
        .member
        .as_ref()
        .and_then(|m| m.nick.as_deref())
        .unwrap_or(msg.author.display_name());
    let text = content_safe(
        &ctx.cache,
        &msg.content,
        &ContentSafeOptions::default(),
        &msg.mentions,
    );
    let mut line = format!("[{time}] {name}: {}", shorten(&text, MAX_MESSAGE_CHARS));
    if !msg.attachments.is_empty() {
        line.push_str(&format!(" ({} attachments)", msg.attachments.len()));
    }
    for embed in &msg.embeds {
        if let Some(title) = &embed.title {
            line.push_str(&format!(" [embed: {}]", shorten(title, 100)));
        }
    }
    line
}

/// The (server, channel, message) of a message link. The server is `None` for DMs.
fn parse_link(link: &str) -> Option<(Option<u64>, u64, u64)> {
    let url = reqwest::Url::parse(link.trim_matches(['<', '>'])).ok()?;
    let host = url.host_str()?;
    if !(host.ends_with("discord.com") || host.ends_with("discordapp.com")) {
        return None;
    }
    let parts: Vec<&str> = url.path_segments()?.collect();
    let ["channels", guild, channel, message] = parts.as_slice() else {
        return None;
    };
    let guild = if *guild == "@me" {
        None
    } else {
        Some(guild.parse().ok()?)
    };
    Some((guild, channel.parse().ok()?, message.parse().ok()?))
}

async fn message_from_link(ctx: &BotCtx, asker: &Asker, link: &str) -> Result<String> {
    let (guild, channel, message) =
        parse_link(link).ok_or_else(|| user_error("That isn't a Discord message link."))?;
    let channel = ChannelId::new(channel);
    if !can_read(ctx, asker, guild, channel).await? {
        return Err(user_error(
            "That message is in a place the asker can't read.",
        ));
    }
    let msg = channel
        .message(&ctx.http, MessageId::new(message))
        .await
        .map_err(|_| user_error("I couldn't load that message. It may be deleted."))?;
    let zone = home_zone(ctx, asker.user).await;
    Ok(format_message(ctx, &msg, zone))
}

/// Whether the asker may read messages in `channel`. In DMs only the DM itself; in a
/// server, channels in the same server where they can view and read history.
async fn can_read(
    ctx: &BotCtx,
    asker: &Asker,
    guild: Option<u64>,
    channel: ChannelId,
) -> Result<bool> {
    if channel == asker.channel {
        return Ok(true);
    }
    let (Some(guild_id), Some(link_guild)) = (asker.guild, guild) else {
        return Ok(false);
    };
    if guild_id.get() != link_guild {
        return Ok(false);
    }
    let Some(target) = channel.to_channel(&ctx.http).await?.guild() else {
        return Ok(false);
    };
    // Threads use their parent channel's permissions. Private threads also need
    // membership, which isn't checked here, so they're refused.
    let target = match target.kind {
        ChannelType::PrivateThread => return Ok(false),
        ChannelType::PublicThread | ChannelType::NewsThread => {
            let Some(parent) = target.parent_id else {
                return Ok(false);
            };
            match parent.to_channel(&ctx.http).await?.guild() {
                Some(parent) => parent,
                None => return Ok(false),
            }
        }
        _ => target,
    };
    let member = guild_id.member(&ctx.http, asker.user).await?;
    let Some(guild) = ctx.cache.guild(guild_id) else {
        return Ok(false);
    };
    let permissions = guild.user_permissions_in(&target, &member);
    Ok(permissions.view_channel() && permissions.read_message_history())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_links() {
        assert_eq!(
            parse_link("https://discord.com/channels/1/2/3"),
            Some((Some(1), 2, 3))
        );
        assert_eq!(
            parse_link("<https://ptb.discord.com/channels/@me/2/3>"),
            Some((None, 2, 3))
        );
        assert_eq!(
            parse_link("https://discordapp.com/channels/1/2/3"),
            Some((Some(1), 2, 3))
        );
        assert_eq!(parse_link("https://example.com/channels/1/2/3"), None);
        assert_eq!(parse_link("https://discord.com/channels/1/2"), None);
    }

    #[test]
    fn user_ids() {
        assert_eq!(parse_user_id("<@123>"), Some(UserId::new(123)));
        assert_eq!(parse_user_id("<@!123>"), Some(UserId::new(123)));
        assert_eq!(parse_user_id("123"), Some(UserId::new(123)));
        assert_eq!(parse_user_id("rene"), None);
    }

    #[test]
    fn time_format() {
        let now = DateTime::from_timestamp(1_791_500_000, 0).unwrap();
        assert_eq!(
            format_time(now, Tz::Europe__Oslo),
            "Friday 2026-10-09 00:53 in Europe/Oslo (UTC+02:00). Unix time 1791500000."
        );
    }
}
