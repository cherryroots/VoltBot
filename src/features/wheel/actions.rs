//! The buttons, menus and bet modal. Each handler checks and changes the game in one
//! database transaction, and only talks to Discord after it committed.

use anyhow::{Context as _, bail};
use chrono::Utc;
use rusqlite::Connection;
use serenity::all::{
    ActionRowComponent, ChannelId, ComponentInteraction, ComponentInteractionDataKind,
    CreateActionRow, CreateAttachment, CreateInteractionResponse,
    CreateInteractionResponseFollowup, CreateInteractionResponseMessage, EditMessage, GuildId,
    MessageId, ModalInteraction, UserId,
};
use tracing::{info, warn};

use super::ledger::{self, CLAIM, Season, TAX_THRESHOLD, check_bet, ledger};
use super::ui::{self, Action, PickKind, View};
use super::{names, render, store};
use crate::core::{BotCtx, BotEvent, Result, user_error};

const SERVER_ONLY: &str = "The wheel only works in a server.";
const NO_GAME: &str = "There's no game running. Start one with /wheel_status.";
const NOT_CURRENT: &str = "Only the current round can be changed from this message.";

/// A season as loaded, and whether it's the one being played.
pub struct Game {
    pub season: Season,
    pub active: bool,
}

impl Game {
    fn latest(&self) -> usize {
        self.season.rounds.len() - 1
    }
}

/// A status message: the round as a picture, with its buttons under it.
pub struct Status {
    pub picture: CreateAttachment,
    pub buttons: Vec<CreateActionRow>,
}

impl Status {
    /// Replaces the message this status's button was on. Clears the embed that status
    /// messages from before the picture had.
    fn update(self) -> CreateInteractionResponse {
        CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .embeds(Vec::new())
                .files([self.picture])
                .components(self.buttons),
        )
    }

    /// The same, as an edit of a message by ID.
    fn edit(self) -> EditMessage {
        EditMessage::new()
            .embeds(Vec::new())
            .remove_all_attachments()
            .new_attachment(self.picture)
            .components(self.buttons)
    }
}

/// The picture and buttons of round `index` of a game.
pub async fn status_message(
    ctx: &BotCtx,
    guild: GuildId,
    game: &Game,
    index: usize,
) -> Result<Status> {
    let names = names::lookup(ctx, guild, &ledger::users(&game.season)).await;
    let numbers = ledger(&game.season);
    let view = View {
        season: &game.season,
        ledger: &numbers,
        index,
        active: game.active,
        names: &names,
    };
    let svg = render::round_svg(&view);
    let buttons = ui::status_buttons(&view);
    // Drawing takes a few milliseconds of CPU, so it runs on tokio's blocking threads.
    let png = tokio::task::spawn_blocking(move || render::png(&svg)).await??;

    // Discord blurs files named SPOILER_, which hides the winner as voltgpt did.
    let round = &game.season.rounds[index];
    let file = match round.winner {
        Some(_) => format!("SPOILER_round-{}.png", round.number),
        None => format!("round-{}.png", round.number),
    };
    Ok(Status {
        picture: CreateAttachment::bytes(png, file),
        buttons,
    })
}

/// Loads the server's game. Fails with [`NO_GAME`] if there is none.
pub fn active_game(conn: &Connection, guild: u64) -> Result<Game> {
    let id = store::active_season(conn, guild)?.ok_or_else(|| user_error(NO_GAME))?;
    Ok(Game {
        season: store::load_season(conn, id)?,
        active: true,
    })
}

/// Loads the server's game and checks that `round` is its open, latest round.
fn open_round(conn: &Connection, guild: u64, round: i64) -> Result<Game> {
    let game = active_game(conn, guild)?;
    match game.season.rounds.last() {
        Some(last) if last.id == round && last.winner.is_none() => Ok(game),
        _ => Err(user_error(NOT_CURRENT)),
    }
}

fn now() -> i64 {
    Utc::now().timestamp()
}

fn require_admin(ctx: &BotCtx, user: UserId, what: &str) -> Result<()> {
    if ctx.is_admin(user) {
        Ok(())
    } else {
        Err(user_error(format!("Only admins can {what}.")))
    }
}

