//! Chat: "@Vivy what's in this picture?" gets an answer from the AI model, streamed into a
//! reply. Replying to an answer continues the conversation.
//!
//! - `answer.rs`: one answer: streaming, the status line, tool calls, files
//! - `history.rs`: Discord messages to chat turns, and stored turns to model input
//! - `store.rs`: the `chat_turns` and `chat_messages` tables
//! - `tools.rs`: chat's own tools (time, channel, users, messages, pins, events)
//! - `search.rs`: the `search_messages` tool
//! - `chime.rs`: chiming in now and then without being mentioned
//! - `prompt.md`: the system prompt
//!
//! Reactions on an answer, from the person who asked: ❌ stops it while it's being written
//! and deletes it once it's done, 🔁 writes it again.

mod answer;
mod chime;
mod history;
mod search;
mod store;
mod tools;

use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;
use serenity::all::{Message, MessageId, Reaction, ReactionType, UserId};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use self::answer::{End, Job};
use self::store::{NewTurn, StoredPart};
use crate::ai::{ChatProvider, Input, Part, Role, ToolDef};
use crate::core::{Asker, BotCtx, Feature, Result, user_error};
use crate::util::reply::LiveReply;

const STOP: &str = "❌";
const REGENERATE: &str = "🔁";

#[derive(Default)]
pub struct Chat {
    /// Answers being written right now, so ❌ can stop them.
    running: Mutex<Vec<Running>>,
    chime: chime::Chime,
}

struct Running {
    requester: UserId,
    cancel: CancellationToken,
    /// The answer's Discord messages, filled in as they're sent.
    message_ids: Arc<Mutex<Vec<MessageId>>>,
}

#[async_trait]
impl Feature for Chat {
    fn name(&self) -> &'static str {
        "chat"
    }

    fn migrations(&self) -> &'static [&'static str] {
        store::MIGRATIONS
    }

    /// The empty prefix: every mention no other feature claimed.
    fn mention_prefixes(&self) -> &'static [&'static str] {
        &[""]
    }

    fn tools(&self) -> Vec<ToolDef> {
        tools::defs()
    }

    async fn run_tool(
        &self,
        ctx: &BotCtx,
        asker: &Asker,
        name: &str,
        args: &Value,
    ) -> Result<String> {
        tools::run(ctx, asker, name, args).await
    }

    async fn on_mention(&self, ctx: &BotCtx, msg: &Message, rest: &str) -> Result<()> {
        let provider = provider(ctx)?;
        let msg = history::with_previews(ctx, msg).await;

        // The message this one replies to, if any, is the conversation so far.
        let parent_id = match &msg.referenced_message {
            Some(referenced) => Some(history::turn_for_reference(ctx, referenced).await?),
            None => None,
        };
        let question = NewTurn {
            parent_id,
            role: Role::User,
            author_id: msg.author.id.get(),
            channel_id: msg.channel_id.get(),
            parts: history::read_message(ctx, &msg, rest, false).await,
            written: None,
            created_at: Utc::now().timestamp(),
        };
        let message_id = msg.id.get();
        let question_id = ctx
            .db
            .call(move |conn| store::add_turn(conn, &question, &[message_id]))
            .await
            .context("saving the question")?;

        let asker = Asker {
            user: msg.author.id,
            guild: msg.guild_id,
            channel: msg.channel_id,
            message: msg.id,
        };
        let reply = LiveReply::new(msg.channel_id, Some(msg.id));
        self.answer(ctx, provider.as_ref(), asker, question_id, reply)
            .await
    }

    /// Every message might make her chime in.
    async fn on_message(&self, ctx: &BotCtx, msg: &Message) -> Result<()> {
        self.chime.on_message(ctx, msg).await
    }

    async fn on_reaction_add(&self, ctx: &BotCtx, reaction: &Reaction) -> Result<()> {
        let ReactionType::Unicode(emoji) = &reaction.emoji else {
            return Ok(());
        };
        let Some(user) = reaction.user_id else {
            return Ok(());
        };
        // Discord says whose message it is; skip the lookups for everyone else's.
        if reaction
            .message_author_id
            .is_some_and(|author| author != ctx.bot_id)
        {
            return Ok(());
        }
        match emoji.as_str() {
            STOP => {
                if self.is_running(reaction.message_id) {
                    self.stop(reaction.message_id, user);
                    Ok(())
                } else {
                    self.delete(ctx, reaction, user).await
                }
            }
            REGENERATE => self.regenerate(ctx, reaction, user).await,
            _ => Ok(()),
        }
    }
}

fn provider(ctx: &BotCtx) -> Result<Arc<dyn ChatProvider>> {
    ctx.ai
        .chat
        .clone()
        .ok_or_else(|| user_error("Chat is turned off: the bot has no OpenAI key."))
}

