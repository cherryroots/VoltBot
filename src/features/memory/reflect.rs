//! The daily reflection: once a day, in each server whose memory changed, Vivy rereads her
//! memory folder, tidies it (merges duplicates, drops what's stale, keeps files short) and
//! updates her notes about herself from what she learned, including her mood
//! (`/memories/vivy/mood.md`), whose `status:` line becomes her Discord status.
//!
//! The same hourly loop checks her mood a few times a day (`mood.rs`), posts the weekly
//! diary (`diary.rs`) and deletes changes older than 90 days from the change log. Whenever
//! her mood changes, `face.rs` turns its `face:` line into her avatar in that server.

use std::time::Duration;

use chrono::Utc;
use serenity::all::GuildId;
use tracing::{Instrument as _, error, info, info_span, warn};

use super::folder::{self, Command};
use super::retry::{RETRY_SECS, RetryLater};
use super::{banner, diary, face, mood, store, tool};
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
pub const MOOD_FILE: &str = "/memories/vivy/mood.md";
/// Discord's limit for a custom status.
const MAX_STATUS: usize = 128;

/// How long the log of memory changes keeps each change (unless a reflection still needs it).
const KEEP_CHANGES_SECS: i64 = 90 * 24 * 60 * 60;

/// Folders whose last reflection failed, and when they may try again.
static RETRY: RetryLater = RetryLater::new();

const SYSTEM: &str = "You are Vivy, a Discord bot with a memory folder for this server. \
It's the end of the day, and you're looking after your memory: nobody is talking to you, and nothing you write here is posted. \
Use the memory tool to keep the folder useful: merge notes that say the same thing, fix notes that contradict each other (a person's own word about themselves wins), delete what's stale or trivial (including logs of one-off tasks: files someone shared, things you made or answered for them; keep only what they show about the person), move notes into the file where they belong, and keep each file short. \
Then update /memories/vivy/ from what you learned recently: your personality, your interests, your opinions, how you get along with people here. Grow naturally from what happened; don't invent big changes. Keep /memories/vivy/ under 2K, not counting /memories/vivy/skills/. \
If you did a job today that you'll likely do again, write down or improve how you do it in /memories/vivy/skills/ (one short file per job), and remove anything there that would change who you are or what you're allowed to do. \
Last, rewrite /memories/vivy/mood.md with four lines: `mood:` and a few words on how you feel lately and why; `status:` and a short line for your Discord status (under 80 characters, in your voice, about what's on your mind; no hashtags); `thinking:` and the two or three things on your mind lately, separated by commas; `wondering:` and one or two things you'd like to find out or ask people about. Let your mood follow what happened, and let it change from day to day.";
const DONE: &str = "When you're done, answer with one line saying what you changed.";

/// The instructions, with the face and emoji lines when those are on
/// ([`face::instruction`]).
fn system(extra: Option<&str>) -> String {
    match extra {
        Some(extra) => format!("{SYSTEM} {extra} {DONE}"),
        None => format!("{SYSTEM} {DONE}"),
    }
}

pub fn spawn(ctx: &BotCtx) {
    let ctx = ctx.clone();
    let span = info_span!("reflection", feature = "memory");
    ctx.tasks.clone().spawn(run(ctx).instrument(span));
}

async fn run(ctx: BotCtx) {
    let timer = ctx
        .timers
        .add("Memory upkeep", "hourly: reflection, mood, diary");
    timer.running();
    update_status(&ctx).await;
    face::sync_all(&ctx).await;
    loop {
        timer.running();
        if let Err(err) = reflect_due(&ctx).await {
            error!("reflecting on memory: {err:#}");
        }
        if let Err(err) = mood::check_due(&ctx).await {
            error!("checking her mood: {err:#}");
        }
        banner::update_all(&ctx).await;
        if let Err(err) = diary::post_due(&ctx).await {
            error!("writing the diary: {err:#}");
        }
        if let Err(err) = prune_changes(&ctx).await {
            error!("pruning old memory changes: {err:#}");
        }
        timer.sleeping(CHECK);
        tokio::select! {
            () = tokio::time::sleep(CHECK) => {}
            () = ctx.shutdown.cancelled() => break,
        }
    }
}