pub async fn on_component(ctx: &BotCtx, i: &ComponentInteraction, action: &str) -> Result<()> {
    let guild = i.guild_id.ok_or_else(|| user_error(SERVER_ONLY))?;
    match Action::parse(action) {
        Some(Action::Current) => current(ctx, i, guild).await,
        Some(Action::Claim { round }) => claim(ctx, i, guild, round).await,
        Some(Action::Bet { round }) => open_menu(ctx, i, guild, PickKind::Place, round).await,
        Some(Action::Unbet { round }) => open_menu(ctx, i, guild, PickKind::Remove, round).await,
        Some(Action::Winner { round }) => open_menu(ctx, i, guild, PickKind::Winner, round).await,
        Some(Action::Undo { round }) => undo(ctx, i, guild, round).await,
        Some(Action::Pick {
            kind,
            round,
            message,
        }) => picked(ctx, i, guild, kind, round, MessageId::new(message)).await,
        Some(Action::Reset { keep_options }) => reset(ctx, i, guild, keep_options).await,
        Some(Action::Amount { .. }) | None => bail!("unknown action {action:?}"),
    }
}

pub async fn on_modal(ctx: &BotCtx, i: &ModalInteraction, action: &str) -> Result<()> {
    let Some(Action::Amount { round, on, message }) = Action::parse(action) else {
        bail!("unknown modal {action:?}");
    };
    let guild = i.guild_id.ok_or_else(|| user_error(SERVER_ONLY))?;
    let input = modal_value(i).unwrap_or_default();
    let by = i.user.id.get();

    let (game, amount) = ctx
        .db
        .call(move |conn| {
            let tx = conn.transaction()?;
            let game = open_round(&tx, guild.get(), round)?;
            let amount = check_bet(&game.season, by, on, &input).map_err(user_error)?;
            store::place_bet(&tx, round, &ledger::Bet { by, on, amount })?;
            let season = store::load_season(&tx, game.season.id)?;
            tx.commit()?;
            Ok((
                Game {
                    season,
                    active: true,
                },
                amount,
            ))
        })
        .await?;
    info!("bet {amount} on {on}");

    let on_name = names::lookup(ctx, guild, &[on]).await.remove(&on);
    let text = format!("Bet {amount} on {}.", on_name.unwrap_or_default());
    i.create_response(&ctx.http, update_text(text)).await?;
    let message = MessageId::new(message);
    edit_status(ctx, i.channel_id, message, guild, &game, game.latest()).await;
    Ok(())
}

/// "View Current Round": shows the latest round on this message.
async fn current(ctx: &BotCtx, i: &ComponentInteraction, guild: GuildId) -> Result<()> {
    let game = ctx
        .db
        .call(move |conn| active_game(conn, guild.get()))
        .await?;
    let status = status_message(ctx, guild, &game, game.latest()).await?;
    i.create_response(&ctx.http, status.update()).await?;
    Ok(())
}

async fn claim(ctx: &BotCtx, i: &ComponentInteraction, guild: GuildId, round: i64) -> Result<()> {
    let user = i.user.id.get();
    let (game, claimed) = ctx
        .db
        .call(move |conn| {
            let tx = conn.transaction()?;
            let game = open_round(&tx, guild.get(), round)?;
            let claimed = store::claim(&tx, round, user)?;
            let season = store::load_season(&tx, game.season.id)?;
            tx.commit()?;
            Ok((
                Game {
                    season,
                    active: true,
                },
                claimed,
            ))
        })
        .await?;
    if !claimed {
        return Err(user_error("You've already claimed this round!"));
    }
    info!("claimed");

    let status = status_message(ctx, guild, &game, game.latest()).await?;
    i.create_response(&ctx.http, status.update()).await?;
    let followup = CreateInteractionResponseFollowup::new()
        .content(claimed_text(&game, user))
        .ephemeral(true);
    i.create_followup(&ctx.http, followup).await?;
    Ok(())
}

