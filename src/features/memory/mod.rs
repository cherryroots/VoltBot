//! Memory: a folder of notes the chat model keeps between conversations, like
//! `/memories/users/123.md`. One folder per server, and a private one per person in DMs.
//!
//! The model reads and writes it with the `memory` tool, whose commands match Anthropic's
//! memory tool, so the same folder works with Claude's built-in memory later. Nothing is
//! pasted into the system prompt: each question only carries the list of file names.
//!
//! - `folder.rs`: the commands on a folder (pure, no Discord or database)
//! - `store.rs`: the tables and queries, and the log of every change
//! - `tool.rs`: the chat tool and the file list
//! - `commands.rs`: `/memory show`, `forget` and `delete`

mod commands;
mod folder;
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

    async fn chat_context(&self, ctx: &BotCtx, asker: &Asker) -> Result<Option<String>> {
        Ok(Some(tool::file_list(ctx, asker).await?))
    }

    async fn stats(&self, ctx: &BotCtx) -> Result<Vec<Stat>> {
        let (files, folders) = ctx.db.call(|conn| Ok(store::stats(conn)?)).await?;
        Ok(vec![Stat::new(
            "Files",
            format!("{files} in {folders} folders"),
        )])
    }
}
