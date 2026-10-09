//! `BotCtx`: the shared services every feature handler receives.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serenity::all::{ActivityData, Cache, Http, ShardManager, UserId};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::config::{Config, Gate};
use super::db::Db;
use super::{BotEvent, Feature};
use crate::ai::Ai;

/// Cheap to clone: every field is a handle to something shared.
#[derive(Clone)]
pub struct BotCtx {
    /// Discord's REST API: send messages, edit them, respond to interactions.
    pub http: Arc<Http>,
    /// What the gateway has told us about servers, channels and users.
    pub cache: Arc<Cache>,
    /// The gateway connections, for latency and shutdown.
    pub shard_manager: Arc<ShardManager>,
    pub db: Db,
    pub config: Arc<Config>,
    /// AI models (chat, and one-off calls any feature can make).
    pub ai: Ai,
    /// For downloading files from the web (attachments, links). Shares connections.
    pub web: reqwest::Client,
    /// Publish with [`BotCtx::publish`]; the dispatcher delivers to every feature.
    pub events: broadcast::Sender<BotEvent>,
    /// Cancelled when the bot shuts down. Background tasks stop when they see it.
    pub shutdown: CancellationToken,
    /// Background tasks. Shutdown waits for them, so they can finish what they're doing.
    pub tasks: TaskTracker,
    /// Every feature, in the order of `features::all()`.
    pub features: Arc<Vec<Arc<dyn Feature>>>,
    pub bot_id: UserId,
    pub started_at: DateTime<Utc>,
}

impl BotCtx {
    /// Sends an event to every feature's `on_bot_event`.
    pub fn publish(&self, event: BotEvent) {
        // `send` only fails when nobody is subscribed, which is fine.
        let _ = self.events.send(event);
    }

    pub fn gate(&self, feature: &str) -> Gate {
        self.config.gate(feature)
    }

    /// Sets the bot's custom status, the line under its name, on every gateway connection.
    pub async fn set_status(&self, text: &str) {
        let activity = ActivityData::custom(text);
        for runner in self.shard_manager.runners.lock().await.values() {
            runner.runner_tx.set_activity(Some(activity.clone()));
        }
    }

    /// Whether the user is listed in `admins` in config.toml.
    pub fn is_admin(&self, user: UserId) -> bool {
        self.config.admins.contains(&user.get())
    }
}
