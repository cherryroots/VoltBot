//! Chiming in: now and then Vivy reads along in a channel and, if she has something to say,
//! adds one short line or a reaction, without anyone mentioning her.
//!
//! After each message there's a small chance (`chime_chance` in `[features.chat]`) that she
//! looks at the last [`READ_MESSAGES`] messages and decides: a line, an emoji, or nothing.
//! A channel then waits `chime_cooldown_minutes` before she considers it again. Her line is
//! saved like an answer, so replying to it continues the conversation.
//!
//! Nobody asked for these lines (nor for check-ins, which use [`speak`] too), and they're
//! posted for the whole channel, so she only gets [`UNPROMPTED_TOOLS`]: tools that read
//! what people in this channel could see anyway, and her own memory.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use async_trait::async_trait;
use chrono::Utc;
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::json;
use serenity::all::{
    ChannelId, CreateAllowedMentions, CreateMessage, GetMessages, GuildId, Message, ReactionType,
    UserId,
};
use tracing::{info, warn};

use super::answer::{self, SYSTEM_PROMPT};
use super::store::{self, NewTurn, StoredPart, Written};
use super::tools::{format_message, parse_link};
use crate::ai::{ChatRequest, Input, Part, Role, ToolCall, ToolRunner, Turn, complete};
use crate::core::{Asker, BotCtx, Feature, Result};
use crate::util::shorten;

/// How many messages she reads before deciding.
const READ_MESSAGES: u8 = 25;
/// Rounds of tool calls (saving to memory) before she has to decide.
const MAX_ROUNDS: usize = 4;
/// The longest line she posts.
const MAX_LINE: usize = 400;