/// Place Bet, Remove Bet and Set Winner: a private menu to pick a user.
async fn open_menu(
    ctx: &BotCtx,
    i: &ComponentInteraction,
    guild: GuildId,
    kind: PickKind,
    round: i64,
) -> Result<()> {
    if kind == PickKind::Winner {
        require_admin(ctx, i.user.id, "pick winners")?;
    }
    let game = ctx
        .db
        .call(move |conn| open_round(conn, guild.get(), round))
        .await?;
    let numbers = ledger(&game.season);
    let current = numbers.last().context("a season has rounds")?;
    let user = i.user.id.get();

    let choices: Vec<u64> = match kind {
        PickKind::Place => {
            if current.money(user) <= 0 {
                return Err(user_error(format!(
                    "You have nothing to bet yet. Press Claim! for {CLAIM}."
                )));
            }
            current.options_left.clone()
        }
        PickKind::Remove => game.season.rounds[game.latest()]
            .bets
            .iter()
            .filter(|b| b.by == user)
            .map(|b| b.on)
            .collect(),
        PickKind::Winner => current.options_left.clone(),
    };
    if choices.is_empty() {
        return Err(user_error(match kind {
            PickKind::Remove => "You don't have any bets in this round.",
            _ => "Nobody is on the wheel yet. Admins add options with /wheel_add.",
        }));
    }

    let names = names::lookup(ctx, guild, &choices).await;
    let (content, menu) = ui::pick_menu(kind, round, i.message.id.get(), &choices, &names);
    let response = CreateInteractionResponseMessage::new()
        .content(content)
        .components(menu)
        .ephemeral(true);
    i.create_response(&ctx.http, CreateInteractionResponse::Message(response))
        .await?;
    Ok(())
}

/// A user was picked in one of the menus.
async fn picked(
    ctx: &BotCtx,
    i: &ComponentInteraction,
    guild: GuildId,
    kind: PickKind,
    round: i64,
    message: MessageId,
) -> Result<()> {
    let ComponentInteractionDataKind::StringSelect { values } = &i.data.kind else {
        bail!("the menu sent no values");
    };
    let on: u64 = values.first().context("nothing selected")?.parse()?;
    let user = i.user.id.get();
    match kind {
        PickKind::Place => {
            // The amount comes from a modal, which must be the first response, so only read
            // here; the bet is checked again when the amount arrives.
            let game = ctx
                .db
                .call(move |conn| open_round(conn, guild.get(), round))
                .await?;
            let numbers = ledger(&game.season);
            let usable = numbers
                .last()
                .and_then(|n| n.standing(user))
                .map_or(0, |s| s.usable());
            let existing = game.season.rounds[game.latest()]
                .bets
                .iter()
                .find(|b| b.by == user && b.on == on)
                .map_or(0, |b| b.amount);
            let names = names::lookup(ctx, guild, &[on]).await;
            let modal = ui::amount_modal(
                round,
                on,
                message.get(),
                ui::name(&names, on),
                usable,
                existing,
            );
            i.create_response(&ctx.http, CreateInteractionResponse::Modal(modal))
                .await?;
        }
        PickKind::Remove => {
            let game = ctx
                .db
                .call(move |conn| {
                    let tx = conn.transaction()?;
                    let game = open_round(&tx, guild.get(), round)?;
                    if !store::remove_bet(&tx, round, user, on)? {
                        return Err(user_error("That bet is already gone."));
                    }
                    let season = store::load_season(&tx, game.season.id)?;
                    tx.commit()?;
                    Ok(Game {
                        season,
                        active: true,
                    })
                })
                .await?;
            info!("removed the bet on {on}");
            let names = names::lookup(ctx, guild, &[on]).await;
            let text = format!("Removed your bet on {}.", ui::name(&names, on));
            i.create_response(&ctx.http, update_text(text)).await?;
            edit_status(ctx, i.channel_id, message, guild, &game, game.latest()).await;
        }
        PickKind::Winner => {
            require_admin(ctx, i.user.id, "pick winners")?;
            let game = ctx
                .db
                .call(move |conn| {
                    let tx = conn.transaction()?;
                    let game = open_round(&tx, guild.get(), round)?;
                    let numbers = ledger(&game.season);
                    if !numbers.last().is_some_and(|n| n.options_left.contains(&on)) {
                        return Err(user_error("That option isn't on the wheel any more."));
                    }
                    store::set_winner(&tx, round, on, now())?;
                    let season = store::load_season(&tx, game.season.id)?;
                    tx.commit()?;
                    Ok(Game {
                        season,
                        active: true,
                    })
                })
                .await?;
            // Show the round that just ended, with the button to the new one.
            let resolved = game.latest() - 1;
            let number = game.season.rounds[resolved].number;
            info!("round {number} won by {on}");
            ctx.publish(BotEvent::WheelRoundResolved {
                guild_id: guild,
                round: number,
                winner: UserId::new(on),
            });
            let names = names::lookup(ctx, guild, &[on]).await;
            let text = format!(
                "Set {} as the winner of round {number}.",
                ui::name(&names, on)
            );
            i.create_response(&ctx.http, update_text(text)).await?;
            edit_status(ctx, i.channel_id, message, guild, &game, resolved).await;
        }
    }
    Ok(())
}