impl Chat {
    /// Answers the question turn `question_id` into `reply`, and saves the answer.
    async fn answer(
        &self,
        ctx: &BotCtx,
        provider: &dyn ChatProvider,
        asker: Asker,
        question_id: i64,
        reply: LiveReply,
    ) -> Result<()> {
        let chain = ctx
            .db
            .call(move |conn| store::chain(conn, question_id, history::MAX_TURNS))
            .await?;
        let mut input = history::build_input(ctx, provider, &chain).await;
        let fresh = matches!(input, Input::Full(_));
        add_context(&mut input, answer::context_for(ctx, &asker, fresh).await);

        let cancel = CancellationToken::new();
        let message_ids = Arc::new(Mutex::new(Vec::new()));
        self.running.lock().unwrap().push(Running {
            requester: asker.user,
            cancel: cancel.clone(),
            message_ids: message_ids.clone(),
        });
        let job = Job {
            asker: asker.clone(),
            reply,
            input,
            cancel,
            message_ids: message_ids.clone(),
        };
        let outcome = answer::run(ctx, provider, job).await;
        self.running
            .lock()
            .unwrap()
            .retain(|r| !Arc::ptr_eq(&r.message_ids, &message_ids));

        match &outcome.end {
            End::Finished => info!("answered"),
            End::Stopped => info!("answer stopped with ❌"),
            // The error is already shown in the reply's status line, so only log it.
            End::Failed(err) => error!("answering failed: {err:#}"),
        }
        if outcome.message_ids.is_empty() {
            return Ok(());
        }
        let answer_turn = NewTurn {
            parent_id: Some(question_id),
            role: Role::Assistant,
            author_id: ctx.bot_id.get(),
            channel_id: asker.channel.get(),
            parts: vec![StoredPart::Text { text: outcome.text }],
            written: outcome.written,
            created_at: Utc::now().timestamp(),
        };
        let ids: Vec<u64> = outcome.message_ids.iter().map(|id| id.get()).collect();
        ctx.db
            .call(move |conn| store::add_turn(conn, &answer_turn, &ids))
            .await
            .context("saving the answer")?;
        Ok(())
    }

    /// ❌: stops the answer that `message` belongs to, if `user` asked for it.
    fn stop(&self, message: MessageId, user: UserId) {
        for running in self.running.lock().unwrap().iter() {
            if running.requester == user && running.message_ids.lock().unwrap().contains(&message) {
                running.cancel.cancel();
            }
        }
    }

    fn is_running(&self, message: MessageId) -> bool {
        self.running
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.message_ids.lock().unwrap().contains(&message))
    }

    /// 🔁: writes the answer again, in the same messages, if `user` asked the question.
    async fn regenerate(&self, ctx: &BotCtx, reaction: &Reaction, user: UserId) -> Result<()> {
        if self.is_running(reaction.message_id) {
            return Ok(());
        }
        let Some(found) = find_answer(ctx, reaction.message_id, user).await? else {
            return Ok(());
        };
        let provider = provider(ctx)?;
        // Take the reaction away again, so it can be used for the next try.
        let _ = reaction.delete(&ctx.http).await;

        let question_message = found.question_messages.first().copied();
        let asker = Asker {
            user,
            guild: reaction.guild_id,
            channel: reaction.channel_id,
            message: question_message.unwrap_or(reaction.message_id),
        };
        let reply = LiveReply::resume(
            reaction.channel_id,
            question_message,
            &found.answer_messages,
        );
        info!("writing an answer again for 🔁");
        self.answer(ctx, provider.as_ref(), asker, found.question_id, reply)
            .await
    }

    /// ❌ on a finished answer: deletes its messages, if `user` asked the question.
    async fn delete(&self, ctx: &BotCtx, reaction: &Reaction, user: UserId) -> Result<()> {
        let Some(found) = find_answer(ctx, reaction.message_id, user).await? else {
            return Ok(());
        };
        for message in found.answer_messages {
            // A part someone already deleted is fine.
            if let Err(err) = reaction.channel_id.delete_message(&ctx.http, message).await {
                warn!("couldn't delete an answer message: {err}");
            }
        }
        info!("answer deleted with ❌");
        Ok(())
    }
}

/// Adds the features' context to the question, the last turn of the input. It isn't
/// stored, so later requests only carry the newest context.
fn add_context(input: &mut Input, texts: Vec<String>) {
    let turns = match input {
        Input::Full(turns) => turns,
        Input::After { new, .. } => new,
    };
    if let Some(question) = turns.last_mut() {
        question.parts.extend(texts.into_iter().map(Part::Text));
    }
}

/// An answer and the question it answers, found from one of the answer's messages.
struct FoundAnswer {
    question_id: i64,
    question_messages: Vec<MessageId>,
    answer_messages: Vec<MessageId>,
}

/// The answer that `message` is part of, if it is an answer and `user` asked the question.
async fn find_answer(
    ctx: &BotCtx,
    message: MessageId,
    user: UserId,
) -> Result<Option<FoundAnswer>> {
    let message = message.get();
    let found = ctx
        .db
        .call(move |conn| {
            let Some(answer) = store::turn_for_message(conn, message)? else {
                return Ok(None);
            };
            let Some(question_id) = answer.parent_id.filter(|_| answer.role == Role::Assistant)
            else {
                return Ok(None);
            };
            let Some(question) = store::get_turn(conn, question_id)? else {
                return Ok(None);
            };
            if question.author_id != user.get() {
                return Ok(None);
            }
            let ids = |turn| -> anyhow::Result<Vec<MessageId>> {
                Ok(store::messages_of(conn, turn)?
                    .into_iter()
                    .map(MessageId::new)
                    .collect())
            };
            Ok(Some(FoundAnswer {
                question_id,
                question_messages: ids(question_id)?,
                answer_messages: ids(answer.id)?,
            }))
        })
        .await?;
    Ok(found)
}
