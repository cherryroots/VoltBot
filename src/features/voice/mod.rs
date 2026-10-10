//! Voice: Vivy sends Discord voice messages in her own voice, made with ElevenLabs.
//!
//! Chat gets one tool, `send_voice_message`: she writes a short script with audio tags
//! (`[whispers]`, `[laughs]`), ElevenLabs speaks it in the voice `voice_id`, and the bot
//! posts it as a real voice message (the kind with a play button and bars) replying to the
//! asker. Only her script is sent to ElevenLabs.
//!
//! - `speech.rs`: the ElevenLabs request
//! - `audio.rs`: OGG Opus, length and waveform with ffmpeg
//! - `store.rs`: what was sent, for the monthly character limit
//!
//! The key goes in .env as `ELEVENLABS_API_KEY`. Without it the tool isn't offered.

mod audio;
mod speech;
mod store;

use anyhow::Context as _;
use async_trait::async_trait;
use base64::Engine as _;
use base64::prelude::BASE64_STANDARD;
use chrono::{DateTime, Datelike, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use serenity::all::CreateAttachment;
use tracing::info;

use crate::ai::ToolDef;
use crate::core::{Asker, BotCtx, Feature, Result, Stat, user_error};

const KEY_VAR: &str = "ELEVENLABS_API_KEY";
const TOOL: &str = "send_voice_message";
/// Discord's flag for a voice message.
const IS_VOICE_MESSAGE: u64 = 1 << 13;

/// `[features.voice]` settings.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The ElevenLabs voice she speaks with.
    pub voice_id: String,
    /// "eleven_v4", or "eleven_v4_turbo" for faster and half the price.
    pub model: String,
    /// 0 to 1: lower is more expressive and varied, higher is steadier.
    pub stability: f64,
    /// 0 to 1: how closely it sticks to the voice.
    pub similarity: f64,
    /// A language code like "en" to force a language. Left out, it follows the text.
    pub language: Option<String>,
    /// The most characters a month, all voice messages together. 0 for no limit.
    pub monthly_characters: u64,
    /// The most characters in one voice message.
    pub max_characters: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            voice_id: String::new(),
            model: "eleven_v4".to_string(),
            stability: 0.5,
            similarity: 0.75,
            language: None,
            monthly_characters: 50_000,
            max_characters: 1_000,
        }
    }
}

/// The ElevenLabs key from .env, if it's set.
fn key() -> Option<String> {
    std::env::var(KEY_VAR)
        .ok()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
}

/// The start of this month in UTC, as a Unix timestamp.
fn month_start(now: DateTime<Utc>) -> i64 {
    now.date_naive()
        .with_day(1)
        .and_then(|day| day.and_hms_opt(0, 0, 0))
        .map(|start| start.and_utc().timestamp())
        .unwrap_or_else(|| now.timestamp())
}

pub struct Voice;

