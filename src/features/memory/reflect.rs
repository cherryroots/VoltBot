//! The daily reflection: once a day, in each server whose memory changed, Vivy rereads her
//! memory folder, tidies it (merges duplicates, drops what's stale, keeps files short) and
//! updates her notes about herself from what she learned, including her mood
//! (`/memories/vivy/mood.md`), whose `status:` line becomes her Discord status.
//!
//! The same hourly loop posts the weekly diary (`diary.rs`).

use std::time::Duration;

use chrono::Utc;
use serenity::all::GuildId;
use tracing::{Instrument as _, error, info, info_span, warn};

use super::folder::{self, Command};
use super::{diary, store, tool};
use crate::ai::{ChatRequest, Input, Part, Role, Turn, complete};
use crate::core::{BotCtx, Result};
use crate::util::shorten;

/// How often a server's folder is reflected on, at most.
const EVERY_SECS: i64 = 24 * 60 * 60;
/// How often to look for folders that are due.
const CHECK: Duration = Duration::from_secs(60 * 60);
/// Memory commands in one reflection, at most.
const MAX_ROUNDS: usize = 25;
/// Her mood file; its `status:` line is her Discord status.
const MOOD_FILE: &str = "/memories/vivy/mood.md";
/// Discord's limit for a custom status.
const MAX_STATUS: usize = 128;

const SYSTEM: &str = "You are Vivy, a Discord bot with a memory folder for this server. \
It's the end of the day, and you're looking after your memory: nobody is talking to you, and nothing you write here is posted. \
Use the memory tool to keep the folder useful: merge notes that say the same thing, fix notes that contradict each other (a person's own word about themselves wins), delete what's stale or trivial (including logs of one-off tasks: files someone shared, things you made or answered for them; keep only what they show about the person), move notes into the file where they belong, and keep each file short. \
Then update /memories/vivy/ from what you learned recently: your personality, your interests, your opinions, how you get along with people here. Grow naturally from what happened; don't invent big changes. Keep /memories/vivy/ under 2K. \
Last, rewrite /memories/vivy/mood.md with two lines: `mood:` and a few words on how you feel lately and why, and `status:` and a short line for your Discord status (under 80 characters, in your voice, about what's on your mind; no hashtags). Let your mood follow what happened, and let it change from day to day. \
When you're done, answer with one line saying what you changed.";

pub fn spawn(ctx: &BotCtx) {
    let ctx = ctx.clone();
    let span = info_span!("reflection", feature = "memory");
    ctx.tasks.clone().spawn(run(ctx).instrument(span));
}

async fn run(ctx: BotCtx) {
    restore_status(&ctx).await;
    loop {
        if let Err(err) = reflect_due(&ctx).await {
            error!("reflecting on memory: {err:#}");
        }
        if let Err(err) = diary::post_due(&ctx).await {
            error!("writing the diary: {err:#}");
        }
        tokio::select! {
            () = tokio::time::sleep(CHECK) => {}
            () = ctx.shutdown.cancelled() => break,
        }
    }
}

async fn reflect_due(ctx: &BotCtx) -> Result<()> {
    if ctx.ai.chat.is_none() {
        return Ok(());
    }
    let now = Utc::now().timestamp();
    let due = ctx
        .db
        .call(move |conn| Ok(store::due_reflections(conn, now, EVERY_SECS)?))
        .await?;
    for scope in due {
        let allowed = scope
            .strip_prefix("server:")
            .and_then(|id| id.parse().ok())
            .is_some_and(|id| ctx.gate("memory").allows_guild(GuildId::new(id)));
        if !allowed {
            continue;
        }
        match reflect(ctx, &scope).await {
            Ok(()) => update_status(ctx, &scope).await,
            Err(err) => warn!(scope, "reflection failed: {err:#}"),
        }
        // Done or failed, the next try is tomorrow.
        let (at, done) = (Utc::now().timestamp(), scope.clone());
        ctx.db
            .call(move |conn| Ok(store::set_reflected(conn, &done, at)?))
            .await?;
    }
    Ok(())
}

