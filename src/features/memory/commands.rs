//! `/memory show`, `/memory forget` and `/memory delete`. The answers are only visible to
//! the person who asked.

use chrono::Utc;
use poise::CreateReply;
use serenity::all::{CreateAllowedMentions, CreateAttachment};
use tracing::info;

use super::folder::{self, Command, Folder, ROOT};
use super::store;
use crate::core::{Context, Result, user_error};

/// Longest file shown in the message itself; longer ones come as an attachment.
const MAX_INLINE: usize = 1800;

/// See or wipe what Vivy remembers
#[poise::command(
    slash_command,
    subcommands("show", "forget", "delete"),
    subcommand_required
)]
pub async fn memory(_ctx: Context<'_>) -> Result<()> {
    Ok(())
}

/// Show a memory file or folder (default: your own file)
#[poise::command(slash_command, ephemeral)]
async fn show(
    ctx: Context<'_>,
    #[description = "A path like /memories/server.md, or /memories for the list"] path: Option<
        String,
    >,
) -> Result<()> {
    let path = match path {
        Some(path) => folder::clean_path(&path).map_err(user_error)?,
        None => own_file(ctx),
    };
    let folder = load(ctx).await?;
    let reply = match folder.get(&path) {
        Some(content) if content.len() <= MAX_INLINE => CreateReply::default()
            .content(format!("**{path}**\n```md\n{}\n```", fence_safe(content))),
        Some(content) => CreateReply::default()
            .content(format!("**{path}**"))
            .attachment(CreateAttachment::bytes(
                content.as_bytes(),
                file_name(&path),
            )),
        None if path == own_file(ctx) => {
            CreateReply::default().content("Vivy hasn't saved anything about you here.")
        }
        None => {
            // A directory gets the same listing the model sees.
            let mut copy = folder.clone();
            let view = Command::View {
                path: path.clone(),
                view_range: None,
            };
            let text = folder::run(&mut copy, view).unwrap_or_else(|err| err);
            CreateReply::default().content(format!("```\n{}\n```", shorten(&text)))
        }
    };
    ctx.send(reply.allowed_mentions(CreateAllowedMentions::new()))
        .await?;
    Ok(())
}

/// Delete everything Vivy remembers about you here
#[poise::command(slash_command, ephemeral)]
async fn forget(ctx: Context<'_>) -> Result<()> {
    // In DMs the whole folder is yours; in a server, your own file.
    let path = if ctx.guild_id().is_some() {
        own_file(ctx)
    } else {
        ROOT.to_string()
    };
    let text = match remove(ctx, &path).await? {
        0 => "There was nothing to forget.",
        _ => "Done. Vivy has forgotten what it saved about you here.",
    };
    ctx.say(text).await?;
    Ok(())
}

/// Delete a memory file or folder (admins)
#[poise::command(slash_command, ephemeral)]
async fn delete(
    ctx: Context<'_>,
    #[description = "A path like /memories/users/123.md"] path: String,
) -> Result<()> {
    if !ctx.data().is_admin(ctx.author().id) {
        return Err(user_error("Only admins can delete other memory files."));
    }
    let path = folder::clean_path(&path).map_err(user_error)?;
    let count = remove(ctx, &path).await?;
    let text = match count {
        0 => format!("There's nothing at {path}."),
        1 => format!("Deleted {path}."),
        n => format!("Deleted {path} ({n} files)."),
    };
    ctx.say(text).await?;
    Ok(())
}

/// `/memories/users/<your id>.md`
fn own_file(ctx: Context<'_>) -> String {
    format!("{ROOT}/users/{}.md", ctx.author().id)
}

fn scope(ctx: Context<'_>) -> String {
    store::scope(ctx.guild_id().map(|g| g.get()), ctx.author().id.get())
}

async fn load(ctx: Context<'_>) -> Result<Folder> {
    let scope = scope(ctx);
    ctx.data()
        .db
        .call(move |conn| Ok(store::load(conn, &scope)?))
        .await
}

/// Deletes a file, or everything under a directory, and logs it. Returns how many files.
async fn remove(ctx: Context<'_>, path: &str) -> Result<usize> {
    let scope = scope(ctx);
    let user = ctx.author().id.get();
    let target = path.to_string();
    let log_scope = scope.clone();
    let count = ctx
        .data()
        .db
        .call(move |conn| {
            let tx = conn.transaction()?;
            let before = store::load(&tx, &scope)?;
            let prefix = format!("{target}/");
            let after: Folder = before
                .iter()
                .filter(|(p, _)| **p != target && !p.starts_with(&prefix))
                .map(|(p, c)| (p.clone(), c.clone()))
                .collect();
            let count = store::save(&tx, &scope, &before, &after, user, Utc::now().timestamp())?;
            tx.commit()?;
            Ok(count)
        })
        .await?;
    if count > 0 {
        info!(scope = log_scope, "memory: {path} deleted with /memory");
    }
    Ok(count)
}

/// Keeps a file's own ``` from ending the code block early.
fn fence_safe(text: &str) -> String {
    text.replace("```", "`\u{200b}``")
}

fn shorten(text: &str) -> String {
    crate::util::shorten(&fence_safe(text), MAX_INLINE)
}

/// "/memories/users/1.md" → "1.md"
fn file_name(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or("memory");
    if name.contains('.') {
        name.to_string()
    } else {
        format!("{name}.txt")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_and_fences() {
        assert_eq!(file_name("/memories/users/1.md"), "1.md");
        assert_eq!(file_name("/memories/notes"), "notes.txt");
        assert_eq!(fence_safe("a ``` b"), "a `\u{200b}`` b");
    }
}
