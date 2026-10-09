//! Slash commands: `/wheel_status` for everyone, `/wheel_add`, `/insert_bet` and
//! `/reset_wheel` for admins.

use anyhow::Context as _;
use chrono::Utc;
use poise::CreateReply;
use serenity::all::{GuildId, User};
use tracing::info;

use super::actions::{Game, active_game, status_message};
use super::ledger::Bet;
use super::{names, store, ui};
use crate::core::{Context, Result, user_error};

/// Movie wheel: the current round, or an earlier one
#[poise::command(slash_command, guild_only)]
pub async fn wheel_status(
    ctx: Context<'_>,
    #[description = "Which round to show (default: the current one)"]
    #[min = 1]
    round: Option<i64>,
    #[description = "Which season (default: the one being played)"]
    #[min = 1]
    season: Option<i64>,
) -> Result<()> {
    let guild = guild(ctx)?;
    // Looking up names and drawing can take longer than Discord waits for an answer.
    ctx.defer().await?;
    let now = Utc::now().timestamp();
    let game = ctx
        .data()
        .db
        .call(move |conn| {
            let tx = conn.transaction()?;
            let (id, active) = match season {
                None => (store::ensure_season(&tx, guild.get(), now)?, true),
                Some(number) => {
                    let list = store::seasons(&tx, guild.get())?;
                    let found = usize::try_from(number - 1)
                        .ok()
                        .and_then(|n| list.get(n))
                        .ok_or_else(|| {
                            user_error(format!("This server has {} seasons.", list.len()))
                        })?;
                    (found.id, found.ended_at.is_none())
                }
            };
            let season = store::load_season(&tx, id)?;
            tx.commit()?;
            Ok(Game { season, active })
        })
        .await?;

    let count = game.season.rounds.len();
    let index = match round {
        None => count - 1,
        Some(number) => usize::try_from(number - 1)
            .ok()
            .filter(|&n| n < count)
            .ok_or_else(|| user_error(format!("Pick a round from 1 to {count}.")))?,
    };
    let status = status_message(ctx.data(), guild, &game, index).await?;
    ctx.send(
        CreateReply::default()
            .attachment(status.picture)
            .components(status.buttons),
    )
    .await?;
    Ok(())
}

/// Add a user to the wheel, or take them off (admins)
#[poise::command(slash_command, guild_only, ephemeral)]
pub async fn wheel_add(
    ctx: Context<'_>,
    #[description = "User to add to the wheel"] user: User,
    #[description = "Remove the user from the wheel"] remove: Option<bool>,
) -> Result<()> {
    require_admin(ctx)?;
    let guild = guild(ctx)?;
    let remove = remove.unwrap_or(false);
    let id = user.id.get();
    let now = Utc::now().timestamp();
    let changed = ctx
        .data()
        .db
        .call(move |conn| {
            let tx = conn.transaction()?;
            let season = store::ensure_season(&tx, guild.get(), now)?;
            let changed = if remove {
                store::remove_option(&tx, season, id)?
            } else {
                store::add_option(&tx, season, id)?
            };
            tx.commit()?;
            Ok(changed)
        })
        .await?;

    let names = names::lookup(ctx.data(), guild, &[id]).await;
    let name = ui::name(&names, id);
    let text = match (remove, changed) {
        (false, true) => format!("Added {name} to the wheel!"),
        (false, false) => format!("{name} is already on the wheel."),
        (true, true) => format!("Removed {name} from the wheel!"),
        (true, false) => format!("{name} isn't on the wheel."),
    };
    if changed {
        info!("{text}");
    }
    ctx.say(text).await?;
    Ok(())
}

/// Add, change or remove a bet in any round of the current season (admins)
#[poise::command(slash_command, guild_only, ephemeral)]
pub async fn insert_bet(
    ctx: Context<'_>,
    #[description = "Who made the bet"] by: User,
    #[description = "Who the bet is on"] on: User,
    #[description = "How much was bet; 0 removes the bet"]
    #[min = 0]
    amount: i64,
    #[description = "Which round"]
    #[min = 1]
    round: i64,
) -> Result<()> {
    require_admin(ctx)?;
    let guild = guild(ctx)?;
    if amount < 0 {
        return Err(user_error("The amount can't be negative."));
    }
    let bet = Bet {
        by: by.id.get(),
        on: on.id.get(),
        amount,
    };
    let saved = bet.clone();
    let changed =
        ctx.data()
            .db
            .call(move |conn| {
                let tx = conn.transaction()?;
                let game = active_game(&tx, guild.get())?;
                let rounds = &game.season.rounds;
                let found = rounds.iter().find(|r| r.number == round).ok_or_else(|| {
                    user_error(format!("Pick a round from 1 to {}.", rounds.len()))
                })?;
                let changed = if saved.amount == 0 {
                    store::remove_bet(&tx, found.id, saved.by, saved.on)?
                } else {
                    store::place_bet(&tx, found.id, &saved)?;
                    true
                };
                tx.commit()?;
                Ok(changed)
            })
            .await?;

    let names = names::lookup(ctx.data(), guild, &[bet.by, bet.on]).await;
    let (by, on) = (ui::name(&names, bet.by), ui::name(&names, bet.on));
    let text = match (amount, changed) {
        (0, true) => format!("Removed the bet by {by} on {on} in round {round}."),
        (0, false) => format!("{by} has no bet on {on} in round {round}."),
        _ => format!("Set the bet by {by} on {on} in round {round} to {amount}."),
    };
    if changed {
        info!("{text}");
    }
    ctx.say(text).await?;
    Ok(())
}

/// Start a new season of the wheel; the old one stays viewable (admins)
#[poise::command(slash_command, guild_only, ephemeral)]
pub async fn reset_wheel(
    ctx: Context<'_>,
    #[description = "Keep the current wheel options"] keep_options: Option<bool>,
) -> Result<()> {
    require_admin(ctx)?;
    let (text, button) = ui::reset_confirmation(keep_options.unwrap_or(false));
    ctx.send(CreateReply::default().content(text).components(button))
        .await?;
    Ok(())
}

fn guild(ctx: Context<'_>) -> Result<GuildId> {
    ctx.guild_id()
        .context("guild_only command without a server")
}

fn require_admin(ctx: Context<'_>) -> Result<()> {
    if ctx.data().is_admin(ctx.author().id) {
        Ok(())
    } else {
        Err(user_error("Only admins can use this command."))
    }
}
