//! What reminders look like in Discord: message texts, buttons, the delete menu, and the
//! button IDs.

use poise::CreateReply;
use serenity::all::{
    ButtonStyle, CreateActionRow, CreateAllowedMentions, CreateAttachment, CreateButton,
    CreateMessage, CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption, UserId,
};

use super::store::{Reminder, Summary};
use crate::util::shorten;

/// Shown when a reminder can't be understood.
pub const HELP: &str = "Try one of these:
- `remind me in 2h30m to do the thing`
- `remind me at 16:30 CET to do the thing`
- `remind me tomorrow at 9am to do the thing`
- `remind me to do the thing next friday`";

/// The snooze buttons under a delivered reminder: (label, minutes).
const SNOOZES: [(&str, i64); 3] = [("10 min", 10), ("1 hour", 60), ("Tomorrow", 24 * 60)];

/// Every button and menu this feature makes. The ID text is built and parsed only here.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// The `/reminders` delete menu. The reminder ID is the selected value.
    Delete,
    /// A snooze button on a delivered reminder.
    Snooze { id: i64, minutes: i64 },
}

impl Action {
    pub fn custom_id(&self) -> String {
        match self {
            Action::Delete => "reminders:delete".to_string(),
            Action::Snooze { id, minutes } => format!("reminders:snooze:{id}:{minutes}"),
        }
    }

    /// Parses what the dispatcher passes on: the ID without the `reminders:` prefix.
    pub fn parse(action: &str) -> Option<Action> {
        let parts: Vec<&str> = action.split(':').collect();
        match parts.as_slice() {
            ["delete"] => Some(Action::Delete),
            ["snooze", id, minutes] => Some(Action::Snooze {
                id: id.parse().ok()?,
                minutes: minutes.parse().ok()?,
            }),
            _ => None,
        }
    }
}

/// The reply after setting a reminder.
pub fn confirmation(fire_at: i64, message: &str, zone_hint: bool, skipped: &[String]) -> String {
    let mut text = format!(
        "I'll remind you <t:{fire_at}:R> (<t:{fire_at}:f>): {}",
        shorten(message, 1500)
    );
    if zone_hint {
        text.push_str("\n-# Times without a timezone are read as UTC. Set yours with `/timezone`.");
    }
    if !skipped.is_empty() {
        text.push_str(&format!(
            "\n-# Couldn't save {}, so the reminder will link to your message instead.",
            skipped.join(", ")
        ));
    }
    text
}

/// The delivered reminder: pings only its owner, links to the message that set it, and
/// carries its images and snooze buttons. With `with_images` false the images are left off
/// (used when Discord refused the upload) and counted in the "missing" hint instead.
pub fn fired_message(reminder: &Reminder, now: i64, with_images: bool) -> CreateMessage {
    let set = match reminder.source_message_id {
        Some(message) => {
            let guild = reminder
                .guild_id
                .map_or("@me".to_string(), |id| id.to_string());
            // The <> around the link stops Discord from showing a preview of it.
            format!(
                "[Reminder set](<https://discord.com/channels/{guild}/{}/{message}>)",
                reminder.channel_id
            )
        }
        None => "Reminder set".to_string(),
    };
    let mut content = format!(
        "<@{}> ⏰ {}\n-# {set} <t:{}:f>",
        reminder.user_id,
        shorten(&reminder.message, 1800),
        reminder.created_at
    );
    // Say so when it's late, for example after the bot was offline.
    if now - reminder.fire_at > 5 * 60 {
        content.push_str(&format!(", due <t:{}:R>", reminder.fire_at));
    }
    let mut missing = reminder.missing_images as usize;
    if !with_images {
        missing += reminder.images.len();
    }
    match missing {
        0 => {}
        1 => content.push_str(" · [image missing]"),
        n => content.push_str(&format!(" · [{n} images missing]")),
    }

    let buttons = SNOOZES
        .iter()
        .map(|(label, minutes)| {
            let action = Action::Snooze {
                id: reminder.id,
                minutes: *minutes,
            };
            CreateButton::new(action.custom_id())
                .label(format!("Snooze {label}"))
                .style(ButtonStyle::Secondary)
        })
        .collect();

    let mut message = CreateMessage::new()
        .content(content)
        .allowed_mentions(CreateAllowedMentions::new().users([UserId::new(reminder.user_id)]))
        .components(vec![CreateActionRow::Buttons(buttons)]);
    if !with_images {
        return message;
    }
    for image in &reminder.images {
        message = message.add_file(CreateAttachment::bytes(
            image.data.clone(),
            image.filename.clone(),
        ));
    }
    message
}