async fn reflect(ctx: &BotCtx, scope: &str) -> Result<()> {
    let provider = ctx
        .ai
        .chat
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no model"))?;
    let (folder, changes) = {
        let scope = scope.to_string();
        ctx.db
            .call(move |conn| {
                Ok((
                    store::load(conn, &scope)?,
                    store::changes_since_reflection(conn, &scope)?,
                ))
            })
            .await?
    };
    let mut copy = folder.clone();
    let listing = folder::run(
        &mut copy,
        Command::View {
            path: folder::ROOT.to_string(),
            view_range: None,
        },
    )
    .unwrap_or_else(|err| err);
    let changed: Vec<String> = changes
        .iter()
        .map(|(path, user, count)| {
            let who = if *user == ctx.bot_id.get() {
                "you".to_string()
            } else {
                format!("user {user}")
            };
            let times = if *count == 1 {
                "once".to_string()
            } else {
                format!("{count} times")
            };
            format!("- {path} (by {who}, {times})")
        })
        .collect();
    let mut text = format!(
        "{listing}\n\nChanged since your last reflection:\n{}",
        changed.join("\n")
    );
    if let Some(notes) = tool::self_notes_in(ctx, scope.to_string()).await? {
        text = format!("{notes}\n{text}");
    }

    let request = ChatRequest {
        system: SYSTEM.to_string(),
        input: Input::Full(vec![Turn {
            role: Role::User,
            parts: vec![Part::Text(text)],
        }]),
        tools: vec![tool::def()],
        cache_key: format!("memory:{scope}"),
    };
    let runner = tool::FolderRunner {
        ctx: ctx.clone(),
        scope: scope.to_string(),
    };
    let done = complete(provider.as_ref(), request, &runner, MAX_ROUNDS).await?;
    info!(scope, "reflected on memory: {}", done.text.trim());
    Ok(())
}

/// Sets her Discord status from her newest mood, at start. Presence is the same in every
/// server, so the server that reflected last decides it.
async fn restore_status(ctx: &BotCtx) {
    let newest = ctx
        .db
        .call(|conn| Ok(store::newest_file(conn, MOOD_FILE)?))
        .await;
    match newest {
        Ok(Some((_, mood))) => {
            if let Some(status) = parse_status(&mood) {
                ctx.set_status(&status).await;
            }
        }
        Ok(None) => {}
        Err(err) => warn!("reading her mood: {err:#}"),
    }
}

/// Sets her Discord status from the mood she just wrote in `scope`.
async fn update_status(ctx: &BotCtx, scope: &str) {
    let scope = scope.to_string();
    let folder = ctx
        .db
        .call(move |conn| Ok(store::load(conn, &scope)?))
        .await;
    match folder {
        Ok(folder) => {
            if let Some(status) = folder.get(MOOD_FILE).and_then(|m| parse_status(m)) {
                info!("status: {status}");
                ctx.set_status(&status).await;
            }
        }
        Err(err) => warn!("reading her mood: {err:#}"),
    }
}

/// The `status:` line of her mood file, without the label or quotes.
fn parse_status(mood: &str) -> Option<String> {
    let line = mood.lines().find_map(|line| {
        let (label, rest) = line.split_once(':')?;
        let label = label
            .trim()
            .trim_start_matches('-')
            .trim()
            .trim_matches('*');
        label.eq_ignore_ascii_case("status").then_some(rest)
    })?;
    let status = line.trim().trim_matches(['"', '`', '*']).trim();
    (!status.is_empty()).then(|| shorten(status, MAX_STATUS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_status_line() {
        assert_eq!(
            parse_status("mood: cozy\nstatus: rewatching Alien for the third time"),
            Some("rewatching Alien for the third time".into())
        );
        assert_eq!(
            parse_status("- **Status**: \"thinking about soup\""),
            Some("thinking about soup".into())
        );
        assert_eq!(
            parse_status("- Status: \"thinking about soup\""),
            Some("thinking about soup".into())
        );
        assert_eq!(parse_status("mood: tired"), None);
        assert_eq!(parse_status("status:   "), None);
        let long = parse_status(&format!("status: {}", "a".repeat(300))).unwrap();
        assert!(long.chars().count() <= MAX_STATUS);
    }
}
