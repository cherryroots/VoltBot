//! Her name: with `nickname` under `[features.memory]`, her nickname in each server is that
//! name plus the emoji on the `emoji:` line of her mood there, like "Vivy ☕". She picks the
//! emoji with her mood (reflection, mood checks, `set_mood`), so it is hers like the rest
//! of her mood file. Without `nickname` her name is left alone.

use chrono::Utc;
use serde::Deserialize;
use serenity::all::GuildId;
use tracing::info;

use super::{face, reflect, store};
use crate::core::{BotCtx, Result};

/// Discord's limit for a nickname, in characters.
const MAX_NICK: usize = 32;
/// The longest emoji accepted, in characters: enough for a flag or a joined emoji like 🧑‍🍳.
const MAX_EMOJI: usize = 8;
/// Its name in `memory_pictures`, which keeps the nickname each server shows.
const SLOT: &str = "nickname";

/// `[features.memory]` settings for her name.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct Settings {
    /// Her name before the emoji, like "Vivy". Leave out to keep her name as it is.
    nickname: Option<String>,
}

/// Her name before the emoji, if mood nicknames are on.
fn base(ctx: &BotCtx) -> Option<String> {
    let settings: Settings = ctx.config.feature_part("memory").ok()?;
    settings
        .nickname
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// Whether her nickname follows her mood.
pub fn enabled(ctx: &BotCtx) -> bool {
    base(ctx).is_some()
}

/// The line added to the mood instructions when her nickname follows her mood.
pub const INSTRUCTION: &str = "Add an `emoji:` line with one emoji that fits your mood or what you're doing; it's shown after your name in this server.";

/// `emoji` if it is one short emoji (or a few joined ones), without letters, digits or
/// spaces, so the line can't put words in her name.
pub fn clean(emoji: &str) -> Option<String> {
    let emoji = emoji.trim().trim_matches(['"', '`']);
    let count = emoji.chars().count();
    let ok = (1..=MAX_EMOJI).contains(&count)
        && !emoji
            .chars()
            .any(|c| c.is_alphanumeric() || c.is_whitespace() || c.is_ascii());
    ok.then(|| emoji.to_string())
}

/// The `emoji:` line of her mood file, if it holds an emoji.
pub fn parse(mood: &str) -> Option<String> {
    clean(&reflect::mood_line(mood, "emoji")?)
}

/// Her nickname: `base`, then the emoji. A long name is cut so the emoji still fits.
fn nick(base: &str, emoji: Option<&str>) -> String {
    let Some(emoji) = emoji else {
        return base.chars().take(MAX_NICK).collect();
    };
    let room = MAX_NICK.saturating_sub(emoji.chars().count() + 1);
    let base: String = base.chars().take(room).collect();
    format!("{} {emoji}", base.trim_end())
}

/// Sets her nickname in `guild` from `mood`. Like the faces, an unchanged nickname is never
/// sent again, and changes are at least 10 minutes apart: the result is then how many
/// seconds are left to wait.
pub async fn update(ctx: &BotCtx, guild: GuildId, mood: &str) -> Result<Option<i64>> {
    let Some(base) = base(ctx) else {
        return Ok(None);
    };
    let name = nick(&base, parse(mood).as_deref());
    let id = guild.get();
    let current = ctx
        .db
        .call(move |conn| Ok(store::picture(conn, id, SLOT)?))
        .await?;
    if let Some((current, at)) = current {
        if current == name {
            return Ok(None);
        }
        let since = Utc::now().timestamp() - at;
        if since < face::MIN_GAP_SECS {
            info!(%guild, name, "nickname changed less than 10 minutes ago, changing it later");
            return Ok(Some(face::MIN_GAP_SECS - since));
        }
    }

    let mut body = serde_json::Map::new();
    body.insert("nick".into(), name.clone().into());
    ctx.http.edit_member_me(guild, &body, None).await?;
    info!(%guild, name, "changed her nickname");
    let now = Utc::now().timestamp();
    ctx.db
        .call(move |conn| Ok(store::set_picture(conn, id, SLOT, &name, "", now)?))
        .await?;
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_emoji() {
        assert_eq!(parse("mood: cozy\nemoji: ☕"), Some("☕".into()));
        assert_eq!(parse("- **Emoji**: \"🧑‍🍳\""), Some("🧑‍🍳".into()));
        assert_eq!(parse("emoji: 🇸🇪"), Some("🇸🇪".into()));
        // Words, spaces or nothing don't go in her name.
        assert_eq!(parse("emoji: happy"), None);
        assert_eq!(parse("emoji: ☕ hi"), None);
        assert_eq!(parse("emoji: :)"), None);
        assert_eq!(parse("emoji:"), None);
        assert_eq!(parse("mood: cozy"), None);
    }

    #[test]
    fn builds_the_nickname() {
        assert_eq!(nick("Vivy", Some("☕")), "Vivy ☕");
        assert_eq!(nick("Vivy", None), "Vivy");
        let long = nick(&"a".repeat(40), Some("☕"));
        assert_eq!(long.chars().count(), MAX_NICK);
        assert!(long.ends_with(" ☕"));
    }
}
