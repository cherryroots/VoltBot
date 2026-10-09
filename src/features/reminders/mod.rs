//! Reminders: "@Vivy remind me in 2h to check the oven".
//!
//! This file only wires the feature into the bot. The pieces live next to it:
//!
//! - `parse.rs`: the time grammar (pure, no Discord or database)
//! - `store.rs`: the tables and queries
//! - `scheduler.rs`: the background task that delivers reminders
//! - `commands.rs`: `/reminders` and `/timezone`
//! - `ui.rs`: message texts, buttons and the delete menu
//! - `import.rs`: reminders from voltgpt's database
//! - `tools.rs`: create, list and cancel reminders from chat

mod commands;
mod import;
mod parse;
mod scheduler;
mod store;
mod tools;
mod ui;

use std::sync::Arc;

use anyhow::{Context as _, bail};
use async_trait::async_trait;
use chrono::Utc;
use chrono_tz::Tz;
use serenity::all::{
    ComponentInteraction, ComponentInteractionDataKind, CreateAllowedMentions,
    CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage, Message,
    PremiumTier,
};
use tokio::sync::Notify;
use tracing::{info, warn};

use self::parse::When;
use self::store::{Image, NewReminder};
use self::ui::Action;
use crate::ai::ToolDef;
use crate::core::{
    Asker, BotCtx, Command, Feature, LegacyImport, Result, Stat, settings, user_error,
};

#[derive(Default)]
pub struct Reminders {
    /// Wakes the scheduler when a reminder is added or deleted.
    wake: Arc<Notify>,
}

