//! Snails: spotting reposts. A "snail" is someone posting a link or picture the server has
//! already seen.
//!
//! Every new message's links and pictures are saved as it arrives. "Check Snail" (right-click a
//! message → Apps) lists the earlier posts of the same thing. `/snail_backfill` reads the
//! server's older history, but only when an admin starts it.
//!
//! - `links.rs`: links to keys, so mirrors and tracking junk share one key (pure)
//! - `fingerprint.rs`: picture fingerprints and the detail check (pure)
//! - `collect.rs`: which links and pictures of a message count (pure)
//! - `store.rs`: the tables and queries
//! - `index.rs`: downloading and saving one message
//! - `check.rs`: the Check Snail command
//! - `backfill.rs`: `/snail_backfill` and the history crawl

mod backfill;
mod check;
mod collect;
mod fingerprint;
mod index;
mod links;
mod store;

use async_trait::async_trait;
use serenity::all::Message;
use tracing::warn;

use crate::core::{BotCtx, Command, Feature, Result, Stat};
use crate::util::media;

pub struct Snails;

#[async_trait]
impl Feature for Snails {
    fn name(&self) -> &'static str {
        "snails"
    }

    fn commands(&self) -> Vec<Command> {
        vec![check::check_snail(), backfill::snail_backfill()]
    }

    fn migrations(&self) -> &'static [&'static str] {
        store::MIGRATIONS
    }

    /// Carries on with crawls an admin started before the restart. Never starts a new one.
    async fn start(&self, ctx: &BotCtx) -> Result<()> {
        let guilds = ctx.db.call(|conn| Ok(store::running_guilds(conn)?)).await?;
        let gate = ctx.gate(self.name());
        for guild in guilds {
            let guild = serenity::all::GuildId::new(guild);
            if gate.allows_guild(guild) {
                backfill::spawn_worker(ctx, guild);
            }
        }
        Ok(())
    }

    async fn on_message(&self, ctx: &BotCtx, msg: &Message) -> Result<()> {
        let Some(guild) = msg.guild_id else {
            return Ok(());
        };
        if collect::message_links(msg).is_empty()
            && msg.attachments.is_empty()
            && msg.embeds.is_empty()
        {
            return Ok(());
        }
        let msg = media::with_previews(&ctx.http, msg).await;
        if let Err(err) = index::index_message(ctx, guild, &msg).await {
            warn!("couldn't save the message's links and pictures: {err:#}");
        }
        Ok(())
    }

    async fn stats(&self, ctx: &BotCtx) -> Result<Vec<Stat>> {
        let (links, pictures, finished, channels) =
            ctx.db.call(|conn| Ok(store::stats(conn)?)).await?;
        let mut stats = vec![Stat::new("Links", links), Stat::new("Pictures", pictures)];
        if channels > 0 {
            stats.push(Stat::new(
                "Backfill",
                format!("{finished}/{channels} channels"),
            ));
        }
        Ok(stats)
    }
}