/// "Undo Winner" on a resolved round.
async fn undo(ctx: &BotCtx, i: &ComponentInteraction, guild: GuildId, round: i64) -> Result<()> {
    require_admin(ctx, i.user.id, "undo a winner")?;
    let game = ctx
        .db
        .call(move |conn| {
            let tx = conn.transaction()?;
            let game = active_game(&tx, guild.get())?;
            let index = game
                .season
                .rounds
                .iter()
                .position(|r| r.id == round)
                .ok_or_else(|| user_error("This round is from a past season."))?;
            if !ledger::can_undo(&game.season, index) {
                return Err(user_error(
                    "Only the latest winner can be undone, and only while nobody has bet in the new round.",
                ));
            }
            store::undo_winner(&tx, round)?;
            let season = store::load_season(&tx, game.season.id)?;
            tx.commit()?;
            Ok(Game {
                season,
                active: true,
            })
        })
        .await?;
    info!(
        "undid the winner of round {}",
        game.season.rounds[game.latest()].number
    );
    let status = status_message(ctx, guild, &game, game.latest()).await?;
    i.create_response(&ctx.http, status.update()).await?;
    Ok(())
}

/// The confirm button of `/reset_wheel`.
async fn reset(
    ctx: &BotCtx,
    i: &ComponentInteraction,
    guild: GuildId,
    keep_options: bool,
) -> Result<()> {
    require_admin(ctx, i.user.id, "start a new season")?;
    let number = ctx
        .db
        .call(move |conn| {
            let tx = conn.transaction()?;
            let mut options = Vec::new();
            if let Some(old) = store::active_season(&tx, guild.get())? {
                if keep_options {
                    options = store::load_season(&tx, old)?.options;
                }
                store::end_season(&tx, old, now())?;
            }
            store::start_season(&tx, guild.get(), &options, now())?;
            let number = store::seasons(&tx, guild.get())?.len();
            tx.commit()?;
            Ok(number)
        })
        .await?;
    info!("started season {number}");
    let text = format!("Started season {number}. Show it with /wheel_status.");
    i.create_response(&ctx.http, update_text(text)).await?;
    Ok(())
}

/// The private note after Claim!: what the player has now and the bet that avoids the tax.
fn claimed_text(game: &Game, user: u64) -> String {
    let numbers = ledger(&game.season);
    match numbers.last().and_then(|n| n.standing(user)) {
        Some(s) => format!(
            "Claimed {CLAIM}! You have {} this round. Bet at least {} ({TAX_THRESHOLD}%) to avoid the tax.",
            s.money,
            s.safe_bet()
        ),
        None => format!("Claimed {CLAIM}!"),
    }
}

/// Replaces a private menu with a line of text.
fn update_text(text: String) -> CreateInteractionResponse {
    CreateInteractionResponse::UpdateMessage(
        CreateInteractionResponseMessage::new()
            .content(text)
            .components(Vec::new()),
    )
}

/// Shows round `index` on the public status message after a change made from a private
/// menu. Failing isn't an error, for example when the message was deleted: the change is
/// saved either way.
async fn edit_status(
    ctx: &BotCtx,
    channel: ChannelId,
    message: MessageId,
    guild: GuildId,
    game: &Game,
    index: usize,
) {
    let edit = match status_message(ctx, guild, game, index).await {
        Ok(status) => status.edit(),
        Err(err) => {
            warn!("couldn't draw the wheel status: {err:#}");
            return;
        }
    };
    if let Err(err) = channel.edit_message(&ctx.http, message, edit).await {
        warn!("couldn't update the wheel status message: {err}");
    }
}

/// The text typed into the bet modal.
fn modal_value(i: &ModalInteraction) -> Option<String> {
    i.data
        .components
        .iter()
        .flat_map(|row| &row.components)
        .find_map(|component| match component {
            ActionRowComponent::InputText(input) => input.value.clone(),
            _ => None,
        })
}
