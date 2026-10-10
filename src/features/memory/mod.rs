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
//! - `mood.rs`: mood checks a few times a day, which follow the time of day
//! - `set_mood.rs`: the `set_mood` tool, for when a conversation changes her mood
//! - `face.rs`: her face, a picture per mood that becomes her avatar in each server
//! - `banner.rs`: her banner, a picture per time of day
//! - `diary.rs`: the weekly diary she posts in `diary_channels`

mod banner;
mod commands;
mod diary;
mod face;
mod folder;
mod mood;
mod reflect;
mod set_mood;
mod store;
mod tool;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;

use crate::ai::ToolDef;
use crate::core::{Asker, BotCtx, Command, Feature, Panel, Result, Stat};

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
        vec![tool::def(), set_mood::def()]
    }

    async fn run_tool(
        &self,
        ctx: &BotCtx,
        asker: &Asker,
        name: &str,
        args: &Value,
    ) -> Result<String> {
        match name {
            set_mood::NAME => set_mood::run(ctx, asker, args).await,
            _ => tool::run(ctx, asker, args).await,
        }
    }

    /// The file list every time, and Vivy's own notes about herself (with her faces) at the
    /// start of a conversation (a continued one still has them from its first answer).
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
        if fresh
            && asker.guild.is_some()
            && let Some(faces) = set_mood::faces_note(ctx)
        {
            text = format!("{faces}\n{text}");
        }
        Ok(Some(text))
    }

    async fn start(&self, ctx: &BotCtx) -> Result<()> {
        reflect::spawn(ctx);
        Ok(())
    }

    async fn stats(&self, ctx: &BotCtx) -> Result<Vec<Stat>> {
        let day_ago = Utc::now().timestamp() - 86_400;
        let ((files, folders), (changes, reflected)) = ctx
            .db
            .call(move |conn| Ok((store::stats(conn)?, store::activity(conn, day_ago)?)))
            .await?;
        let mut stats = vec![
            Stat::new("Files", files),
            Stat::new("Folders", folders),
            Stat::new("Changes today", changes),
        ];
        if let Some(at) = reflected {
            stats.push(Stat::new("Last reflection", format!("<t:{at}:R>")));
        }
        Ok(stats)
    }

    /// Her mood, what's on her mind and what she wonders about, from the mood file she
    /// rewrote last (the same one her Discord status comes from), with its face.
    async fn panels(&self, ctx: &BotCtx) -> Result<Vec<Panel>> {
        let newest = ctx
            .db
            .call(|conn| Ok(store::newest_file(conn, reflect::MOOD_FILE)?))
            .await?;
        let Some((_, mood)) = newest else {
            return Ok(Vec::new());
        };
        let rows: Vec<Stat> = [
            ("Mood", "mood"),
            ("Thinking about", "thinking"),
            ("Wants to know", "wondering"),
        ]
        .into_iter()
        .filter_map(|(name, label)| Some(Stat::new(name, reflect::mood_line(&mood, label)?)))
        .collect();
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let mut panel = Panel::about_bot(ctx, rows);
        panel.picture = face::panel_picture(ctx, &mood).await;
        Ok(vec![panel])
    }
}
