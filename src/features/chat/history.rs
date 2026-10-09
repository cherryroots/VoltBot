//! Turning Discord messages into chat turns, and stored turns into what the model reads.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context as _;
use serde_json::{Value, json};
use serenity::all::{ContentSafeOptions, Message, content_safe};
use tracing::warn;

use super::store::{self, StoredPart, Turn};
use crate::ai::{self, ChatProvider, Input, Part, Role};
use crate::core::BotCtx;
use crate::util::media::{self, Media};
use crate::util::text::{attachment_text, embed_text};

/// At most this many images, GIFs or videos are read from one message.
const MAX_MEDIA_PER_MESSAGE: usize = 8;
/// Only the newest turns with media send it again in a full history; older ones say there
/// was an image. Videos turn into several grids each, so this keeps requests small.
const MEDIA_TURNS: usize = 4;
/// How far up a reply chain the history goes.
pub const MAX_TURNS: usize = 40;

/// Discord sometimes adds link previews (and the GIF of a GIF link) a moment after the
/// message arrives. When a message has links but no previews yet, wait and read it again.
pub async fn with_previews(ctx: &BotCtx, msg: &Message) -> Message {
    let has_link = msg.content.contains("https://") || msg.content.contains("http://");
    if !has_link || !msg.embeds.is_empty() {
        return msg.clone();
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    match msg.channel_id.message(&ctx.http, msg.id).await {
        Ok(fresh) => fresh,
        Err(err) => {
            warn!("couldn't read the message again for link previews: {err}");
            msg.clone()
        }
    }
}

/// The name the model sees for a message's author: server nickname, display name or
/// username.
fn author_name(msg: &Message) -> String {
    msg.member
        .as_ref()
        .and_then(|m| m.nick.clone())
        .or_else(|| msg.author.global_name.clone())
        .unwrap_or_else(|| msg.author.name.clone())
}

/// What chat stores for a message: its text (with attachments and embeds, tagged with the
/// author for messages from people) and links to its media. `text` replaces the message
/// content, for example without the bot mention.
pub async fn read_message(
    ctx: &BotCtx,
    msg: &Message,
    text: &str,
    from_bot: bool,
) -> Vec<StoredPart> {
    // "<@123>" becomes "@name", so the model sees who was mentioned.
    let text = content_safe(
        &ctx.cache,
        text,
        &ContentSafeOptions::default(),
        &msg.mentions,
    );
    let text = if from_bot {
        text
    } else {
        format!(
            "<user name=\"{}\" id=\"{}\">{}{}{}</user>",
            author_name(msg),
            msg.author.id,
            attachment_text(&ctx.web, msg).await,
            embed_text(msg),
            text.trim()
        )
    };

    let mut parts = vec![StoredPart::Text { text }];
    if !from_bot {
        for found in media::find_media(msg)
            .into_iter()
            .take(MAX_MEDIA_PER_MESSAGE)
        {
            parts.push(StoredPart::Media {
                url: found.url,
                kind: found.kind.into(),
                mime: found.mime.to_string(),
            });
        }
    }
    parts
}

/// Picks what to send: only the new turns when the newest answer in the chain was written
/// by this provider and model and can be continued, otherwise the whole chain.
pub async fn build_input(ctx: &BotCtx, provider: &dyn ChatProvider, chain: &[Turn]) -> Input {
    let continuable = chain.iter().rposition(|turn| {
        turn.role == Role::Assistant
            && turn.provider.as_deref() == Some(provider.name())
            && turn.model.as_deref() == Some(provider.model())
            && turn.continuation_id.is_some()
    });
    match continuable {
        Some(i) => Input::After {
            continuation: chain[i].continuation_id.clone().unwrap_or_default(),
            new: to_model_turns(ctx, &chain[i + 1..]).await,
        },
        None => Input::Full(to_model_turns(ctx, chain).await),
    }
}

/// Stored turns as the model reads them, with media downloaded. Media that can't be loaded
/// is replaced by a short note, so the model knows something was there.
async fn to_model_turns(ctx: &BotCtx, turns: &[Turn]) -> Vec<ai::Turn> {
    let with_media: Vec<i64> = turns
        .iter()
        .filter(|t| {
            t.parts
                .iter()
                .any(|p| matches!(p, StoredPart::Media { .. }))
        })
        .map(|t| t.id)
        .collect();
    let load_from = with_media.len().saturating_sub(MEDIA_TURNS);
    let loaded_turns = &with_media[load_from..];

    // Discord's attachment links expire after about a day; ask for fresh ones.
    let urls: Vec<String> = turns
        .iter()
        .filter(|t| loaded_turns.contains(&t.id))
        .flat_map(|t| &t.parts)
        .filter_map(|p| match p {
            StoredPart::Media { url, .. } => Some(url.clone()),
            StoredPart::Text { .. } => None,
        })
        .collect();
    let fresh = refresh_urls(ctx, &urls).await;

    let mut result = Vec::new();
    for turn in turns {
        let mut parts = Vec::new();
        for part in &turn.parts {
            match part {
                StoredPart::Text { text } => parts.push(Part::Text(text.clone())),
                // Answers never carry media of their own.
                StoredPart::Media { .. } if turn.role == Role::Assistant => {}
                StoredPart::Media { .. } if !loaded_turns.contains(&turn.id) => {
                    parts.push(Part::Text("[an earlier image]".into()));
                }
                StoredPart::Media { url, kind, mime } => {
                    let media = Media {
                        url: fresh.get(url).unwrap_or(url).clone(),
                        kind: (*kind).into(),
                        mime: static_mime(mime),
                    };
                    match media::load_for_model(&ctx.web, &media).await {
                        Ok(images) => parts.extend(images.into_iter().map(Part::Image)),
                        Err(err) => {
                            warn!("couldn't load {url} for chat: {err:#}");
                            parts.push(Part::Text("[an image that couldn't be loaded]".into()));
                        }
                    }
                }
            }
        }
        result.push(ai::Turn {
            role: turn.role,
            parts,
        });
    }
    result
}

/// The stored MIME type as one of the known `&'static str`s.
fn static_mime(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "image/jpeg",
        "image/webp" => "image/webp",
        "image/gif" => "image/gif",
        "video/mp4" => "video/mp4",
        "video/webm" => "video/webm",
        "video/quicktime" => "video/quicktime",
        _ => "image/png",
    }
}

