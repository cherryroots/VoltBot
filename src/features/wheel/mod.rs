//! The movie wheel: a betting game for movie night. Admins put people on the wheel, players
//! claim money each round and bet on who wins, and admins set the winner.
//!
//! This file only wires the feature into the bot. The pieces live next to it:
//!
//! - `ledger.rs`: the money rules (pure, no Discord or database)
//! - `store.rs`: the tables and queries
//! - `actions.rs`: the buttons, menus and bet modal
//! - `ui.rs`: the status embed, buttons, menus and modal, and their IDs
//! - `names.rs`: display names for the stored user IDs
//! - `commands.rs`: `/wheel_status`, `/wheel_add`, `/insert_bet`, `/reset_wheel`
//! - `import.rs`: the running game from voltgpt's database
//! - `tools.rs`: `get_wheel_status` for chat

mod actions;
mod commands;
mod import;
mod ledger;
mod names;
mod store;
mod tools;
mod ui;

use async_trait::async_trait;
use serenity::all::{ComponentInteraction, ModalInteraction};

use crate::ai::ToolDef;
use crate::core::{Asker, BotCtx, Command, Feature, LegacyImport, Result, Stat};

pub struct Wheel;

#[async_trait]
impl Feature for Wheel {
    fn name(&self) -> &'static str {
        "wheel"
    }

    fn commands(&self) -> Vec<Command> {
        vec![
            commands::wheel_status(),
            commands::wheel_add(),
            commands::insert_bet(),
            commands::reset_wheel(),
        ]
    }

    fn migrations(&self) -> &'static [&'static str] {
        store::MIGRATIONS
    }

    fn legacy_import(&self) -> Option<LegacyImport> {
        Some(LegacyImport {
            part: "wheel",
            run: import::import,
        })
    }

    fn tools(&self) -> Vec<ToolDef> {
        tools::defs()
    }

    async fn run_tool(
        &self,
        ctx: &BotCtx,
        asker: &Asker,
        _name: &str,
        args: &serde_json::Value,
    ) -> Result<String> {
        tools::run(ctx, asker, args).await
    }

    async fn stats(&self, ctx: &BotCtx) -> Result<Vec<Stat>> {
        let stats = ctx.db.call(|conn| Ok(store::stats(conn)?)).await?;
        Ok(vec![
            Stat::new("Games", stats.games),
            Stat::new("Open bets", stats.open_bets),
        ])
    }

    async fn on_component(
        &self,
        ctx: &BotCtx,
        interaction: &ComponentInteraction,
        action: &str,
    ) -> Result<()> {
        actions::on_component(ctx, interaction, action).await
    }

    async fn on_modal(
        &self,
        ctx: &BotCtx,
        interaction: &ModalInteraction,
        action: &str,
    ) -> Result<()> {
        actions::on_modal(ctx, interaction, action).await
    }
}
