//! Chat's own tools: what the model can look up about Discord and the time.
//! `search_messages` lives in `search.rs`.
//!
//! Each tool runs as the person who asked ([`Asker`]): it only reads channels that person
//! can read. Results are plain text, which is what the model reads best.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde_json::{Value, json};
use serenity::all::{
    ChannelId, ChannelType, ContentSafeOptions, GetMessages, GuildChannel, GuildId, Member,
    Message, MessageId, ReactionType, ScheduledEventStatus, UserId, content_safe,
};

use super::search;
use crate::ai::ToolDef;
use crate::core::{Asker, BotCtx, Result, settings, user_error};
use crate::util::shorten;

/// Most messages `read_recent_messages` returns.
const MAX_RECENT: u64 = 50;
/// Each message is cut to this many characters in tool results.
const MAX_MESSAGE_CHARS: usize = 600;
/// Most pinned messages `get_pinned_messages` returns.
const MAX_PINS: usize = 25;

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
            name: "list_channels",
            description: "Every channel in this server the asker can see, by category, with each channel's topic. Use it to learn what the channels are for.",
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
        search::def(),
        ToolDef {
            name: "get_pinned_messages",
            description: "The pinned messages of the current channel, newest first, with links.",
            parameters: json!({"type": "object", "properties": {}}),
        },
        ToolDef {
            name: "list_server_events",
            description: "The server's upcoming and ongoing scheduled events: name, time, place, description and how many are interested.",
            parameters: json!({"type": "object", "properties": {}}),
        },
    ]
}

pub async fn run(ctx: &BotCtx, asker: &Asker, name: &str, args: &Value) -> Result<String> {
    let text = |key: &str| args[key].as_str().map(str::trim).filter(|s| !s.is_empty());
    match name {
        "get_current_time" => current_time(ctx, asker, text("timezone")).await,
        "get_channel_info" => channel_info(ctx, asker).await,
        "list_channels" => list_channels(ctx, asker).await,
        "get_user_info" => user_info(ctx, asker, text("user")).await,
        "read_recent_messages" => {
            let count = args["count"].as_u64().unwrap_or(20).clamp(1, MAX_RECENT);
            recent_messages(ctx, asker, count).await
        }
        "get_message" => {
            let link = text("link").ok_or_else(|| user_error("`link` is required."))?;
            message_from_link(ctx, asker, link).await
        }
        "search_messages" => search::run(ctx, asker, args).await,
        "get_pinned_messages" => pinned_messages(ctx, asker).await,
        "list_server_events" => server_events(ctx, asker).await,
        _ => Err(user_error(format!("chat has no tool named {name}"))),
    }
}