#[async_trait]
impl Feature for Voice {
    fn name(&self) -> &'static str {
        "voice"
    }

    fn migrations(&self) -> &'static [&'static str] {
        store::MIGRATIONS
    }

    fn tools(&self) -> Vec<ToolDef> {
        if key().is_none() {
            return Vec::new();
        }
        vec![ToolDef {
            name: TOOL,
            description: "Send a Discord voice message in your own voice, as a reply to the asker. Use it when someone asks to hear you, and now and then when saying something fits better than typing it: a greeting, a laugh, a sleepy good night, a reaction. Keep it short, one to four sentences. \
Write it as spoken words: no markdown, links, emoji or lists. Steer how you say it with audio tags in square brackets before the words they're for, in plain English: [whispers], [excited], [sighs], [laughs], [yawns], [sarcastic], [sleepy, slow]. A tag lasts until the next one; put several in one bracket with commas; don't mix opposite ones like [whispers, shouting]. Let your current mood show in how you sound. \
The voice message is your whole answer, so usually write no text with it.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "script": {"type": "string", "description": "What you say, with audio tags."}
                },
                "required": ["script"]
            }),
        }]
    }

    async fn run_tool(
        &self,
        ctx: &BotCtx,
        asker: &Asker,
        _name: &str,
        args: &Value,
    ) -> Result<String> {
        let key = key().ok_or_else(|| user_error("voice messages aren't set up"))?;
        let settings: Settings = ctx.config.feature("voice")?;
        if settings.voice_id.trim().is_empty() {
            return Err(user_error(
                "voice messages aren't set up (no voice_id in config.toml)",
            ));
        }
        let script = args
            .get("script")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| user_error("give the script to say"))?
            .to_string();
        let characters = script.chars().count();
        if characters > settings.max_characters {
            return Err(user_error(format!(
                "that's {characters} characters; a voice message can have at most {}. Say less.",
                settings.max_characters
            )));
        }
        let since = month_start(Utc::now());
        let (_, used) = ctx
            .db
            .call(move |conn| Ok(store::used_since(conn, since)?))
            .await?;
        let limit = settings.monthly_characters;
        if limit > 0 && used + characters as u64 > limit {
            return Err(user_error(format!(
                "your voice is used up for this month ({used} of {limit} characters). Answer in text."
            )));
        }

        let audio = speech::speak(&ctx.web, &key, &settings, &script).await?;
        let clip = audio::prepare(&audio).await?;
        let payload = json!({
            "flags": IS_VOICE_MESSAGE,
            "attachments": [{
                "id": 0,
                "filename": "voice-message.ogg",
                "duration_secs": clip.seconds,
                "waveform": BASE64_STANDARD.encode(&clip.waveform),
            }],
            "message_reference": {
                "message_id": asker.message.get(),
                "channel_id": asker.channel.get(),
                "fail_if_not_exists": false,
            },
            "allowed_mentions": {"parse": [], "replied_user": true},
        });
        let file = CreateAttachment::bytes(clip.ogg, "voice-message.ogg");
        let message = ctx
            .http
            .send_message(asker.channel, vec![file], &payload)
            .await
            .context("sending the voice message")?;
        info!(channel = %asker.channel, seconds = clip.seconds, characters, "sent a voice message");
        asker
            .posted
            .add(message.id, format!("[Voice message] {script}"));

        let sent = store::Sent {
            message_id: message.id.get(),
            guild_id: asker.guild.map(|g| g.get()),
            channel_id: asker.channel.get(),
            user_id: asker.user.get(),
            characters,
            seconds: clip.seconds,
            at: Utc::now().timestamp(),
        };
        ctx.db
            .call(move |conn| Ok(store::record(conn, &sent)?))
            .await?;
        Ok(format!(
            "Your voice message is sent ({:.1} seconds). It is your answer: write no text after it, unless something can only be shown in writing (a link, code).",
            clip.seconds
        ))
    }

    async fn stats(&self, ctx: &BotCtx) -> Result<Vec<Stat>> {
        if key().is_none() {
            return Ok(Vec::new());
        }
        let settings: Settings = ctx.config.feature("voice")?;
        let since = month_start(Utc::now());
        let (sent, used) = ctx
            .db
            .call(move |conn| Ok(store::used_since(conn, since)?))
            .await?;
        let characters = match settings.monthly_characters {
            0 => used.to_string(),
            limit => format!("{used} / {limit}"),
        };
        Ok(vec![
            Stat::new("Sent this month", sent),
            Stat::new("Characters", characters),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_starts_on_the_first() {
        let now = DateTime::parse_from_rfc3339("2026-10-10T21:40:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let start = DateTime::parse_from_rfc3339("2026-10-01T00:00:00Z")
            .unwrap()
            .timestamp();
        assert_eq!(month_start(now), start);
    }

    #[test]
    fn settings_have_defaults() {
        let settings = Settings::default();
        assert_eq!(settings.model, "eleven_v4");
        assert!(settings.voice_id.is_empty());
    }
}