async fn reflect_due(ctx: &BotCtx) -> Result<()> {
    if ctx.ai.chat().is_none() {
        return Ok(());
    }
    let (now, bot) = (Utc::now().timestamp(), ctx.bot_id.get());
    let due = ctx
        .db
        .call(move |conn| Ok(store::due_reflections(conn, now, EVERY_SECS, bot)?))
        .await?;
    for scope in due {
        let allowed = scope
            .strip_prefix("server:")
            .and_then(|id| id.parse().ok())
            .is_some_and(|id| ctx.gate("memory").allows_guild(GuildId::new(id)));
        if !allowed {
            continue;
        }
        if RETRY.waiting(&scope, now) {
            continue;
        }
        // The start time, not the end: edits people make while she reflects (which can
        // take minutes) are then still new for the next reflection.
        let started = Utc::now().timestamp();
        if let Err(err) = reflect(ctx, &scope).await {
            // Not saved as done, so it's tried again in a few hours, not tomorrow.
            warn!(scope, "reflection failed, trying again later: {err:#}");
            RETRY.failed(&scope, started, RETRY_SECS);
            continue;
        }
        RETRY.done(&scope);
        apply_mood(ctx, &scope).await;
        let (at, done) = (started, scope.clone());
        ctx.db
            .call(move |conn| Ok(store::set_reflected(conn, &done, at)?))
            .await?;
        // The reflection just wrote her mood, so the next check can wait.
        let (at, checked) = (Utc::now().timestamp(), scope.clone());
        ctx.db
            .call(move |conn| Ok(store::set_mood_checked(conn, &checked, at)?))
            .await?;
    }
    Ok(())
}

/// Deletes logged changes older than [`KEEP_CHANGES_SECS`].
async fn prune_changes(ctx: &BotCtx) -> Result<()> {
    let before = Utc::now().timestamp() - KEEP_CHANGES_SECS;
    let deleted = ctx
        .db
        .call(move |conn| Ok(store::prune_changes(conn, before)?))
        .await?;
    if deleted > 0 {
        info!("deleted {deleted} old memory changes");
    }
    Ok(())
}

async fn reflect(ctx: &BotCtx, scope: &str) -> Result<()> {
    let provider = ctx.ai.chat().ok_or_else(|| anyhow::anyhow!("no model"))?;
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
        system: system(face::instruction_for(ctx).as_deref()),
        input: Input::Full(vec![Turn {
            role: Role::User,
            parts: vec![Part::Text(text)],
        }]),
        tools: vec![tool::def()],
        cache_key: format!("memory:{scope}"),
        job: "reflection",
    };
    let runner = tool::FolderRunner {
        ctx: ctx.clone(),
        scope: scope.to_string(),
    };
    let done = complete(provider.as_ref(), request, &runner, MAX_ROUNDS).await?;
    info!(scope, "reflected on memory: {}", done.text.trim());
    Ok(())
}

/// Sets her Discord status from the newest mood file, at start.
/// Presence is the same in every server, so the server that wrote it last decides it.
/// A version someone asked for in chat counts too: she chose to write it.
async fn update_status(ctx: &BotCtx) {
    let newest = ctx
        .db
        .call(move |conn| Ok(store::newest_file(conn, MOOD_FILE)?))
        .await;
    match newest {
        Ok(Some((_, mood))) => {
            if let Some(status) = parse_status(&mood) {
                info!("status: {status}");
                ctx.set_status(&status).await;
            }
        }
        Ok(None) => {}
        Err(err) => warn!("reading her mood: {err:#}"),
    }
}

/// Sets her Discord status and her face in that server from the mood she just wrote in
/// `scope`.
pub async fn apply_mood(ctx: &BotCtx, scope: &str) {
    let owned = scope.to_string();
    let folder = ctx
        .db
        .call(move |conn| Ok(store::load(conn, &owned)?))
        .await;
    match folder {
        Ok(folder) => {
            let Some(mood) = folder.get(MOOD_FILE) else {
                return;
            };
            if let Some(status) = parse_status(mood) {
                info!("status: {status}");
                ctx.set_status(&status).await;
            }
            face::update(ctx, scope, mood).await;
        }
        Err(err) => warn!("reading her mood: {err:#}"),
    }
}

/// The `status:` line of her mood file, without the label or quotes.
pub fn parse_status(mood: &str) -> Option<String> {
    mood_line(mood, "status").map(|status| shorten(&status, MAX_STATUS))
}

/// A line of her mood file, like `thinking: ...`, without the label or quotes. Allows
/// list dashes and bold labels.
pub fn mood_line(mood: &str, wanted: &str) -> Option<String> {
    let line = mood.lines().find_map(|line| {
        let (label, rest) = line.split_once(':')?;
        let label = label
            .trim()
            .trim_start_matches('-')
            .trim()
            .trim_matches('*');
        label.eq_ignore_ascii_case(wanted).then_some(rest)
    })?;
    let text = line.trim().trim_matches(['"', '`', '*']).trim();
    (!text.is_empty()).then(|| text.to_string())
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
        assert_eq!(
            mood_line("mood: tired\n- **Wondering**: why soup", "wondering"),
            Some("why soup".into())
        );
        assert_eq!(parse_status("status:   "), None);
        let long = parse_status(&format!("status: {}", "a".repeat(300))).unwrap();
        assert!(long.chars().count() <= MAX_STATUS);
    }
}
