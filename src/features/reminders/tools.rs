//! Reminder tools for chat: "@Vivy remind me to stretch every... no, in an hour" works in a
//! normal conversation, because the model can call these.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde_json::{Value, json};

use super::parse;
use super::store::{self, NewReminder};
use crate::ai::ToolDef;
use crate::core::{Asker, BotCtx, Result, settings, user_error};

pub fn defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "create_reminder",
            description: "Reminds the asker of something later, in this channel.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "when": {
                        "type": "string",
                        "description": "When, in English: \"in 2h30m\", \"at 16:30 CET\", \"tomorrow at 9am\", \"next friday\". Times without a zone use the asker's timezone."
                    },
                    "message": {"type": "string", "description": "What to remind them of."}
                },
                "required": ["when", "message"],
            }),
        },
        ToolDef {
            name: "list_reminders",
            description: "The asker's pending reminders, soonest first, with their IDs.",
            parameters: json!({"type": "object", "properties": {}}),
        },
        ToolDef {
            name: "cancel_reminder",
            description: "Deletes one of the asker's pending reminders.",
            parameters: json!({
                "type": "object",
                "properties": {"id": {"type": "integer", "description": "From list_reminders."}},
                "required": ["id"],
            }),
        },
    ]
}

/// Runs a tool. Returns whether a reminder was added or removed (the scheduler must wake),
/// and the text for the model.
pub async fn run(ctx: &BotCtx, asker: &Asker, name: &str, args: &Value) -> Result<(bool, String)> {
    let zone = settings::timezone(&ctx.db, asker.user).await?;
    let home = zone.unwrap_or(Tz::UTC);
    let user = asker.user.get();
    match name {
        "create_reminder" => {
            let when = args["when"].as_str().unwrap_or_default();
            let message = args["message"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string();
            if message.is_empty() {
                return Err(user_error("`message` is empty."));
            }
            let parsed = parse::parse_when(when).map_err(user_error)?;
            let fire_at = parse::resolve(&parsed, Utc::now(), home)
                .map_err(user_error)?
                .timestamp();
            let new = NewReminder {
                user_id: user,
                channel_id: asker.channel.get(),
                guild_id: asker.guild.map(|g| g.get()),
                message,
                fire_at,
                created_at: Utc::now().timestamp(),
                source_message_id: Some(asker.message.get()),
                missing_images: 0,
                images: Vec::new(),
            };
            let id = ctx.db.call(move |conn| Ok(store::add(conn, &new)?)).await?;
            let mut text = format!(
                "Reminder {id} set for {}. In Discord, <t:{fire_at}:f> shows that time to each reader.",
                local(fire_at, home)
            );
            if zone.is_none() {
                text.push_str(" The asker has no timezone set, so times were read as UTC; they can set one with /timezone.");
            }
            Ok((true, text))
        }
        "list_reminders" => {
            let list = ctx
                .db
                .call(move |conn| Ok(store::list_pending(conn, user)?))
                .await?;
            if list.is_empty() {
                return Ok((false, "No pending reminders.".to_string()));
            }
            let lines: Vec<String> = list
                .iter()
                .map(|r| format!("ID {}: {}: {}", r.id, local(r.fire_at, home), r.message))
                .collect();
            Ok((false, lines.join("\n")))
        }
        "cancel_reminder" => {
            let id = args["id"]
                .as_i64()
                .ok_or_else(|| user_error("`id` must be a number."))?;
            let deleted = ctx
                .db
                .call(move |conn| Ok(store::delete_pending(conn, id, user)?))
                .await?;
            if deleted {
                Ok((true, format!("Reminder {id} deleted.")))
            } else {
                Err(user_error(format!(
                    "The asker has no pending reminder {id}."
                )))
            }
        }
        _ => Err(user_error(format!("reminders has no tool named {name}"))),
    }
}

/// "2026-10-10 09:00 Europe/Oslo"
fn local(timestamp: i64, zone: Tz) -> String {
    let time = DateTime::from_timestamp(timestamp, 0).unwrap_or_default();
    format!(
        "{} {}",
        time.with_timezone(&zone).format("%Y-%m-%d %H:%M"),
        zone.name()
    )
}
