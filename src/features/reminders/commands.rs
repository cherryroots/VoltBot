//! Slash commands: `/reminders` and `/timezone`.

use chrono::Utc;
use serenity::all::{AutocompleteChoice, CreateAutocompleteResponse};

use super::{parse, store, ui};
use crate::core::{Context, Result, settings, user_error};

/// List your pending reminders and delete them
#[poise::command(slash_command, ephemeral)]
pub async fn reminders(ctx: Context<'_>) -> Result<()> {
    let user = ctx.author().id.get();
    let list = ctx
        .data()
        .db
        .call(move |conn| Ok(store::list_pending(conn, user)?))
        .await?;
    ctx.send(ui::list_reply(&list)).await?;
    Ok(())
}

/// Set the timezone used for your reminders
#[poise::command(slash_command, ephemeral)]
pub async fn timezone(
    ctx: Context<'_>,
    #[description = "A zone like Europe/Oslo or an abbreviation like CET. Leave empty to see yours."]
    #[autocomplete = "autocomplete_zone"]
    zone: Option<String>,
) -> Result<()> {
    let db = &ctx.data().db;
    let user = ctx.author().id;

    let Some(name) = zone else {
        let text = match settings::timezone(db, user).await? {
            Some(zone) => format!("Your timezone is **{zone}**. {}", local_time(zone)),
            None => "You haven't set a timezone, so reminders use UTC.".to_string(),
        };
        ctx.say(text).await?;
        return Ok(());
    };

    let zone = parse::lookup_zone(name.trim()).ok_or_else(|| {
        user_error(format!(
            "I don't know the timezone \"{name}\". Pick one from the list, like Europe/Oslo."
        ))
    })?;
    settings::set_timezone(db, user, zone).await?;
    ctx.say(format!(
        "Your timezone is now **{zone}**. {}",
        local_time(zone)
    ))
    .await?;
    Ok(())
}

fn local_time(zone: chrono_tz::Tz) -> String {
    format!(
        "It's {} there now.",
        Utc::now().with_timezone(&zone).format("%H:%M")
    )
}

/// Suggests zone names containing what the user typed so far.
async fn autocomplete_zone(_ctx: Context<'_>, partial: &str) -> CreateAutocompleteResponse {
    let partial = partial.to_lowercase();
    let choices = chrono_tz::TZ_VARIANTS
        .iter()
        .map(|zone| zone.name())
        .filter(|name| name.contains('/') && name.to_lowercase().contains(&partial))
        .take(25)
        .map(AutocompleteChoice::from)
        .collect();
    CreateAutocompleteResponse::new().set_choices(choices)
}
