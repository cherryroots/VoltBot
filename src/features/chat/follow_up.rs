//! Follow-ups: when someone mentions something coming up ("job interview friday"), Vivy can
//! plan to check in later. When it's due she reads the channel and, if it still fits, asks
//! them how it went.
//!
//! One background task delivers them: it sleeps until the next one is due (at most an hour),
//! and planning a new one wakes it.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildId, MessageId, UserId};
use tokio::sync::Notify;
use tracing::{Instrument as _, error, info, info_span, warn};

use super::chime;
use super::store::{self, FollowUp};
use crate::ai::ToolDef;
use crate::core::{Asker, BotCtx, Result, user_error};

/// Check-ins planned with one person at a time, at most.
const MAX_PER_PERSON: i64 = 5;
/// The latest a check-in can be, in hours (60 days).
const MAX_HOURS: f64 = 60.0 * 24.0;

pub fn def() -> ToolDef {
    ToolDef {
        name: "schedule_follow_up",
        description: "Plans for you to check in with someone later, in this channel. Use it on your own when someone mentions something coming up that a friend would ask about afterwards (an interview, a trip, an exam, a vet visit, a date), timed for after it happens. Don't announce that you scheduled it.",
        parameters: json!({
            "type": "object",
            "properties": {
                "hours_from_now": {
                    "type": "number",
                    "description": "When to check in, in hours from now. Use get_current_time to work it out for a day or time."
                },
                "note": {
                    "type": "string",
                    "description": "What to ask about, with enough detail to remember it later: \"Alice's job interview at the bakery on Friday\"."
                },
                "user": {
                    "type": "string",
                    "description": "The user ID of the person to check in with. Default: the asker."
                }
            },
            "required": ["hours_from_now", "note"]
        }),
    }
}

/// Plans a check-in for the asker (or `user`). Wakes the delivery task.
pub async fn schedule(ctx: &BotCtx, asker: &Asker, args: &Value, wake: &Notify) -> Result<String> {
    let hours = args["hours_from_now"]
        .as_f64()
        .ok_or_else(|| user_error("`hours_from_now` must be a number."))?;
    if !(0.1..=MAX_HOURS).contains(&hours) {
        return Err(user_error(format!(
            "`hours_from_now` must be between 0.1 and {MAX_HOURS}."
        )));
    }
    let note = args["note"].as_str().unwrap_or_default().trim().to_string();
    if note.is_empty() {
        return Err(user_error("`note` is empty."));
    }
    let user = match args["user"].as_str().map(str::trim) {
        Some(id) if !id.is_empty() => id
            .trim_start_matches("<@")
            .trim_end_matches('>')
            .parse::<u64>()
            .map_err(|_| user_error("`user` must be a user ID."))?,
        _ => asker.user.get(),
    };
    let now = Utc::now().timestamp();
    let follow_up = FollowUp {
        id: 0,
        guild_id: asker.guild.map(|g| g.get()),
        channel_id: asker.channel.get(),
        user_id: user,
        message_id: asker.message.get(),
        note,
        due_at: now + (hours * 3600.0) as i64,
    };
    let due_at = follow_up.due_at;
    ctx.db
        .call(move |conn| {
            if store::pending_follow_ups(conn, user)? >= MAX_PER_PERSON {
                return Err(user_error(format!(
                    "You already have {MAX_PER_PERSON} check-ins planned with this person."
                )));
            }
            Ok(store::add_follow_up(conn, &follow_up, now)?)
        })
        .await?;
    wake.notify_one();
    Ok(format!(
        "Planned. You'll check in around <t:{due_at}:f>. Don't mention it unless asked."
    ))
}

pub fn spawn(ctx: &BotCtx, wake: Arc<Notify>) {
    let ctx = ctx.clone();
    let span = info_span!("follow-ups", feature = "chat");
    ctx.tasks.clone().spawn(run(ctx, wake).instrument(span));
}

async fn run(ctx: BotCtx, wake: Arc<Notify>) {
    loop {
        if let Err(err) = deliver_due(&ctx).await {
            error!("delivering follow-ups: {err:#}");
        }
        let next = ctx
            .db
            .call(|conn| Ok(store::next_follow_up(conn)?))
            .await
            .unwrap_or_else(|err| {
                error!("finding the next follow-up: {err:#}");
                None
            });
        let wait = match next {
            Some(at) => (at - Utc::now().timestamp()).clamp(1, 3600),
            None => 3600,
        };
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(wait as u64)) => {}
            () = wake.notified() => {}
            () = ctx.shutdown.cancelled() => break,
        }
    }
}

async fn deliver_due(ctx: &BotCtx) -> Result<()> {
    let now = Utc::now().timestamp();
    let due = ctx
        .db
        .call(move |conn| Ok(store::take_due_follow_ups(conn, now)?))
        .await?;
    for follow_up in due {
        let id = follow_up.id;
        // Each is tried once: a check-in that fails isn't worth repeating later.
        if let Err(err) = deliver(ctx, &follow_up).await {
            warn!(follow_up = id, "couldn't check in: {err:#}");
        }
    }
    Ok(())
}

async fn deliver(ctx: &BotCtx, follow_up: &FollowUp) -> Result<()> {
    let asker = Asker {
        user: UserId::new(follow_up.user_id),
        guild: follow_up.guild_id.map(GuildId::new),
        channel: ChannelId::new(follow_up.channel_id),
        message: MessageId::new(follow_up.message_id),
    };
    let user = follow_up.user_id;
    let instructions = format!(
        "Earlier you planned to check in with <@{user}> about: {}. It's time. \
If the recent messages show it was already talked about, or it no longer fits, answer PASS. \
Otherwise write one short, warm message to them the way a friend would on Discord, starting with <@{user}>.",
        follow_up.note
    );
    info!(follow_up = follow_up.id, "checking in");
    chime::speak(ctx, &asker, None, &instructions, Some(asker.user)).await
}