/// Whether a link is a Discord attachment link, which expires.
fn is_discord_attachment(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    matches!(
        parsed.host_str(),
        Some("cdn.discordapp.com" | "media.discordapp.net")
    ) && (parsed.path().starts_with("/attachments/")
        || parsed.path().starts_with("/ephemeral-attachments/"))
}

/// Asks Discord for fresh copies of expired attachment links. Returns old → new. On any
/// error the old links are used as they are.
async fn refresh_urls(ctx: &BotCtx, urls: &[String]) -> HashMap<String, String> {
    let urls: Vec<&String> = urls.iter().filter(|u| is_discord_attachment(u)).collect();
    if urls.is_empty() {
        return HashMap::new();
    }
    let result: anyhow::Result<Value> = async {
        let response = ctx
            .web
            .post("https://discord.com/api/v10/attachments/refresh-urls")
            .header("Authorization", ctx.http.token())
            .json(&json!({ "attachment_urls": urls }))
            .send()
            .await?
            .error_for_status()?;
        Ok(response.json().await?)
    }
    .await;
    match result {
        Ok(body) => body["refreshed_urls"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|pair| {
                Some((
                    pair["original"].as_str()?.to_string(),
                    pair["refreshed"].as_str()?.to_string(),
                ))
            })
            .collect(),
        Err(err) => {
            warn!("couldn't refresh attachment links: {err:#}");
            HashMap::new()
        }
    }
}

/// The turn for the message someone replied to: the stored one if the bot has seen it,
/// otherwise a new turn made from it (a reply to someone else's message, or to a voltgpt
/// answer).
pub async fn turn_for_reference(ctx: &BotCtx, referenced: &Message) -> anyhow::Result<i64> {
    let id = referenced.id.get();
    if let Some(turn) = ctx
        .db
        .call(move |conn| store::turn_for_message(conn, id))
        .await?
    {
        return Ok(turn.id);
    }
    let from_bot = referenced.author.id == ctx.bot_id;
    let parts = read_message(ctx, referenced, &referenced.content, from_bot).await;
    let turn = store::NewTurn {
        parent_id: None,
        role: if from_bot {
            Role::Assistant
        } else {
            Role::User
        },
        author_id: referenced.author.id.get(),
        channel_id: referenced.channel_id.get(),
        parts,
        written: None,
        created_at: referenced.timestamp.unix_timestamp(),
    };
    ctx.db
        .call(move |conn| store::add_turn(conn, &turn, &[id]))
        .await
        .context("saving the replied-to message")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discord_attachment_links() {
        assert!(is_discord_attachment(
            "https://cdn.discordapp.com/attachments/1/2/a.png?ex=1"
        ));
        assert!(is_discord_attachment(
            "https://media.discordapp.net/attachments/1/2/a.png"
        ));
        assert!(!is_discord_attachment(
            "https://cdn.discordapp.com/emojis/1.png"
        ));
        assert!(!is_discord_attachment(
            "https://example.com/attachments/a.png"
        ));
    }
}
