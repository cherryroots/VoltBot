//! Reading a message's links and pictures and saving them.

use std::time::Duration;

use anyhow::Context as _;
use image::DynamicImage;
use serenity::all::{GuildId, Message};
use serenity::futures::future::join_all;
use tracing::debug;

use super::collect::{self, Picture};
use super::fingerprint::{self, Fingerprint};
use super::links;
use super::store::{self, Indexed};
use crate::core::BotCtx;
use crate::util::media;

/// The small proxy pictures are a few KB; anything bigger than this is not what we asked for.
const MAX_SMALL_BYTES: usize = 5 * 1024 * 1024;
/// When the proxy can't serve a picture, the original is fetched up to this size.
const MAX_ORIGINAL_BYTES: usize = 20 * 1024 * 1024;

/// The link keys in a message. Short links (vm.tiktok.com, t.co...) are followed first.
pub async fn link_keys(ctx: &BotCtx, msg: &Message) -> Vec<String> {
    let mut keys = Vec::new();
    for link in collect::message_links(msg) {
        let link = if links::needs_resolving(&link) {
            resolve(ctx, &link).await.unwrap_or(link)
        } else {
            link
        };
        if let Some(key) = links::link_key(&link)
            && !keys.contains(&key)
        {
            keys.push(key);
        }
    }
    keys
}

/// Where a short link leads.
async fn resolve(ctx: &BotCtx, link: &str) -> Option<String> {
    let response = ctx
        .web
        .head(link)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|err| debug!("couldn't follow {link}: {err}"))
        .ok()?;
    Some(response.url().to_string())
}

/// Downloads a picture, small from the media proxy if it can, and decodes it.
pub async fn load_picture(ctx: &BotCtx, picture: &Picture) -> anyhow::Result<DynamicImage> {
    let data = match media::download(&ctx.web, &picture.url, MAX_SMALL_BYTES).await {
        Ok(data) => data,
        Err(err) => {
            debug!("the media proxy couldn't serve {}: {err}", picture.url);
            media::download(&ctx.web, &picture.original, MAX_ORIGINAL_BYTES)
                .await
                .with_context(|| format!("downloading {}", picture.original))?
        }
    };
    tokio::task::spawn_blocking(move || image::load_from_memory(&data))
        .await?
        .context("decoding the picture")
}

/// A message's pictures, fingerprinted. Pictures that fail to load are left out.
pub async fn load_pictures(ctx: &BotCtx, msg: &Message) -> Vec<(usize, DynamicImage, Fingerprint)> {
    let pictures = collect::pictures(msg);
    let loads = pictures
        .iter()
        .enumerate()
        .map(|(position, picture)| async move {
            let img = match load_picture(ctx, picture).await {
                Ok(img) => img,
                Err(err) => {
                    debug!("skipping picture {position} of message {}: {err:#}", msg.id);
                    return None;
                }
            };
            let (img, fp) = tokio::task::spawn_blocking(move || {
                let fp = fingerprint::fingerprint(&img);
                (img, fp)
            })
            .await
            .ok()?;
            Some((position, img, fp))
        });
    join_all(loads).await.into_iter().flatten().collect()
}

/// Indexes one message of a server unless it already is. Returns how many pictures were
/// saved. (Messages read from history don't say which server they are in, so it's passed.)
pub async fn index_message(ctx: &BotCtx, guild: GuildId, msg: &Message) -> anyhow::Result<usize> {
    let id = msg.id.get();
    if ctx
        .db
        .call(move |conn| Ok(store::is_indexed(conn, id)?))
        .await?
    {
        return Ok(0);
    }
    let links = link_keys(ctx, msg).await;
    let pictures: Vec<(usize, Fingerprint)> = load_pictures(ctx, msg)
        .await
        .into_iter()
        .map(|(position, _, fp)| (position, fp))
        .collect();
    let count = pictures.len();
    let found = Indexed {
        guild: guild.get(),
        channel: msg.channel_id.get(),
        message: id,
        author: msg.author.id.get(),
        links,
        pictures,
    };
    ctx.db
        .call(move |conn| {
            let tx = conn.transaction()?;
            store::save(&tx, &found)?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    Ok(count)
}
