//! Memory: a folder of notes the chat model keeps between conversations, like
//! `/memories/users/123/games.md`. One folder per server, and a private one per person in DMs.
//!
//! The model reads and writes it with the `memory` tool, whose commands match Anthropic's
//! memory tool, so the same folder works with Claude's built-in memory later. Nothing is
//! pasted into the system prompt: each question only carries the list of file names.
//!
//! - `folder.rs`: the commands on a folder (pure, no Discord or database)
//! - `store.rs`: the tables and queries, and the log of every change
//! - `tool.rs`: the chat tool and the file list
//! - `commands.rs`: `/memory show`, `forget` and `delete`
//! - `reflect.rs`: the daily reflection, where Vivy tidies her memory and updates her own notes
//!   and mood (which sets her Discord status)
//! - `diary.rs`: the weekly diary she posts in `diary_channels`

mod commands;
mod diary;
mod folder;
mod reflect;
mod store;
mod tool;

use async_trait::async_trait;
use serde_json::Value;

use crate::ai::ToolDef;
use crate::core::{Asker, BotCtx, Command, Feature, Result, Stat};

pub struct Memory;

#[async_trait]
impl Feature for Memory {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn commands(&self) -> Vec<Command> {
        vec![commands::memory()]
    }

    fn migrations(&self) -> &'static [&'static str] {
        store::MIGRATIONS
    }

    fn tools(&self) -> Vec<ToolDef> {
        vec![tool::def()]
    }

    async fn run_tool(
        &self,
        ctx: &BotCtx,
        asker: &Asker,
        _name: &str,
        args: &Value,
    ) -> Result<String> {
        tool::run(ctx, asker, args).await
    }

    /// The file list every time, and Vivy's own notes about herself at the start of a
    /// conversation (a continued one still has them from its first answer).
    async fn chat_context(
        &self,
        ctx: &BotCtx,
        asker: &Asker,
        fresh: bool,
    ) -> Result<Option<String>> {
        let mut text = tool::file_list(ctx, asker).await?;
        if fresh && let Some(notes) = tool::self_notes(ctx, asker).await? {
            text = format!("{notes}\n{text}");
        }
        Ok(Some(text))
    }

    async fn start(&self, ctx: &BotCtx) -> Result<()> {
        reflect::spawn(ctx);
        Ok(())
    }

    async fn stats(&self, ctx: &BotCtx) -> Result<Vec<Stat>> {
        let (files, folders) = ctx.db.call(|conn| Ok(store::stats(conn)?)).await?;
        Ok(vec![
            Stat::new("Files", files),
            Stat::new("Folders", folders),
        ])
    }
}
