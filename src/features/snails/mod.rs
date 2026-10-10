//! Snails: spotting reposts. A "snail" is someone posting a link or picture the server has
//! already seen.
//!
//! Every new message's links and pictures are saved as it arrives, and a new message that
//! repeats an earlier post is counted (quietly, for the control panel). "Check Snail" (right-click a
//! message → Apps) lists the earlier posts of the same thing. `/snail_backfill` reads the
//! server's older history, but only when an admin starts it; after that, each start catches
//! up on what those channels got while the bot was offline.
//!
//! Edits are read again (links and pictures added count, removed ones stop counting), and
//! deleted messages are forgotten. A snail that gets deleted stays counted as caught: it was
//! posted. Pictures that fail to download are retried later and listed by host in
//! `/snail_backfill status`.
//!
//! - `links.rs`: links to keys, so mirrors and tracking junk share one key (pure)
//! - `fingerprint.rs`: picture fingerprints and the detail check (pure)
//! - `collect.rs`: which links and pictures of a message count (pure)
//! - `store.rs`: the tables and queries
//! - `index.rs`: downloading and saving one message
//! - `check.rs`: the Check Snail command
//! - `backfill.rs`: `/snail_backfill`, the history crawl and the startup catch-up
//! - `retry.rs`: trying failed pictures again

mod backfill;
mod check;
mod collect;
mod fingerprint;
mod index;
mod links;
mod retry;
mod store;

use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serenity::all::{ChannelId, GuildId, Message, MessageId, MessageUpdateEvent};
use tracing::{debug, warn};

use crate::core::{BotCtx, Command, Feature, Result, Stat};
use crate::util::media;

/// How long after a new message it's read again for late link previews.
const PREVIEW_RECHECK: Duration = Duration::from_secs(12);

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

    /// Carries on with crawls an admin started before the restart, catches up channels
    /// crawled before, and starts retrying failed pictures. Never starts a new crawl.
    async fn start(&self, ctx: &BotCtx) -> Result<()> {
        let (running, crawled) = ctx
            .db
            .call(|conn| Ok((store::running_guilds(conn)?, store::crawled_guilds(conn)?)))
            .await?;
        let gate = ctx.gate(self.name());
        for guild in running {
            let guild = GuildId::new(guild);
            if gate.allows_guild(guild) {
                backfill::spawn_worker(ctx, guild);
            }
        }
        let crawled = crawled
            .into_iter()
            .map(GuildId::new)
            .filter(|&guild| gate.allows_guild(guild))
            .collect();
        backfill::spawn_catch_up(ctx, crawled);
        retry::spawn(ctx);
        Ok(())
    }

    /// Saves a new message's links and pictures right away, then reads it once more a bit
    /// later: Discord adds link previews (and their pictures) a few seconds after the
    /// message. That usually arrives as an edit too; reading twice is harmless, since only
    /// what's new is checked and a message counts as a snail once.
    async fn on_message(&self, ctx: &BotCtx, msg: &Message) -> Result<()> {
        let Some(guild) = msg.guild_id else {
            return Ok(());
        };
        if !collect::has_content(msg) {
            return Ok(());
        }
        if let Err(err) = check::on_new_message(ctx, guild, msg).await {
            warn!("couldn't save the message's links and pictures: {err:#}");
        }
        if collect::message_links(msg).is_empty() {
            return Ok(());
        }
        tokio::select! {
            _ = ctx.shutdown.cancelled() => return Ok(()),
            _ = tokio::time::sleep(PREVIEW_RECHECK) => {}
        }
        match msg.channel_id.message(&ctx.http, msg.id).await {
            Ok(fresh) => {
                if let Err(err) = check::on_new_message(ctx, guild, &fresh).await {
                    warn!("couldn't save the message's late link previews: {err:#}");
                }
            }
            // Deleted in the meantime: on_messages_deleted forgets it.
            Err(err) => debug!("couldn't read the message again for link previews: {err}"),
        }
        Ok(())
    }

    /// An edit (or a link preview Discord added later): reads the message again so added
    /// links and pictures count and removed ones stop counting.
    async fn on_message_edit(&self, ctx: &BotCtx, event: &MessageUpdateEvent) -> Result<()> {
        let Some(guild) = event.guild_id else {
            return Ok(());
        };
        // Pins, reactions' flags and the like: nothing that's indexed changed.
        if event.content.is_none() && event.embeds.is_none() && event.attachments.is_none() {
            return Ok(());
        }
        // Skip the read when the message has nothing to index now and had nothing before.
        let may_have_content = event
            .content
            .as_deref()
            .is_some_and(|c| !media::links(c).is_empty())
            || event.embeds.as_ref().is_some_and(|e| !e.is_empty())
            || event.attachments.as_ref().is_some_and(|a| !a.is_empty());
        let id = event.id.get();
        if !may_have_content
            && !ctx
                .db
                .call(move |conn| Ok(store::is_indexed(conn, id)?))
                .await?
        {
            return Ok(());
        }
        let msg = match event.channel_id.message(&ctx.http, event.id).await {
            Ok(msg) => msg,
            Err(err) => {
                debug!("couldn't read an edited message: {err}");
                return Ok(());
            }
        };
        if msg.author.bot {
            return Ok(());
        }
        if let Err(err) = check::on_new_message(ctx, guild, &msg).await {
            warn!("couldn't save an edited message's links and pictures: {err:#}");
        }
        Ok(())
    }

    /// Deleted messages stop counting as earlier posts. Snails already counted stay counted.
    async fn on_messages_deleted(
        &self,
        ctx: &BotCtx,
        _channel: ChannelId,
        messages: &[MessageId],
    ) -> Result<()> {
        let ids: Vec<u64> = messages.iter().map(|m| m.get()).collect();
        ctx.db
            .call(move |conn| {
                for id in ids {
                    store::forget(conn, id)?;
                }
                Ok(())
            })
            .await
    }

    async fn stats(&self, ctx: &BotCtx) -> Result<Vec<Stat>> {
        let week_ago = Utc::now().timestamp() - 7 * 86_400;
        let ((links, pictures, finished, channels), caught) = ctx
            .db
            .call(move |conn| Ok((store::stats(conn)?, store::caught_since(conn, week_ago)?)))
            .await?;
        let mut stats = vec![
            Stat::new("Caught this week", caught),
            Stat::new("Links", links),
            Stat::new("Pictures", pictures),
        ];
        if channels > 0 {
            stats.push(Stat::new(
                "Backfill",
                format!("{finished}/{channels} channels"),
            ));
        }
        Ok(stats)
    }
}