/// The delivered reminder's text after a snooze button was pressed.
pub fn snoozed(original: &str, until: i64) -> String {
    let line = format!("\n-# 😴 Snoozed until <t:{until}:f>");
    // Discord allows 2000 characters, so cut the original to leave room for the line.
    let room = 2000 - line.chars().count();
    format!("{}{line}", shorten(original, room))
}

/// The `/reminders` list, with a menu to delete one. Discord menus hold at most 25 options.
pub fn list_reply(reminders: &[Summary]) -> CreateReply {
    if reminders.is_empty() {
        return CreateReply::default().content("You have no pending reminders.");
    }

    let shown = &reminders[..reminders.len().min(25)];
    let mut text = String::from("**Your reminders**\n");
    for (n, reminder) in shown.iter().enumerate() {
        let images = match reminder.image_count {
            0 => String::new(),
            1 => " (1 image)".to_string(),
            n => format!(" ({n} images)"),
        };
        text.push_str(&format!(
            "{}. <t:{}:R>: {}{images}\n",
            n + 1,
            reminder.fire_at,
            shorten(&reminder.message, 60)
        ));
    }
    if reminders.len() > shown.len() {
        text.push_str(&format!("-# Showing the first 25 of {}.", reminders.len()));
    }

    let options = shown
        .iter()
        .enumerate()
        .map(|(n, reminder)| {
            let label = shorten(&format!("{}. {}", n + 1, reminder.message), 100);
            CreateSelectMenuOption::new(label, reminder.id.to_string())
        })
        .collect();
    let menu = CreateSelectMenu::new(
        Action::Delete.custom_id(),
        CreateSelectMenuKind::String { options },
    )
    .placeholder("Delete a reminder…");

    CreateReply::default()
        .content(text)
        .components(vec![CreateActionRow::SelectMenu(menu)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::reminders::store::Image;

    fn reminder() -> Reminder {
        Reminder {
            id: 1,
            user_id: 2,
            channel_id: 3,
            guild_id: Some(4),
            message: "check the oven".into(),
            fire_at: 1000,
            created_at: 500,
            attempts: 0,
            source_message_id: Some(5),
            missing_images: 0,
            images: vec![Image {
                filename: "a.png".into(),
                data: vec![1],
            }],
        }
    }

    /// The text the message would send.
    fn content(message: &CreateMessage) -> String {
        let json = serde_json::to_value(message).unwrap();
        json["content"].as_str().unwrap().to_string()
    }

    #[test]
    fn fired_message_links_to_the_source() {
        let text = content(&fired_message(&reminder(), 1000, true));
        assert_eq!(
            text,
            "<@2> ⏰ check the oven\n-# [Reminder set](<https://discord.com/channels/4/3/5>) <t:500:f>"
        );

        let mut dm = reminder();
        dm.guild_id = None;
        assert!(content(&fired_message(&dm, 1000, true)).contains("/channels/@me/3/5"));

        let mut imported = reminder();
        imported.source_message_id = None;
        assert!(
            content(&fired_message(&imported, 1000, true)).contains("-# Reminder set <t:500:f>")
        );
    }

    #[test]
    fn fired_message_counts_missing_images() {
        let mut r = reminder();
        assert!(!content(&fired_message(&r, 1000, true)).contains("missing"));
        assert!(content(&fired_message(&r, 1000, false)).ends_with(" · [image missing]"));
        r.missing_images = 2;
        assert!(content(&fired_message(&r, 1000, true)).ends_with(" · [2 images missing]"));
        assert!(content(&fired_message(&r, 1000, false)).ends_with(" · [3 images missing]"));
    }

    #[test]
    fn snoozed_fits_in_a_message() {
        assert_eq!(snoozed("hi", 5), "hi\n-# 😴 Snoozed until <t:5:f>");
        let long = "a".repeat(2000);
        let text = snoozed(&long, 1_800_000_000);
        assert_eq!(text.chars().count(), 2000);
        assert!(text.ends_with("<t:1800000000:f>"));
    }

    #[test]
    fn action_round_trip() {
        for action in [
            Action::Delete,
            Action::Snooze {
                id: 12,
                minutes: 60,
            },
        ] {
            let id = action.custom_id();
            let rest = id.strip_prefix("reminders:").unwrap();
            assert_eq!(Action::parse(rest), Some(action));
        }
        assert_eq!(Action::parse("snooze:x:10"), None);
        assert_eq!(Action::parse("explode"), None);
    }
}
