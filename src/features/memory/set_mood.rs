//! The `set_mood` chat tool: Vivy changes her mood in the middle of a conversation, when
//! something there really moves her. It rewrites the `mood:`, `status:` and `face:` lines
//! of her mood file in that server (keeping the rest), then sets her Discord status and
//! face like any other mood change. The timed mood checks (`mood.rs`) cover the rest of the
//! day: waking up, getting hungry, getting sleepy.

use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use super::{face, reflect, store};
use crate::ai::ToolDef;
use crate::core::{Asker, BotCtx, Result, user_error};

pub const NAME: &str = "set_mood";

pub fn def() -> ToolDef {
    ToolDef {
        name: NAME,
        description: "Change your mood right now, when something in this conversation really moves you: someone made you laugh, annoyed you, cheered you up or told you sad news. It rewrites the mood and status lines of /memories/vivy/mood.md, which set your Discord status, and your face (your profile picture here) when you give one. \
Don't use it for small things or in every conversation: your mood also changes on its own through the day. Only in servers.",
        parameters: json!({
            "type": "object",
            "properties": {
                "mood": {"type": "string", "description": "A few words on how you feel now and why."},
                "status": {"type": "string", "description": "Your new Discord status: under 80 characters, in your voice, about what's on your mind. No hashtags."},
                "face": {"type": "string", "description": "Optional: one of your faces (listed at the start of the conversation), like happy or grumpy."}
            },
            "required": ["mood", "status"]
        }),
    }
}

#[derive(Debug, Deserialize)]
struct Args {
    mood: String,
    status: String,
    #[serde(default)]
    face: Option<String>,
}

/// The faces she can pick, for the start of a conversation. `None` without faces.
pub fn faces_note(ctx: &BotCtx) -> Option<String> {
    let names = face::names(ctx);
    (!names.is_empty()).then(|| {
        format!(
            "Your faces (for set_mood and the face: line of your mood): {}.",
            names.join(", ")
        )
    })
}

pub async fn run(ctx: &BotCtx, asker: &Asker, args: &Value) -> Result<String> {
    let Some(guild) = asker.guild else {
        return Err(user_error(
            "your mood belongs to a server; in DMs it stays as it is",
        ));
    };
    let args: Args = serde_json::from_value(args.clone())
        .map_err(|err| user_error(format!("not valid set_mood arguments: {err}")))?;
    let names = face::names(ctx);
    let face = match args
        .face
        .as_deref()
        .map(str::trim)
        .filter(|f| !f.is_empty())
    {
        None => None,
        Some(face) => Some(
            names
                .iter()
                .find(|name| name.eq_ignore_ascii_case(face))
                .cloned()
                .ok_or_else(|| {
                    user_error(format!(
                        "there is no face named {face}; the faces are: {}",
                        names.join(", ")
                    ))
                })?,
        ),
    };

    let scope = store::scope(Some(guild.get()), asker.user.get());
    let (owned, user) = (scope.clone(), asker.user.get());
    let (mood, status) = (args.mood, args.status);
    ctx.db
        .call(move |conn| {
            let tx = conn.transaction()?;
            let before = store::load(&tx, &owned)?;
            let mut after = before.clone();
            let mut file = before.get(reflect::MOOD_FILE).cloned().unwrap_or_default();
            file = set_line(&file, "mood", &mood);
            file = set_line(&file, "status", &status);
            if let Some(face) = &face {
                file = set_line(&file, "face", face);
            }
            after.insert(reflect::MOOD_FILE.to_string(), file);
            store::save(&tx, &owned, &before, &after, user, Utc::now().timestamp())?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    info!(scope, "changed her mood in a conversation");
    reflect::apply_mood(ctx, &scope).await;
    Ok("Your mood is updated.".to_string())
}

/// `mood` with the line labelled `label` set to `value`: replaced where it is, or added
/// at the end.
fn set_line(mood: &str, label: &str, value: &str) -> String {
    let value = value.trim().replace('\n', " ");
    let new = format!("{label}: {value}");
    let mut found = false;
    let mut lines: Vec<String> = mood
        .lines()
        .map(|line| {
            let matches = reflect::mood_line(line, label).is_some()
                || line.trim().eq_ignore_ascii_case(&format!("{label}:"));
            if !found && matches {
                found = true;
                new.clone()
            } else {
                line.to_string()
            }
        })
        .collect();
    if !found {
        lines.push(new);
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_lines() {
        let mood = "mood: sleepy\n- **Status**: \"zzz\"\nthinking: soup";
        let changed = set_line(mood, "status", "wide awake now");
        assert_eq!(
            changed,
            "mood: sleepy\nstatus: wide awake now\nthinking: soup"
        );
        assert_eq!(
            set_line("mood: ok", "face", "happy"),
            "mood: ok\nface: happy"
        );
        assert_eq!(set_line("", "mood", "fine"), "mood: fine");
    }
}
