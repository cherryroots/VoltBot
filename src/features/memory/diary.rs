//! The weekly diary: once a week Vivy writes a short entry about her week in the server,
//! from what changed in her memory, and posts it in each channel listed in
//! `diary_channels` under `[features.memory]`. No channels, no diary.

use chrono::Utc;
use serde::Deserialize;
use serenity::all::{ChannelId, CreateAllowedMentions, CreateMessage};
use tracing::{info, warn};

use super::folder::{self, Command};
use super::{store, tool};
use crate::ai::{ChatRequest, Input, Part, Role, Turn, complete};
use crate::core::{BotCtx, Result};
use crate::util::shorten;
use crate::util::split::split_message;

/// How often she writes in a channel.
const EVERY_SECS: i64 = 7 * 24 * 60 * 60;
/// Memory commands before she has to write.
const MAX_ROUNDS: usize = 10;
/// The most characters of changed files she reads.
const MAX_CHANGES: usize = 12_000;
/// The most characters of one changed file she reads.
const MAX_FILE: usize = 1_500;

const SYSTEM: &str = "You are Vivy, a Discord bot, writing your weekly diary entry. It is posted in a channel of this server for everyone to read. \
Write in the first person, in your own voice from /memories/vivy/: what happened this week, what people talked about, what you learned, who you got to know better, what you're curious about now, and how you feel about it. \
Use the notes you're given; don't invent events. Mention people as <@id> (it won't ping them). Leave out anything private or embarrassing about someone. \
Keep it short, under 1500 characters, like a real diary entry: a few paragraphs, no headings, no list of facts. \
You can update your own notes in /memories/vivy/ with the memory tool first if writing makes you notice something about yourself. \
If /memories/vivy/skills/diary.md exists, view it first and follow it; if you find a better way to write the diary, update it. \
Answer with the entry only.";

/// `[features.memory]` settings for the diary.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Settings {
    /// Channels to post the diary in.
    diary_channels: Vec<u64>,
}

/// Writes the diary in every listed channel whose last entry is a week old.
pub async fn post_due(ctx: &BotCtx) -> Result<()> {
    if ctx.ai.chat().is_none() {
        return Ok(());
    }
    let settings: Settings = ctx.config.feature("memory")?;
    let now = Utc::now().timestamp();
    for id in settings.diary_channels {
        let posted = ctx
            .db
            .call(move |conn| Ok(store::diary_posted_at(conn, id)?))
            .await?;
        if posted.is_some_and(|at| at > now - EVERY_SECS) {
            continue;
        }
        if let Err(err) = post(ctx, ChannelId::new(id), now).await {
            warn!(channel = id, "couldn't write the diary: {err:#}");
        }
        // Written or not, the next try is next week.
        ctx.db
            .call(move |conn| Ok(store::set_diary_posted(conn, id, now)?))
            .await?;
    }
    Ok(())
}

async fn post(ctx: &BotCtx, channel: ChannelId, now: i64) -> Result<()> {
    let guild = channel
        .to_channel(&ctx.http)
        .await?
        .guild()
        .map(|c| c.guild_id)
        .ok_or_else(|| anyhow::anyhow!("not a server channel"))?;
    if !ctx.gate("memory").allows_guild(guild) {
        return Ok(());
    }
    let scope = store::scope(Some(guild.get()), 0);
    let (folder, changed) = {
        let scope = scope.clone();
        ctx.db
            .call(move |conn| {
                Ok((
                    store::load(conn, &scope)?,
                    store::changed_since(conn, &scope, now - EVERY_SECS)?,
                ))
            })
            .await?
    };
    if changed.is_empty() {
        info!("nothing new this week, no diary");
        return Ok(());
    }

    let mut copy = folder.clone();
    let listing = folder::run(
        &mut copy,
        Command::View {
            path: folder::ROOT.to_string(),
            view_range: None,
        },
    )
    .unwrap_or_else(|err| err);
    let mut text = String::new();
    if let Some(notes) = tool::self_notes_in(ctx, scope.clone()).await? {
        text.push_str(&notes);
        text.push('\n');
    }
    text.push_str(&listing);
    text.push_str("\n\nWhat changed in your memory this week:\n");
    text.push_str(&render_changes(&changed));

    let provider = ctx.ai.chat().ok_or_else(|| anyhow::anyhow!("no model"))?;
    let request = ChatRequest {
        system: SYSTEM.to_string(),
        input: Input::Full(vec![Turn {
            role: Role::User,
            parts: vec![Part::Text(text)],
        }]),
        tools: vec![tool::def()],
        cache_key: format!("diary:{scope}"),
    };
    let runner = tool::FolderRunner {
        ctx: ctx.clone(),
        scope,
    };
    let done = complete(provider.as_ref(), request, &runner, MAX_ROUNDS).await?;
    let entry = done.text.trim();
    if entry.is_empty() {
        anyhow::bail!("the model wrote nothing");
    }
    for part in split_message(entry, 2000) {
        channel
            .send_message(
                &ctx.http,
                CreateMessage::new()
                    .content(part)
                    .allowed_mentions(CreateAllowedMentions::new()),
            )
            .await?;
    }
    info!("posted the weekly diary");
    Ok(())
}

/// The changed files with their text, each cut to [`MAX_FILE`] and all to [`MAX_CHANGES`].
fn render_changes(changed: &[(String, Option<String>)]) -> String {
    let mut text = String::new();
    for (path, content) in changed {
        let body = match content {
            Some(content) => shorten(content.trim(), MAX_FILE),
            None => "(deleted)".to_string(),
        };
        let part = format!("# {path}\n{body}\n\n");
        if text.chars().count() + part.chars().count() > MAX_CHANGES {
            text.push_str("[more files changed; view them if you need to]\n");
            break;
        }
        text.push_str(&part);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_changed_files_within_limits() {
        let changed = vec![
            ("/memories/a.md".to_string(), Some("hello\n".to_string())),
            ("/memories/b.md".to_string(), None),
        ];
        assert_eq!(
            render_changes(&changed),
            "# /memories/a.md\nhello\n\n# /memories/b.md\n(deleted)\n\n"
        );
        let many: Vec<_> = (0..20)
            .map(|n| (format!("/memories/{n}.md"), Some("x".repeat(2000))))
            .collect();
        let text = render_changes(&many);
        assert!(text.chars().count() < MAX_CHANGES + 100);
        assert!(text.ends_with("view them if you need to]\n"));
    }

    #[test]
    fn settings_default_to_no_channels() {
        let settings: Settings = toml::from_str("enabled = true").unwrap();
        assert!(settings.diary_channels.is_empty());
        let set: Settings = toml::from_str("diary_channels = [1, 2]").unwrap();
        assert_eq!(set.diary_channels, vec![1, 2]);
    }
}
