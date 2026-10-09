//! Chiming in: now and then Vivy reads along in a channel and, if she has something to say,
//! adds one short line or a reaction, without anyone mentioning her.
//!
//! After each message there's a small chance (`chime_chance` in `[features.chat]`) that she
//! looks at the last [`READ_MESSAGES`] messages and decides: a line, an emoji, or nothing.
//! A channel then waits `chime_cooldown_minutes` before she considers it again. Her line is
//! saved like an answer, so replying to it continues the conversation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use async_trait::async_trait;
use chrono::Utc;
use chrono_tz::Tz;
use serde::Deserialize;
use serenity::all::{
    ChannelId, CreateAllowedMentions, CreateMessage, GetMessages, Message, ReactionType,
};
use tracing::{info, warn};

use super::answer::{self, SYSTEM_PROMPT};
use super::store::{self, NewTurn, StoredPart, Written};
use super::tools::format_message;
use crate::ai::{ChatRequest, Input, Part, Role, ToolCall, ToolRunner, Turn, complete};
use crate::core::{Asker, BotCtx, Feature, Result};
use crate::util::shorten;

/// How many messages she reads before deciding.
const READ_MESSAGES: u8 = 25;
/// Rounds of tool calls (saving to memory) before she has to decide.
const MAX_ROUNDS: usize = 4;
/// The longest line she posts.
const MAX_LINE: usize = 400;

/// `[features.chat]` settings for chiming in.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The chance, from 0 to 1, that a message makes her consider chiming in. 0 turns it off.
    pub chime_chance: f64,
    /// How long a channel waits after she considered it.
    pub chime_cooldown_minutes: u64,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            chime_chance: 0.03,
            chime_cooldown_minutes: 60,
        }
    }
}

/// What she does after reading along.
#[derive(Debug, PartialEq)]
enum Decision {
    Pass,
    React(String),
    Say(String),
}

#[derive(Default)]
pub struct Chime {
    settings: OnceLock<Settings>,
    /// When she last considered each channel.
    last: Mutex<HashMap<ChannelId, Instant>>,
}

impl Chime {
    fn settings(&self, ctx: &BotCtx) -> &Settings {
        self.settings.get_or_init(|| {
            ctx.config.feature("chat").unwrap_or_else(|err| {
                warn!("chime-ins use their defaults: {err:#}");
                Settings::default()
            })
        })
    }

    /// Rolls the dice for a message, and reads along if it hits.
    pub async fn on_message(&self, ctx: &BotCtx, msg: &Message) -> Result<()> {
        let (Some(guild), Some(provider)) = (msg.guild_id, ctx.ai.chat.clone()) else {
            return Ok(());
        };
        // Messages to her get a real answer instead.
        let to_her = msg.mentions_user_id(ctx.bot_id)
            || msg
                .referenced_message
                .as_ref()
                .is_some_and(|r| r.author.id == ctx.bot_id);
        if to_her || !self.roll(ctx, msg.channel_id) {
            return Ok(());
        }

        let asker = Asker {
            user: msg.author.id,
            guild: Some(guild),
            channel: msg.channel_id,
            message: msg.id,
        };
        let transcript = read_along(ctx, msg).await?;
        let mut parts = vec![Part::Text(format!("{transcript}\n{INSTRUCTIONS}"))];
        parts.extend(
            answer::context_for(ctx, &asker, true)
                .await
                .into_iter()
                .map(Part::Text),
        );
        let (tools, owners) = answer::tools_for(ctx, &asker);
        let request = ChatRequest {
            system: SYSTEM_PROMPT.to_string(),
            input: Input::Full(vec![Turn {
                role: Role::User,
                parts,
            }]),
            tools,
            cache_key: format!("discord:{}", msg.channel_id),
        };
        let runner = Runner {
            ctx: ctx.clone(),
            asker,
            owners,
        };
        let done = complete(provider.as_ref(), request, &runner, MAX_ROUNDS).await?;

        match decide(&done.text) {
            Decision::Pass => info!("read along and passed"),
            Decision::React(emoji) => {
                let reaction = ReactionType::try_from(emoji.as_str())
                    .with_context(|| format!("{emoji:?} isn't an emoji"))?;
                msg.react(&ctx.http, reaction).await?;
                info!("chimed in with {emoji}");
            }
            Decision::Say(line) => {
                let sent = msg
                    .channel_id
                    .send_message(
                        &ctx.http,
                        CreateMessage::new()
                            .content(&line)
                            .allowed_mentions(CreateAllowedMentions::new()),
                    )
                    .await?;
                info!("chimed in");
                // Saved like a question and its answer, so a reply to her line continues.
                let question = NewTurn {
                    parent_id: None,
                    role: Role::User,
                    author_id: msg.author.id.get(),
                    channel_id: msg.channel_id.get(),
                    parts: vec![StoredPart::Text { text: transcript }],
                    written: None,
                    created_at: Utc::now().timestamp(),
                };
                let written = Written {
                    provider: provider.name().to_string(),
                    model: provider.model().to_string(),
                    continuation_id: done.continuation,
                    native_json: Some(serde_json::Value::Array(done.natives).to_string()),
                };
                let (sent_id, bot_id) = (sent.id.get(), ctx.bot_id.get());
                ctx.db
                    .call(move |conn| {
                        let question_id = store::add_turn(conn, &question, &[])?;
                        let answer = NewTurn {
                            parent_id: Some(question_id),
                            role: Role::Assistant,
                            author_id: bot_id,
                            channel_id: question.channel_id,
                            parts: vec![StoredPart::Text { text: line }],
                            written: Some(written),
                            created_at: Utc::now().timestamp(),
                        };
                        store::add_turn(conn, &answer, &[sent_id])
                    })
                    .await
                    .context("saving the chime-in")?;
            }
        }
        Ok(())
    }

