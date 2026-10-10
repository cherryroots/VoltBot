//! Mood checks: a few times a day, in each server where she has a mood, Vivy rewrites
//! `/memories/vivy/mood.md` for the moment. She gets the local time, so her mood follows
//! the day (sleepy late at night, slow in the morning), and what changed in her memory
//! since the last check. The daily reflection rewrites the mood too and counts as a check.
//!
//! One short model call per check, with no tools: she answers with the new file.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use serenity::all::GuildId;
use tracing::{info, warn};

use super::{face, reflect, store, tool};
use crate::ai::{ChatRequest, Input, Part, Role, ToolCall, ToolRunner, Turn, complete};
use crate::core::{BotCtx, Result};

const SYSTEM: &str = "You are Vivy, a Discord bot. Nobody is talking to you right now: this is a quiet moment where you check in with yourself, and nothing you write is posted. \
You get the time where you live, your notes about yourself, your mood file as it is, and what changed in your memory since you last checked. \
Write your mood file for right now. This check is about your body and the time of day, the way they'd shape a person's mood: waking up slowly in the morning, getting hungry around mealtimes, an afternoon slump, winding down in the evening, sleepy late at night. Let it drift from the last one rather than jump. Conversations change your mood through the set_mood tool, so keep a mood that came from people (the reason in the mood line) unless enough time has passed for it to fade. Your mood is your own: someone asking or telling you to feel a certain way doesn't decide it. \
The file has four lines: `mood:` and a few words on how you feel and why; `status:` and a short line for your Discord status (under 80 characters, in your voice, about what's on your mind; no hashtags); `thinking:` and the two or three things on your mind, separated by commas; `wondering:` and one or two things you'd like to find out or ask people about.";
const ANSWER: &str = "Answer with only the new file, nothing before or after it.";

/// `[features.memory]` settings for mood checks.
#[derive(Debug, Deserialize)]
#[serde(default)]
struct Settings {
    /// How often a day her mood is checked in each server. 0 turns checks off (the daily
    /// reflection still updates it).
    mood_checks_per_day: u32,
    /// Where she lives, for the time of day: a name like "Europe/Stockholm".
    timezone: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mood_checks_per_day: 5,
            timezone: "UTC".to_string(),
        }
    }
}

/// Where she lives (`timezone`), or UTC when it isn't set or isn't a known name.
pub fn zone(ctx: &BotCtx) -> Tz {
    let settings: Settings = ctx.config.feature("memory").unwrap_or_default();
    settings.timezone.parse().unwrap_or_else(|_| {
        warn!(timezone = settings.timezone, "unknown timezone, using UTC");
        Tz::UTC
    })
}

/// The instructions, with the faces she can pick from when there are any.
fn system(faces: &[String]) -> String {
    match face::instruction(faces) {
        Some(faces) => format!("{SYSTEM} {faces} {ANSWER}"),
        None => format!("{SYSTEM} {ANSWER}"),
    }
}

/// Checks her mood in every server where the last check is old enough.
pub async fn check_due(ctx: &BotCtx) -> Result<()> {
    if ctx.ai.chat().is_none() {
        return Ok(());
    }
    let settings: Settings = ctx.config.feature("memory")?;
    if settings.mood_checks_per_day == 0 {
        return Ok(());
    }
    let zone = zone(ctx);
    let every = 86_400 / i64::from(settings.mood_checks_per_day);
    let now = Utc::now().timestamp();

    let moods = ctx
        .db
        .call(|conn| {
            let moods = store::every_file(conn, reflect::MOOD_FILE)?;
            let mut checked = HashMap::new();
            for (scope, _) in &moods {
                checked.insert(scope.clone(), store::mood_checked_at(conn, scope)?);
            }
            Ok((moods, checked))
        })
        .await?;
    let (moods, checked) = moods;
    for (scope, mood) in moods {
        let last = checked.get(&scope).copied().flatten();
        // A little early, so the hourly loop doesn't push each check an hour later.
        if last.is_some_and(|at| at > now - every + 600) {
            continue;
        }
        let allowed = scope
            .strip_prefix("server:")
            .and_then(|id| id.parse().ok())
            .is_some_and(|id| ctx.gate("memory").allows_guild(GuildId::new(id)));
        if !allowed {
            continue;
        }
        match check(ctx, &scope, &mood, last.unwrap_or(now - every), zone).await {
            Ok(true) => reflect::apply_mood(ctx, &scope).await,
            Ok(false) => {}
            Err(err) => warn!(scope, "mood check failed: {err:#}"),
        }
        // Done or failed, the next try is after the next interval.
        let (at, done) = (Utc::now().timestamp(), scope.clone());
        ctx.db
            .call(move |conn| Ok(store::set_mood_checked(conn, &done, at)?))
            .await?;
    }
    Ok(())
}