/// The tools she may use when nobody asked. An allow-list, so a new tool stays out until
/// someone decides it's safe here. Left out: anything that acts for a person (reminders,
/// check-ins), and `list_channels`, which lists what one person can see, not everyone here.
/// `get_message` and `search_messages` only reach this channel (see [`Runner`]). `memory`
/// is in: what she saves is her own decision.
const UNPROMPTED_TOOLS: &[&str] = &[
    "get_current_time",
    "get_channel_info",
    "get_user_info",
    "read_recent_messages",
    "get_message",
    "search_messages",
    "get_pinned_messages",
    "list_server_events",
    "list_server_emoji",
    "get_wheel_status",
    "memory",
];

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
    pub(super) fn settings(&self, ctx: &BotCtx) -> &Settings {
        self.settings.get_or_init(|| {
            ctx.config.feature("chat").unwrap_or_else(|err| {
                warn!("chime-ins use their defaults: {err:#}");
                Settings::default()
            })
        })
    }

    /// Rolls the dice for a message, and reads along if it hits.
    pub async fn on_message(&self, ctx: &BotCtx, msg: &Message) -> Result<()> {
        let Some(guild) = msg.guild_id.filter(|_| ctx.ai.chat().is_some()) else {
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
        speak(ctx, &asker, Some(msg), INSTRUCTIONS, None).await
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
Chime in only when you have something that fits: a joke, a reaction, a fact, an opinion, or a short question about what they're talking about when you don't know it and are curious (what a name or in-joke means, how something turned out). Most of the time, pass. \
When someone answers one of your questions later, save what you learned. \
Reactions after a message show how people took it, your own lines included. \
If you noticed something lasting about the server, its people or yourself, you can save it with the memory tool first. \
Then answer with exactly one of: PASS; REACT followed by one emoji; or one short line the way people write on Discord, with no greeting.";

/// Reads the last messages of `asker.channel` (ending with `last`, when given), asks the
/// model what to do with `instructions`, and does it: posts a line, reacts to `last`, or
/// nothing. A posted line is saved like an answer, so a reply to it continues the
/// conversation. `mention` is the one person the line may ping.
pub async fn speak(
    ctx: &BotCtx,
    asker: &Asker,
    last: Option<&Message>,
    instructions: &str,
    mention: Option<UserId>,
) -> Result<()> {
    let provider = ctx.ai.chat().context("chat has no model")?;
    let transcript = read_along(ctx, asker.guild, asker.channel, last).await?;
    let question = format!("{transcript}\n{instructions}");
    let context = answer::context_for(ctx, asker, true).await;
    let mut parts = vec![Part::Text(question.clone())];
    parts.extend(context.iter().cloned().map(Part::Text));
    let (mut tools, mut owners) = answer::tools_for(ctx, asker);
    tools.retain(|tool| UNPROMPTED_TOOLS.contains(&tool.name));
    owners.retain(|name, _| UNPROMPTED_TOOLS.contains(name));
    let request = ChatRequest {
        system: SYSTEM_PROMPT.to_string(),
        input: Input::Full(vec![Turn {
            role: Role::User,
            parts,
        }]),
        tools,
        cache_key: format!("discord:{}", asker.channel),
        job: "chime",
    };
    let runner = Runner {
        ctx: ctx.clone(),
        asker: asker.clone(),
        owners,
    };
    let done = complete(provider.as_ref(), request, &runner, MAX_ROUNDS).await?;

    let line = match (decide(&done.text), last) {
        (Decision::Say(line), _) => line,
        (Decision::React(emoji), Some(msg)) => {
            let reaction = ReactionType::try_from(emoji.as_str())
                .with_context(|| format!("{emoji:?} isn't an emoji"))?;
            msg.react(&ctx.http, reaction).await?;
            info!("reacted with {emoji}");
            return Ok(());
        }
        _ => {
            info!("read along and passed");
            return Ok(());
        }
    };
    let pings = match mention {
        Some(user) => CreateAllowedMentions::new().users([user]),
        None => CreateAllowedMentions::new(),
    };
    let sent = asker
        .channel
        .send_message(
            &ctx.http,
            CreateMessage::new().content(&line).allowed_mentions(pings),
        )
        .await?;
    info!("spoke up on her own");
    // Saved like a question and its answer, exactly as the model read it, so a reply to
    // her line continues the same conversation.
    let mut stored = vec![StoredPart::Text { text: question }];
    stored.extend(context.into_iter().map(|text| StoredPart::Context { text }));
    let question = NewTurn {
        parent_id: None,
        role: Role::User,
        // Saved as the bot's own, because nobody asked: nobody can 🔁 or ❌ her line as
        // if it were their answer (🔁 would run it as a normal answer and post "PASS").
        author_id: ctx.bot_id.get(),
        channel_id: asker.channel.get(),
        parts: stored,
        written: None,
        created_at: Utc::now().timestamp(),
    };
    let written = Written {
        provider: provider.name().to_string(),
        model: provider.model().to_string(),
        continuation_id: done.continuation,
        native_json: serde_json::to_string(&done.rounds).ok(),
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
        .context("saving what she said")?;
    Ok(())
}

/// The last [`READ_MESSAGES`] messages of the channel, oldest first, ending with `last`
/// when given (otherwise with the newest).
async fn read_along(
    ctx: &BotCtx,
    guild: Option<GuildId>,
    channel: ChannelId,
    last: Option<&Message>,
) -> Result<String> {
    let mut messages = match last {
        Some(msg) => {
            let mut before = channel
                .messages(
                    &ctx.http,
                    GetMessages::new().before(msg.id).limit(READ_MESSAGES - 1),
                )
                .await?;
            before.insert(0, msg.clone());
            before
        }
        None => {
            channel
                .messages(&ctx.http, GetMessages::new().limit(READ_MESSAGES))
                .await?
        }
    };
    // Discord gives the newest first.
    messages.reverse();
    let name = guild
        .and_then(|guild| ctx.cache.guild(guild))
        .and_then(|guild| {
            guild
                .channels
                .get(&channel)
                .or_else(|| guild.threads.iter().find(|t| t.id == channel))
                .map(|c| c.name.clone())
        })
        .unwrap_or_default();
    let lines: Vec<String> = messages
        .iter()
        .map(|m| format_message(ctx, m, Tz::UTC))
        .collect();
    Ok(format!(
        "<recent_messages channel=\"#{name}\">\n{}\n</recent_messages>",
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

/// Runs her tool calls with [`UNPROMPTED_TOOLS`], as the author of the last message (or the
/// person she checks in with), but kept to this channel: what she reads ends up in a public
/// line, so it mustn't come from a channel only that person can read.
///
/// In a server, `memory` runs as Vivy herself: what she saves unprompted is her own call, so
/// the change log names her, not whoever wrote last. The folder is the server's either way.
/// In a DM (a follow-up) the folder is the person's, so it stays theirs.
struct Runner {
    ctx: BotCtx,
    asker: Asker,
    owners: HashMap<&'static str, Arc<dyn Feature>>,
}

#[async_trait]
impl ToolRunner for Runner {
    async fn run(&self, call: &ToolCall) -> String {
        match here_only(call, self.asker.channel) {
            Ok(call) if call.name == "memory" && self.asker.guild.is_some() => {
                let herself = Asker {
                    user: self.ctx.bot_id,
                    ..self.asker.clone()
                };
                answer::run_tool(&self.ctx, &herself, &self.owners, &call).await
            }
            Ok(call) => answer::run_tool(&self.ctx, &self.asker, &self.owners, &call).await,
            Err(text) => text,
        }
    }
}

/// Keeps a tool call to `channel`: a search only searches it, and a message link must point
/// into it. Returns the call to run, or the error to give the model instead.
fn here_only(call: &ToolCall, channel: ChannelId) -> Result<ToolCall, String> {
    let mut call = call.clone();
    match call.name.as_str() {
        "search_messages" => match call.args.as_object_mut() {
            Some(args) => {
                args.insert("channel".to_string(), json!("here"));
            }
            None => call.args = json!({"channel": "here"}),
        },
        "get_message" => {
            let link = call.args["link"].as_str().unwrap_or_default();
            // A link that doesn't parse is left to the tool, which explains the mistake.
            if let Some((_, linked, _)) = parse_link(link)
                && linked != channel.get()
            {
                return Err("Error: right now you can only read messages in this channel.".into());
            }
        }
        _ => {}
    }
    Ok(call)
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
    fn tools_stay_in_this_channel() {
        let here = ChannelId::new(10);
        let call = |name: &str, args| ToolCall {
            id: "1".into(),
            name: name.into(),
            args,
        };
        let search = here_only(&call("search_messages", json!({"channel": "secret"})), here);
        assert_eq!(search.unwrap().args["channel"], "here");
        let search = here_only(&call("search_messages", json!(null)), here);
        assert_eq!(search.unwrap().args["channel"], "here");

        let link = |channel| json!({"link": format!("https://discord.com/channels/1/{channel}/5")});
        assert!(here_only(&call("get_message", link(10)), here).is_ok());
        assert!(here_only(&call("get_message", link(11)), here).is_err());
        assert!(here_only(&call("get_message", json!({"link": "nonsense"})), here).is_ok());
    }

    #[test]
    fn no_tools_that_act_for_someone() {
        for name in [
            "create_reminder",
            "cancel_reminder",
            "list_reminders",
            "schedule_follow_up",
        ] {
            assert!(!UNPROMPTED_TOOLS.contains(&name), "{name}");
        }
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
