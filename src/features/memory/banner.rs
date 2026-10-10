//! Vivy's banner: one picture per time of day (`morning.png`, `day.png`, `evening.png`,
//! `night.png`) in the folder `banners_dir` under `[features.memory]`. Every hour, each
//! server where she has a mood gets the banner for the time where she lives (`timezone`).
//! A banner that's already there isn't sent again, so most hours do nothing.

use std::path::PathBuf;

use chrono::{Timelike as _, Utc};
use serde::Deserialize;
use serenity::all::GuildId;
use tracing::warn;

use super::face::{self, Slot};
use super::{mood, reflect, store};
use crate::core::BotCtx;

/// `[features.memory]` settings for her banner.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Settings {
    /// The folder with one PNG per time of day.
    banners_dir: Option<String>,
}

/// The time of day at `hour` (0 to 23, local time).
fn period(hour: u32) -> &'static str {
    match hour {
        5..=10 => "morning",
        11..=16 => "day",
        17..=21 => "evening",
        _ => "night",
    }
}

/// Puts the banner for this time of day on her profile in every server where she has a
/// mood. Does nothing without `banners_dir` or without a picture for this time of day.
pub async fn update_all(ctx: &BotCtx) {
    let settings: Settings = ctx.config.feature("memory").unwrap_or_default();
    let Some(dir) = settings.banners_dir.map(PathBuf::from) else {
        return;
    };
    let hour = Utc::now().with_timezone(&mood::zone(ctx)).hour();
    let name = period(hour);
    let path = dir.join(format!("{name}.png"));
    if !path.exists() {
        return;
    }
    let bot = ctx.bot_id.get();
    let moods = ctx
        .db
        .call(move |conn| Ok(store::every_file(conn, reflect::MOOD_FILE, bot)?))
        .await;
    let scopes = match moods {
        Ok(moods) => moods.into_iter().map(|(scope, _)| scope),
        Err(err) => {
            warn!("reading her moods: {err:#}");
            return;
        }
    };
    for scope in scopes {
        let Some(guild) = scope
            .strip_prefix("server:")
            .and_then(|id| id.parse().ok())
            .map(GuildId::new)
        else {
            continue;
        };
        if !ctx.gate("memory").allows_guild(guild) {
            continue;
        }
        if let Err(err) = face::set_picture(ctx, guild, Slot::Banner, path.clone(), name).await {
            warn!(%guild, name, "changing her banner: {err:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_of_day() {
        let names: Vec<&str> = [0, 4, 5, 10, 11, 16, 17, 21, 22, 23]
            .into_iter()
            .map(period)
            .collect();
        assert_eq!(
            names,
            [
                "night", "night", "morning", "morning", "day", "day", "evening", "evening",
                "night", "night"
            ]
        );
    }
}