/// The asker's timezone, or UTC.
pub(super) async fn home_zone(ctx: &BotCtx, user: UserId) -> Tz {
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

/// One channel in [`list_channels`], with only what the list shows.
struct ChannelEntry {
    id: u64,
    name: String,
    kind: ChannelType,
    topic: Option<String>,
    parent: Option<u64>,
    position: u16,
}

async fn list_channels(ctx: &BotCtx, asker: &Asker) -> Result<String> {
    let Some(guild_id) = asker.guild else {
        return Ok("A direct message conversation has no channels.".to_string());
    };
    let member = guild_id.member(&ctx.http, asker.user).await?;
    let guild = ctx
        .cache
        .guild(guild_id)
        .ok_or_else(|| user_error("I don't know this server yet."))?;
    let entries: Vec<ChannelEntry> = guild
        .channels
        .values()
        .filter(|c| {
            c.kind == ChannelType::Category || guild.user_permissions_in(c, &member).view_channel()
        })
        .map(|c| ChannelEntry {
            id: c.id.get(),
            name: c.name.clone(),
            kind: c.kind,
            topic: c.topic.clone().filter(|t| !t.trim().is_empty()),
            parent: c.parent_id.map(|p| p.get()),
            position: c.position,
        })
        .collect();
    Ok(format!(
        "Server: {}\n{}",
        guild.name,
        render_channels(&entries, asker.channel.get())
    ))
}

/// Channels without a category first, then each category with its channels, in Discord's
/// order. Categories the asker sees nothing in are left out.
fn render_channels(entries: &[ChannelEntry], here: u64) -> String {
    let by_position = |list: &mut Vec<&ChannelEntry>| list.sort_by_key(|c| (c.position, c.id));
    let line = |c: &ChannelEntry| {
        let mut text = format!("#{} ({})", c.name, kind_name(c.kind));
        if c.id == here {
            text.push_str(" [you are here]");
        }
        if let Some(topic) = &c.topic {
            text.push_str(&format!(": {}", shorten(&topic.replace('\n', " "), 200)));
        }
        text
    };
    let in_category = |parent: Option<u64>| {
        let mut list: Vec<&ChannelEntry> = entries
            .iter()
            .filter(|c| c.kind != ChannelType::Category && c.parent == parent)
            .collect();
        by_position(&mut list);
        list
    };
    let mut lines: Vec<String> = in_category(None).into_iter().map(line).collect();
    let mut categories: Vec<&ChannelEntry> = entries
        .iter()
        .filter(|c| c.kind == ChannelType::Category)
        .collect();
    by_position(&mut categories);
    for category in categories {
        let channels = in_category(Some(category.id));
        if channels.is_empty() {
            continue;
        }
        lines.push(format!("Category {}:", category.name));
        lines.extend(channels.into_iter().map(|c| format!("  {}", line(c))));
    }
    lines.join("\n")
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
        Some(query) => find_member(ctx, guild_id, query).await?,
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

/// A server member from a mention, user ID or name.
pub(super) async fn find_member(ctx: &BotCtx, guild: GuildId, query: &str) -> Result<Member> {
    match parse_user_id(query) {
        Some(id) => guild
            .member(&ctx.http, id)
            .await
            .map_err(|_| user_error(format!("Nobody with ID {id} is in this server."))),
        None => guild
            .search_members(&ctx.http, query, Some(5))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| user_error(format!("No member matches \"{query}\"."))),
    }
}

/// "<@123>", "<@!123>" or "123" → the ID.
pub(super) fn parse_user_id(text: &str) -> Option<UserId> {
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

/// "[2026-10-09 14:02] Cherry: text (2 attachments)"
pub(super) fn format_message(ctx: &BotCtx, msg: &Message, zone: Tz) -> String {
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
    let reactions: Vec<(String, u64)> = msg
        .reactions
        .iter()
        .map(|r| {
            let emoji = match &r.reaction_type {
                ReactionType::Custom {
                    name: Some(name), ..
                } => format!(":{name}:"),
                other => other.to_string(),
            };
            (emoji, r.count)
        })
        .collect();
    line.push_str(&reaction_text(&reactions));
    line
}

/// " [reactions: 😂 3, :pog: 1]", or nothing without reactions.
fn reaction_text(reactions: &[(String, u64)]) -> String {
    if reactions.is_empty() {
        return String::new();
    }
    let list: Vec<String> = reactions
        .iter()
        .map(|(emoji, count)| format!("{emoji} {count}"))
        .collect();
    format!(" [reactions: {}]", list.join(", "))
}

/// The link that opens a message in Discord.
pub(super) fn message_link(
    guild: Option<GuildId>,
    channel: ChannelId,
    message: MessageId,
) -> String {
    let guild = guild.map_or("@me".to_string(), |g| g.to_string());
    format!("https://discord.com/channels/{guild}/{channel}/{message}")
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
    // Discord IDs are never 0, and serenity panics on an ID of 0.
    let channel: u64 = channel.parse().ok().filter(|id| *id != 0)?;
    let message: u64 = message.parse().ok().filter(|id| *id != 0)?;
    Some((guild, channel, message))
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
pub(super) async fn can_read(
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
    // The link's server part is just text, so check the channel really is in the
    // asker's server. Otherwise a link could reach into another server.
    if target.guild_id != guild_id {
        return Ok(false);
    }
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

async fn pinned_messages(ctx: &BotCtx, asker: &Asker) -> Result<String> {
    let pins = asker.channel.pins(&ctx.http).await?;
    if pins.is_empty() {
        return Ok("This channel has no pinned messages.".to_string());
    }
    let zone = home_zone(ctx, asker.user).await;
    let lines: Vec<String> = pins
        .iter()
        .take(MAX_PINS)
        .map(|m| {
            format!(
                "{} {}",
                format_message(ctx, m, zone),
                message_link(asker.guild, m.channel_id, m.id)
            )
        })
        .collect();
    Ok(lines.join("\n"))
}

async fn server_events(ctx: &BotCtx, asker: &Asker) -> Result<String> {
    let Some(guild) = asker.guild else {
        return Ok("Direct messages have no server events.".to_string());
    };
    let mut events = guild.scheduled_events(&ctx.http, true).await?;
    events.retain(|e| {
        matches!(
            e.status,
            ScheduledEventStatus::Scheduled | ScheduledEventStatus::Active
        )
    });
    if events.is_empty() {
        return Ok("The server has no upcoming events.".to_string());
    }
    events.sort_by_key(|e| e.start_time);
    let zone = home_zone(ctx, asker.user).await;
    let local = |timestamp: i64| {
        DateTime::from_timestamp(timestamp, 0)
            .unwrap_or_default()
            .with_timezone(&zone)
            .format("%A %Y-%m-%d %H:%M")
            .to_string()
    };
    let mut blocks = Vec::new();
    for event in &events {
        let start = event.start_time.unix_timestamp();
        let mut lines = vec![format!("{} (ID {})", event.name, event.id)];
        let mut when = format!("Starts: {} {} (<t:{start}:F>)", local(start), zone.name());
        if let Some(end) = event.end_time {
            when.push_str(&format!(", ends {}", local(end.unix_timestamp())));
        }
        if event.status == ScheduledEventStatus::Active {
            when.push_str(", happening now");
        }
        lines.push(when);
        if let Some(channel) = event.channel_id {
            lines.push(format!("Where: <#{channel}>"));
        } else if let Some(place) = event.metadata.as_ref().and_then(|m| m.location.clone()) {
            lines.push(format!("Where: {place}"));
        }
        if let Some(count) = event.user_count {
            lines.push(format!("Interested: {count}"));
        }
        if let Some(description) = event.description.as_deref().filter(|d| !d.is_empty()) {
            lines.push(format!("Description: {}", shorten(description, 300)));
        }
        blocks.push(lines.join("\n"));
    }
    Ok(blocks.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reactions_after_the_message() {
        assert_eq!(reaction_text(&[]), "");
        let list = [("😂".to_string(), 3), (":pog:".to_string(), 1)];
        assert_eq!(reaction_text(&list), " [reactions: 😂 3, :pog: 1]");
    }

    #[test]
    fn channels_by_category() {
        let entry = |id, name: &str, kind, topic: Option<&str>, parent, position| ChannelEntry {
            id,
            name: name.to_string(),
            kind,
            topic: topic.map(str::to_string),
            parent,
            position,
        };
        let entries = vec![
            entry(1, "Voice", ChannelType::Category, None, None, 2),
            entry(2, "Text", ChannelType::Category, None, None, 1),
            entry(3, "Empty", ChannelType::Category, None, None, 3),
            entry(4, "lounge", ChannelType::Voice, None, Some(1), 0),
            entry(
                5,
                "memes",
                ChannelType::Text,
                Some("only\nmemes"),
                Some(2),
                1,
            ),
            entry(6, "general", ChannelType::Text, None, Some(2), 0),
            entry(7, "rules", ChannelType::Text, Some("read me"), None, 0),
        ];
        assert_eq!(
            render_channels(&entries, 6),
            "#rules (text channel): read me
Category Text:
  #general (text channel) [you are here]
  #memes (text channel): only memes
Category Voice:
  #lounge (voice channel)"
        );
    }

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
        assert_eq!(parse_link("https://discord.com/channels/1/0/0"), None);
    }

    #[test]
    fn links_to_messages() {
        let link = message_link(Some(GuildId::new(1)), ChannelId::new(2), MessageId::new(3));
        assert_eq!(link, "https://discord.com/channels/1/2/3");
        assert_eq!(parse_link(&link), Some((Some(1), 2, 3)));
    }

    #[test]
    fn user_ids() {
        assert_eq!(parse_user_id("<@123>"), Some(UserId::new(123)));
        assert_eq!(parse_user_id("<@!123>"), Some(UserId::new(123)));
        assert_eq!(parse_user_id("123"), Some(UserId::new(123)));
        assert_eq!(parse_user_id("cherry"), None);
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
