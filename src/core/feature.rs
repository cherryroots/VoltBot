//! The `Feature` trait: the one thing every feature implements.
//!
//! A feature declares what it adds (commands, mention prefixes, tables, an import from
//! voltgpt) and handles the events it cares about. Every method has a default that does
//! nothing, so a feature only writes the ones it uses. The dispatcher calls these methods;
//! a feature never registers anything by hand.

use async_trait::async_trait;
use serenity::all::{
    ChannelId, ComponentInteraction, GuildId, Message, MessageId, ModalInteraction, Reaction,
    UserId,
};

use super::config::Config;
use super::{BotCtx, BotEvent, Command, Result};
use crate::ai::ToolDef;

#[async_trait]
pub trait Feature: Send + Sync + 'static {
    /// The feature's name. Also its `[features.<name>]` config section, the first part of
    /// its button IDs (`<name>:<action>:...`), and its label in logs.
    fn name(&self) -> &'static str;

    // ---- Declarations, read once at startup ----

    /// Slash commands.
    fn commands(&self) -> Vec<Command> {
        Vec::new()
    }

    /// SQL migrations for the feature's tables. See [`crate::core::db::migrate`].
    fn migrations(&self) -> &'static [&'static str] {
        &[]
    }

    /// Words that send a bot mention to this feature: "@Vivy remind me ..." goes to the
    /// feature that lists "remind me". Matching ignores case; the longest prefix wins.
    fn mention_prefixes(&self) -> &'static [&'static str] {
        &[]
    }

    /// How to import this feature's data from voltgpt's database. See [`crate::core::legacy`].
    fn legacy_import(&self) -> Option<LegacyImport> {
        None
    }

    /// Functions this feature offers to the chat model, like `create_reminder`. Chat lists
    /// the tools of every feature that is enabled where the conversation happens.
    fn tools(&self) -> Vec<ToolDef> {
        Vec::new()
    }

    /// Runs one of [`Feature::tools`] for the person who asked. Returns text for the model.
    /// An error is shown to the model as the result, so it can fix its call and try again;
    /// use [`crate::core::user_error`] for messages meant for it.
    async fn run_tool(
        &self,
        _ctx: &BotCtx,
        _asker: &Asker,
        name: &str,
        _args: &serde_json::Value,
    ) -> Result<String> {
        anyhow::bail!("{} has no tool named {name}", self.name())
    }

    /// Numbers for the control panel's status message.
    async fn stats(&self, _ctx: &BotCtx) -> Result<Vec<Stat>> {
        Ok(Vec::new())
    }

    // ---- Lifecycle ----

    /// Called once after connecting to Discord. Start background tasks here with
    /// `ctx.tasks.spawn(...)` and stop them when `ctx.shutdown` is cancelled.
    async fn start(&self, _ctx: &BotCtx) -> Result<()> {
        Ok(())
    }

    // ---- Events ----

    /// A message that mentions the bot and starts with one of [`Feature::mention_prefixes`].
    /// `rest` is the text after the mention and the prefix.
    async fn on_mention(&self, _ctx: &BotCtx, _msg: &Message, _rest: &str) -> Result<()> {
        Ok(())
    }

    /// Every message from a human, mention or not.
    async fn on_message(&self, _ctx: &BotCtx, _msg: &Message) -> Result<()> {
        Ok(())
    }

    async fn on_reaction_add(&self, _ctx: &BotCtx, _reaction: &Reaction) -> Result<()> {
        Ok(())
    }

    /// A button or select menu whose ID starts with this feature's name.
    /// `action` is the rest of the ID: for `reminders:snooze:12:10` it is `snooze:12:10`.
    async fn on_component(
        &self,
        _ctx: &BotCtx,
        _interaction: &ComponentInteraction,
        _action: &str,
    ) -> Result<()> {
        Ok(())
    }

    /// A submitted modal whose ID starts with this feature's name.
    async fn on_modal(
        &self,
        _ctx: &BotCtx,
        _interaction: &ModalInteraction,
        _action: &str,
    ) -> Result<()> {
        Ok(())
    }

    /// An event published by another feature.
    async fn on_bot_event(&self, _ctx: &BotCtx, _event: &BotEvent) -> Result<()> {
        Ok(())
    }
}

/// The person a chat tool runs for, and where they asked. Tools act as this person: they only
/// see what this person can see and only change this person's things.
#[derive(Debug, Clone)]
pub struct Asker {
    pub user: UserId,
    pub guild: Option<GuildId>,
    pub channel: ChannelId,
    /// The message that asked.
    pub message: MessageId,
}

/// One line on the control panel, like "Pending: 4".
#[derive(Debug, Clone)]
pub struct Stat {
    pub name: String,
    pub value: String,
}

impl Stat {
    pub fn new(name: impl Into<String>, value: impl ToString) -> Stat {
        Stat {
            name: name.into(),
            value: value.to_string(),
        }
    }
}

/// A part of voltgpt's database that a feature imports.
pub struct LegacyImport {
    /// The name recorded in `legacy_imports`, so the part is imported only once.
    /// Must be listed in [`crate::core::legacy::PARTS`].
    pub part: &'static str,
    /// Reads from voltgpt's database (`old`) and writes into ours (`new`). Runs inside one
    /// transaction: if it fails, nothing is written and the next start tries again.
    /// Returns the number of rows imported.
    pub run: fn(
        old: &rusqlite::Connection,
        new: &rusqlite::Transaction,
        config: &Config,
    ) -> anyhow::Result<usize>,
}