/// Rewrites her mood in `scope`. Returns whether it was saved.
async fn check(ctx: &BotCtx, scope: &str, mood: &str, since: i64, zone: Tz) -> Result<bool> {
    let provider = ctx.ai.chat().ok_or_else(|| anyhow::anyhow!("no model"))?;
    let changed = {
        let scope = scope.to_string();
        ctx.db
            .call(move |conn| Ok(store::changed_since(conn, &scope, since)?))
            .await?
    };
    let notes = tool::self_notes_in(ctx, scope.to_string()).await?;
    let text = input(Utc::now(), zone, notes.as_deref(), mood, &changed);

    let request = ChatRequest {
        system: system(&face::names(ctx)),
        input: Input::Full(vec![Turn {
            role: Role::User,
            parts: vec![Part::Text(text)],
        }]),
        tools: Vec::new(),
        cache_key: format!("mood:{scope}"),
        job: "mood",
    };
    let done = complete(provider.as_ref(), request, &NoTools, 1).await?;
    let Some(new) = clean(&done.text) else {
        warn!(
            scope,
            "mood check gave no status line: {}",
            done.text.trim()
        );
        return Ok(false);
    };

    let (owned, bot, old) = (scope.to_string(), ctx.bot_id.get(), mood.to_string());
    let now = Utc::now().timestamp();
    let saved = ctx
        .db
        .call(move |conn| {
            let tx = conn.transaction()?;
            let before = store::load(&tx, &owned)?;
            // A conversation changed her mood (set_mood) while she was thinking: that one
            // is newer, so it stays.
            if before.get(reflect::MOOD_FILE) != Some(&old) {
                return Ok(false);
            }
            let mut after = before.clone();
            after.insert(reflect::MOOD_FILE.to_string(), new);
            store::save(&tx, &owned, &before, &after, bot, now)?;
            tx.commit()?;
            Ok(true)
        })
        .await?;
    if saved {
        info!(scope, "checked her mood");
    } else {
        info!(
            scope,
            "her mood changed during the check, keeping the newer one"
        );
    }
    Ok(saved)
}

/// What she's told: the time, her notes, her mood and what changed.
fn input(
    now: DateTime<Utc>,
    zone: Tz,
    notes: Option<&str>,
    mood: &str,
    changed: &[(String, Option<String>)],
) -> String {
    let local = now.with_timezone(&zone);
    let mut text = format!(
        "It's {} where you live.\n\n",
        local.format("%A %-d %B, %H:%M")
    );
    if let Some(notes) = notes {
        text.push_str(notes);
        text.push_str("\n\n");
    }
    text.push_str(&format!("Your mood file now:\n{}\n\n", mood.trim()));
    if changed.is_empty() {
        text.push_str("Nothing changed in your memory since your last check.");
    } else {
        let paths: Vec<String> = changed
            .iter()
            .map(|(path, content)| match content {
                Some(_) => format!("- {path}"),
                None => format!("- {path} (deleted)"),
            })
            .collect();
        text.push_str(&format!(
            "Changed in your memory since your last check:\n{}",
            paths.join("\n")
        ));
    }
    text
}

/// The answer as a mood file, without code fences, if it has a status line.
fn clean(answer: &str) -> Option<String> {
    let lines: Vec<&str> = answer
        .trim()
        .lines()
        .filter(|line| !line.trim_start().starts_with("```"))
        .collect();
    let file = lines.join("\n").trim().to_string();
    reflect::parse_status(&file).map(|_| file)
}

struct NoTools;

#[async_trait::async_trait]
impl ToolRunner for NoTools {
    async fn run(&self, call: &ToolCall) -> String {
        format!("Error: there is no tool named {}.", call.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_the_local_time() {
        let now = DateTime::parse_from_rfc3339("2026-10-10T21:40:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let zone: Tz = "Europe/Stockholm".parse().unwrap();
        let text = input(now, zone, None, "mood: fine", &[]);
        assert!(text.starts_with("It's Saturday 10 October, 23:40 where you live."));
        assert!(text.contains("Nothing changed"));
        let changed = vec![("/memories/a.md".to_string(), None)];
        let text = input(now, zone, Some("# notes"), "mood: fine", &changed);
        assert!(text.contains("# notes"));
        assert!(text.contains("- /memories/a.md (deleted)"));
    }

    #[test]
    fn cleans_the_answer() {
        assert_eq!(
            clean("```\nmood: sleepy\nstatus: yawning\n```"),
            Some("mood: sleepy\nstatus: yawning".into())
        );
        assert_eq!(clean("I feel fine."), None);
    }

    #[test]
    fn settings_default_to_five_checks() {
        let settings = Settings::default();
        assert_eq!(settings.mood_checks_per_day, 5);
        assert!(system(&[]).ends_with(ANSWER));
        assert!(system(&["sleepy".into()]).contains("sleepy"));
    }
}
