//! Control panel: a status message in a Discord channel that the bot keeps up to date.
//!
//! The other half of the control panel, the log channel, is part of the core logging setup
//! (`core::logging`), so it also works when this feature is turned off.

mod status;

use async_trait::async_trait;
use serde::Deserialize;
use serenity::all::ChannelId;
use tracing::{Instrument as _, info_span};

use crate::core::{BotCtx, Feature, Result};

#[derive(Debug, Deserialize)]
#[serde(default)]
struct Settings {
    /// Channel for the status message. Without one this feature does nothing.
    status_channel: Option<u64>,
    /// Seconds between refreshes.
    status_interval_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            status_channel: None,
            status_interval_secs: 60,
        }
    }
}

#[derive(Default)]
pub struct ControlPanel;

#[async_trait]
impl Feature for ControlPanel {
    fn name(&self) -> &'static str {
        "control_panel"
    }

    fn migrations(&self) -> &'static [&'static str] {
        status::MIGRATIONS
    }

    async fn start(&self, ctx: &BotCtx) -> Result<()> {
        let settings: Settings = ctx.config.feature(self.name())?;
        let Some(channel) = settings.status_channel else {
            return Ok(());
        };
        // Discord allows about 5 edits per 5 seconds; stay far below that.
        let interval = settings.status_interval_secs.max(10);
        let span = info_span!("status", feature = "control_panel");
        let task = status::run(ctx.clone(), ChannelId::new(channel), interval).instrument(span);
        ctx.tasks.spawn(task);
        Ok(())
    }
}