#[async_trait]
impl Feature for Reminders {
    fn name(&self) -> &'static str {
        "reminders"
    }

    fn commands(&self) -> Vec<Command> {
        vec![commands::reminders(), commands::timezone()]
    }

    fn migrations(&self) -> &'static [&'static str] {
        store::MIGRATIONS
    }

    fn mention_prefixes(&self) -> &'static [&'static str] {
        &["remind me", "reminder", "remind"]
    }

    fn legacy_import(&self) -> Option<LegacyImport> {
        Some(LegacyImport {
            part: "reminders",
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
        name: &str,
        args: &serde_json::Value,
    ) -> Result<String> {
        let (changed, text) = tools::run(ctx, asker, name, args).await?;
        if changed {
            self.wake.notify_one();
        }
        Ok(text)
    }

    async fn stats(&self, ctx: &BotCtx) -> Result<Vec<Stat>> {
        let stats = ctx.db.call(|conn| Ok(store::stats(conn)?)).await?;
        let mut lines = vec![Stat::new("Pending", stats.pending)];
        if let Some(next) = stats.next_fire_at {
            lines.push(Stat::new("Next", format!("<t:{next}:R>")));
        }
        if stats.failing > 0 {
            lines.push(Stat::new("Failing to send", stats.failing));
        }
        Ok(lines)
    }

    async fn start(&self, ctx: &BotCtx) -> Result<()> {
        scheduler::spawn(ctx, self.wake.clone());
        Ok(())
    }

    /// "@Vivy remind me in 2h to check the oven": `rest` is "in 2h to check the oven".
    async fn on_mention(&self, ctx: &BotCtx, msg: &Message, rest: &str) -> Result<()> {
        let parsed = parse::parse_reminder(rest)
            .map_err(|err| user_error(format!("{err}\n{}", ui::HELP)))?;
        let saved_zone = settings::timezone(&ctx.db, msg.author.id).await?;
        let fire_at = parse::resolve(&parsed.when, Utc::now(), saved_zone.unwrap_or(Tz::UTC))
            .map_err(user_error)?
            .timestamp();

        // Only keep what the bot can upload again in this server, which depends on its boosts.
        let tier = msg
            .guild_id
            .and_then(|id| ctx.cache.guild(id).map(|guild| guild.premium_tier));
        let (images, skipped) = download_images(msg, upload_limit(tier)).await;
        let new = NewReminder {
            user_id: msg.author.id.get(),
            channel_id: msg.channel_id.get(),
            guild_id: msg.guild_id.map(|g| g.get()),
            message: parsed.message.clone(),
            fire_at,
            created_at: Utc::now().timestamp(),
            source_message_id: Some(msg.id.get()),
            missing_images: skipped.len() as u32,
            images,
        };
        let id = ctx
            .db
            .call(move |conn| Ok(store::add(conn, &new)?))
            .await
            .context("saving the reminder")?;
        self.wake.notify_one();
        info!(reminder = id, "reminder set for <t:{fire_at}:f>");

        // Mention /timezone when a clock time was read as UTC only because nothing was set.
        let zone_hint = saved_zone.is_none() && matches!(parsed.when, When::At { zone: None, .. });
        let reply = CreateMessage::new()
            .content(ui::confirmation(
                fire_at,
                &parsed.message,
                zone_hint,
                &skipped,
            ))
            .reference_message(msg)
            .allowed_mentions(CreateAllowedMentions::new());
        msg.channel_id.send_message(&ctx.http, reply).await?;
        Ok(())
    }

    async fn on_component(
        &self,
        ctx: &BotCtx,
        i: &ComponentInteraction,
        action: &str,
    ) -> Result<()> {
        match Action::parse(action) {
            Some(Action::Delete) => self.delete(ctx, i).await,
            Some(Action::Snooze { id, minutes }) => self.snooze(ctx, i, id, minutes).await,
            None => bail!("unknown action {action:?}"),
        }
    }
}

impl Reminders {
    /// The `/reminders` delete menu.
    async fn delete(&self, ctx: &BotCtx, i: &ComponentInteraction) -> Result<()> {
        let ComponentInteractionDataKind::StringSelect { values } = &i.data.kind else {
            bail!("the delete menu sent no values");
        };
        let id: i64 = values.first().context("no reminder selected")?.parse()?;
        let user = i.user.id.get();
        let deleted = ctx
            .db
            .call(move |conn| Ok(store::delete_pending(conn, id, user)?))
            .await?;
        self.wake.notify_one();

        let text = if deleted {
            "✅ Reminder deleted."
        } else {
            "That reminder is already gone."
        };
        let response = CreateInteractionResponseMessage::new()
            .content(text)
            .components(Vec::new());
        i.create_response(
            &ctx.http,
            CreateInteractionResponse::UpdateMessage(response),
        )
        .await?;
        Ok(())
    }

    /// A snooze button: the same reminder again, `minutes` from now.
    async fn snooze(
        &self,
        ctx: &BotCtx,
        i: &ComponentInteraction,
        id: i64,
        minutes: i64,
    ) -> Result<()> {
        let reminder = ctx
            .db
            .call(move |conn| Ok(store::get(conn, id)?))
            .await?
            .ok_or_else(|| user_error("This reminder is too old to snooze."))?;
        if reminder.user_id != i.user.id.get() {
            return Err(user_error(format!(
                "Only <@{}> can snooze this reminder.",
                reminder.user_id
            )));
        }

        let until = Utc::now().timestamp() + minutes * 60;
        let new = NewReminder {
            user_id: reminder.user_id,
            channel_id: reminder.channel_id,
            guild_id: reminder.guild_id,
            message: reminder.message,
            fire_at: until,
            created_at: reminder.created_at,
            source_message_id: reminder.source_message_id,
            missing_images: reminder.missing_images,
            images: reminder.images,
        };
        ctx.db.call(move |conn| Ok(store::add(conn, &new)?)).await?;
        self.wake.notify_one();

        // Swap the buttons for a "snoozed until" line, so it can't be snoozed twice.
        let response = CreateInteractionResponseMessage::new()
            .content(ui::snoozed(&i.message.content, until))
            .components(Vec::new());
        i.create_response(
            &ctx.http,
            CreateInteractionResponse::UpdateMessage(response),
        )
        .await?;
        Ok(())
    }
}

/// The biggest file the bot can upload in a server with this boost level, in bytes.
/// DMs (`None`) get the base limit.
fn upload_limit(tier: Option<PremiumTier>) -> u32 {
    const MIB: u32 = 1024 * 1024;
    match tier {
        Some(PremiumTier::Tier2) => 50 * MIB,
        Some(PremiumTier::Tier3) => 100 * MIB,
        // Raised from 10 MiB for bots in September 2026.
        _ => 20 * MIB,
    }
}

/// Downloads the images (and videos) attached to the message. Returns them and the names
/// of the ones that couldn't be kept: bigger than `max_bytes`, or the download failed.
async fn download_images(msg: &Message, max_bytes: u32) -> (Vec<Image>, Vec<String>) {
    let mut images = Vec::new();
    let mut skipped = Vec::new();
    // Discord only sets a width on images and videos.
    for attachment in msg.attachments.iter().filter(|a| a.width.is_some()) {
        if attachment.size > max_bytes {
            skipped.push(attachment.filename.clone());
            continue;
        }
        match attachment.download().await {
            Ok(data) => images.push(Image {
                filename: attachment.filename.clone(),
                data,
            }),
            Err(err) => {
                warn!("couldn't download {}: {err}", attachment.filename);
                skipped.push(attachment.filename.clone());
            }
        }
    }
    (images, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_limit_follows_boosts() {
        let mib = 1024 * 1024;
        assert_eq!(upload_limit(None), 20 * mib);
        assert_eq!(upload_limit(Some(PremiumTier::Tier1)), 20 * mib);
        assert_eq!(upload_limit(Some(PremiumTier::Tier2)), 50 * mib);
        assert_eq!(upload_limit(Some(PremiumTier::Tier3)), 100 * mib);
    }
}