    /// Whether to read along now: the dice hit and the channel isn't cooling down. A hit
    /// starts the cooldown right away, so two quick messages can't both start one.
    fn roll(&self, ctx: &BotCtx, channel: ChannelId) -> bool {
        let settings = self.settings(ctx);
        if fastrand::f64() >= settings.chime_chance {
            return false;
        }
        let cooldown = Duration::from_secs(settings.chime_cooldown_minutes * 60);
        let mut last = self.last.lock().unwrap();
        if last.get(&channel).is_some_and(|at| at.elapsed() < cooldown) {
            return false;
        }
        last.insert(channel, Instant::now());
        true
    }
}

/// What she's asked after reading along. In the question, not the system prompt, so the
/// system prompt stays the same as for answers and shares their cache.
const INSTRUCTIONS: &str = "You're reading along in this channel; nobody asked you anything. \
Chime in only when you have something that fits: a joke, a reaction, a fact, an opinion. Most of the time, pass. \
Reactions after a message show how people took it, your own lines included. \
If you noticed something lasting about the server, its people or yourself, you can save it with the memory tool first. \
Then answer with exactly one of: PASS; REACT followed by one emoji; or one short line the way people write on Discord, with no greeting.";

/// The last [`READ_MESSAGES`] messages of the channel, oldest first, ending with `msg`.
async fn read_along(ctx: &BotCtx, msg: &Message) -> Result<String> {
    let mut messages = msg
        .channel_id
        .messages(
            &ctx.http,
            GetMessages::new().before(msg.id).limit(READ_MESSAGES - 1),
        )
        .await?;
    messages.reverse();
    messages.push(msg.clone());
    let channel = msg
        .guild_id
        .and_then(|guild| ctx.cache.guild(guild))
        .and_then(|guild| {
            guild
                .channels
                .get(&msg.channel_id)
                .or_else(|| guild.threads.iter().find(|t| t.id == msg.channel_id))
                .map(|c| c.name.clone())
        })
        .unwrap_or_default();
    let lines: Vec<String> = messages
        .iter()
        .map(|m| format_message(ctx, m, Tz::UTC))
        .collect();
    Ok(format!(
        "<recent_messages channel=\"#{channel}\">\n{}\n</recent_messages>",
        lines.join("\n")
    ))
}

/// Reads her answer: PASS, REACT 😂, or the line to post.
fn decide(text: &str) -> Decision {
    let text = text.trim().trim_matches('"').trim();
    let word = text
        .trim_end_matches(|c: char| c.is_ascii_punctuation())
        .to_ascii_uppercase();
    let starts_with_pass = text
        .strip_prefix("PASS")
        .is_some_and(|rest| !rest.starts_with(|c: char| c.is_alphanumeric()));
    if text.is_empty() || word == "PASS" || starts_with_pass {
        return Decision::Pass;
    }
    if let Some(rest) = text
        .strip_prefix("REACT")
        .or_else(|| text.strip_prefix("React"))
    {
        let emoji = rest.trim_start_matches(':').trim();
        return match emoji.split_whitespace().next() {
            Some(emoji) => Decision::React(emoji.to_string()),
            None => Decision::Pass,
        };
    }
    Decision::Say(shorten(text, MAX_LINE))
}

/// Runs her tool calls with chat's tools, as the author of the last message.
struct Runner {
    ctx: BotCtx,
    asker: Asker,
    owners: HashMap<&'static str, Arc<dyn Feature>>,
}

#[async_trait]
impl ToolRunner for Runner {
    async fn run(&self, call: &ToolCall) -> String {
        answer::run_tool(&self.ctx, &self.asker, &self.owners, call).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_her_decision() {
        assert_eq!(decide("PASS"), Decision::Pass);
        assert_eq!(decide(" pass. "), Decision::Pass);
        assert_eq!(decide(""), Decision::Pass);
        assert_eq!(decide("PASS - nothing to add"), Decision::Pass);
        assert_eq!(
            decide("PASSWORD reset day"),
            Decision::Say("PASSWORD reset day".into())
        );
        assert_eq!(decide("REACT 😂"), Decision::React("😂".into()));
        assert_eq!(decide("REACT: 🍿"), Decision::React("🍿".into()));
        assert_eq!(decide("REACT"), Decision::Pass);
        assert_eq!(
            decide("\"that movie was mid tbh\""),
            Decision::Say("that movie was mid tbh".into())
        );
        let Decision::Say(long) = decide(&"a".repeat(1000)) else {
            panic!("not a line");
        };
        assert!(long.chars().count() <= MAX_LINE);
    }

    #[test]
    fn settings_default_to_a_small_chance() {
        let settings: Settings = toml::from_str("enabled = true").unwrap();
        assert_eq!(settings.chime_chance, 0.03);
        assert_eq!(settings.chime_cooldown_minutes, 60);
        let off: Settings = toml::from_str("chime_chance = 0.0").unwrap();
        assert_eq!(off.chime_chance, 0.0);
    }
}
