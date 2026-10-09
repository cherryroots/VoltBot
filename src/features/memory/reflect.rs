//! The daily reflection: once a day, in each server whose memory changed, Vivy rereads her
//! memory folder, tidies it (merges duplicates, drops what's stale, keeps files short) and
//! updates her notes about herself from what she learned.

use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serenity::all::GuildId;
use tracing::{Instrument as _, error, info, info_span, warn};

use super::folder::{self, Command};
use super::{store, tool};
use crate::ai::{ChatRequest, Input, Part, Role, ToolCall, ToolRunner, Turn, complete};
use crate::core::{BotCtx, Result};

/// How often a server's folder is reflected on, at most.
const EVERY_SECS: i64 = 24 * 60 * 60;
/// How often to look for folders that are due.
const CHECK: Duration = Duration::from_secs(60 * 60);
/// Memory commands in one reflection, at most.
const MAX_ROUNDS: usize = 25;

const SYSTEM: &str = "You are Vivy, a Discord bot with a memory folder for this server. \
It's the end of the day, and you're looking after your memory: nobody is talking to you, and nothing you write here is posted. \
Use the memory tool to keep the folder useful: merge notes that say the same thing, fix notes that contradict each other (a person's own word about themselves wins), delete what's stale or trivial, move notes into the file where they belong, and keep each file short. \
Then update /memories/vivy/ from what you learned recently: your personality, your interests, your opinions, how you get along with people here. Grow naturally from what happened; don't invent big changes. Keep /memories/vivy/ under 2K. \
When you're done, answer with one line saying what you changed.";

pub fn spawn(ctx: &BotCtx) {
    let ctx = ctx.clone();
    let span = info_span!("reflection", feature = "memory");
    ctx.tasks.clone().spawn(run(ctx).instrument(span));
}

async fn run(ctx: BotCtx) {
    loop {
        if let Err(err) = reflect_due(&ctx).await {
            error!("reflecting on memory: {err:#}");
        }
        tokio::select! {
            () = tokio::time::sleep(CHECK) => {}
            () = ctx.shutdown.cancelled() => break,
        }
    }
}

async fn reflect_due(ctx: &BotCtx) -> Result<()> {
    if ctx.ai.chat.is_none() {
        return Ok(());
    }
    let now = Utc::now().timestamp();
    let due = ctx
        .db
        .call(move |conn| Ok(store::due_reflections(conn, now, EVERY_SECS)?))
        .await?;
    for scope in due {
        let allowed = scope
            .strip_prefix("server:")
            .and_then(|id| id.parse().ok())
            .is_some_and(|id| ctx.gate("memory").allows_guild(GuildId::new(id)));
        if !allowed {
            continue;
        }
        if let Err(err) = reflect(ctx, &scope).await {
            warn!(scope, "reflection failed: {err:#}");
        }
        // Done or failed, the next try is tomorrow.
        let (at, done) = (Utc::now().timestamp(), scope.clone());
        ctx.db
            .call(move |conn| Ok(store::set_reflected(conn, &done, at)?))
            .await?;
    }
    Ok(())
}

async fn reflect(ctx: &BotCtx, scope: &str) -> Result<()> {
    let provider = ctx
        .ai
        .chat
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no model"))?;
    let (folder, changes) = {
        let scope = scope.to_string();
        ctx.db
            .call(move |conn| {
                Ok((
                    store::load(conn, &scope)?,
                    store::changes_since_reflection(conn, &scope)?,
                ))
            })
            .await?
    };
    let mut copy = folder.clone();
    let listing = folder::run(
        &mut copy,
        Command::View {
            path: folder::ROOT.to_string(),
            view_range: None,
        },
    )
    .unwrap_or_else(|err| err);
    let changed: Vec<String> = changes
        .iter()
        .map(|(path, user, count)| {
            let who = if *user == ctx.bot_id.get() {
                "you".to_string()
            } else {
                format!("user {user}")
            };
            let times = if *count == 1 {
                "once".to_string()
            } else {
                format!("{count} times")
            };
            format!("- {path} (by {who}, {times})")
        })
        .collect();
    let mut text = format!(
        "{listing}\n\nChanged since your last reflection:\n{}",
        changed.join("\n")
    );
    if let Some(notes) = tool::self_notes_in(ctx, scope.to_string()).await? {
        text = format!("{notes}\n{text}");
    }

    let request = ChatRequest {
        system: SYSTEM.to_string(),
        input: Input::Full(vec![Turn {
            role: Role::User,
            parts: vec![Part::Text(text)],
        }]),
        tools: vec![tool::def()],
        cache_key: format!("memory:{scope}"),
    };
    let runner = Runner {
        ctx: ctx.clone(),
        scope: scope.to_string(),
    };
    let done = complete(provider.as_ref(), request, &runner, MAX_ROUNDS).await?;
    info!(scope, "reflected on memory: {}", done.text.trim());
    Ok(())
}

/// Runs memory commands in the folder being reflected on, logged as Vivy's own changes.
struct Runner {
    ctx: BotCtx,
    scope: String,
}

#[async_trait]
impl ToolRunner for Runner {
    async fn run(&self, call: &ToolCall) -> String {
        if call.name != "memory" {
            return format!("Error: there is no tool named {}.", call.name);
        }
        let user = self.ctx.bot_id.get();
        match tool::run_in(&self.ctx, self.scope.clone(), user, &call.args).await {
            Ok(text) => text,
            Err(err) => format!("Error: {err:#}"),
        }
    }
}
