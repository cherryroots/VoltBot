//! Reading a message's links and pictures and saving them.

use std::time::Duration;

use anyhow::Context as _;
use chrono::Utc;
use image::DynamicImage;
use serenity::all::{GuildId, Message};
use serenity::futures::future::join_all;
use tracing::debug;

use super::collect::{self, Picture};
use super::fingerprint::{self, Fingerprint};
use super::links;
use super::store::{self, Failure, Indexed, StoredPicture};
use crate::core::BotCtx;
use crate::util::media;

/// The small proxy pictures are a few KB; anything bigger than this is not what we asked for.
const MAX_SMALL_BYTES: usize = 5 * 1024 * 1024;
/// When the proxy can't serve a picture, the original is fetched up to this size.
const MAX_ORIGINAL_BYTES: usize = 20 * 1024 * 1024;

/// The link keys in a message. Short links (vm.tiktok.com, t.co...) are followed first.
pub async fn link_keys(msg: &Message) -> Vec<String> {
    let mut keys = Vec::new();
    for link in collect::message_links(msg) {
        let link = if links::needs_resolving(&link) {
            resolve(&link).await.unwrap_or(link)
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

/// Where a short link leads. Goes through the download client that refuses local and
/// private addresses, at every redirect too.
async fn resolve(link: &str) -> Option<String> {
    media::check_url(link)
        .map_err(|err| debug!("not following {link}: {err}"))
        .ok()?;
    let response = media::safe_client()
        .head(link)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|err| debug!("couldn't follow {link}: {err}"))
        .ok()?;
    Some(response.url().to_string())
}

/// Downloads a picture, small from the media proxy if it can, and decodes it.
pub async fn load_picture(picture: &Picture) -> anyhow::Result<DynamicImage> {
    let data = match media::download(&picture.url, MAX_SMALL_BYTES).await {
        Ok(data) => data,
        Err(err) => {
            debug!("the media proxy couldn't serve {}: {err}", picture.url);
            media::download(&picture.original, MAX_ORIGINAL_BYTES)
                .await
                .with_context(|| format!("downloading {}", picture.original))?
        }
    };
    let img = tokio::task::spawn_blocking(move || decode(&data))
        .await?
        .context("decoding the picture")?;
    // An empty picture has nothing to fingerprint (and would break the crops).
    if img.width() == 0 || img.height() == 0 {
        anyhow::bail!("the picture is empty");
    }
    Ok(img)
}

/// Decodes a picture, refusing huge ones so a small file can't ask for gigabytes of memory.
fn decode(data: &[u8]) -> image::ImageResult<DynamicImage> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(128 * 1024 * 1024);
    let mut reader = image::ImageReader::new(std::io::Cursor::new(data)).with_guessed_format()?;
    reader.limits(limits);
    reader.decode()
}

/// A picture that downloaded, with its fingerprint.
pub struct LoadedPicture {
    pub position: usize,
    pub source: String,
    pub image: DynamicImage,
    pub fp: Fingerprint,
}

impl LoadedPicture {
    pub fn stored(&self) -> StoredPicture {
        StoredPicture {
            position: self.position,
            source: self.source.clone(),
            fp: self.fp.clone(),
        }
    }
}

/// Downloads and fingerprints the picture at `position` of a message.
pub async fn load_one(position: usize, picture: &Picture) -> Result<LoadedPicture, Failure> {
    let failure = |err: anyhow::Error| Failure {
        position,
        source: picture.source(),
        host: picture.host(),
        error: format!("{err:#}"),
    };
    let img = load_picture(picture).await.map_err(failure)?;
    let (image, fp) = tokio::task::spawn_blocking(move || {
        let fp = fingerprint::fingerprint(&img);
        (img, fp)
    })
    .await
    .map_err(|err| failure(err.into()))?;
    Ok(LoadedPicture {
        position,
        source: picture.source(),
        image,
        fp,
    })
}

/// Loads several pictures at once, splitting them into the ones that loaded and the ones
/// that failed.
async fn load_all(
    msg: &Message,
    pictures: &[(usize, Picture)],
) -> (Vec<LoadedPicture>, Vec<Failure>) {
    let loads = pictures
        .iter()
        .map(|(position, picture)| load_one(*position, picture));
    let mut loaded = Vec::new();
    let mut failed = Vec::new();
    for result in join_all(loads).await {
        match result {
            Ok(picture) => loaded.push(picture),
            Err(failure) => {
                debug!(
                    "picture {} of message {} failed, will retry: {}",
                    failure.position, msg.id, failure.error
                );
                failed.push(failure);
            }
        }
    }
    (loaded, failed)
}

/// All of a message's pictures, fingerprinted, and the ones that failed to load.
pub async fn load_pictures(msg: &Message) -> (Vec<LoadedPicture>, Vec<Failure>) {
    let pictures: Vec<(usize, Picture)> = collect::pictures(msg).into_iter().enumerate().collect();
    load_all(msg, &pictures).await
}

/// What [`sync_message`] found that wasn't saved before.
pub struct Synced {
    pub new_links: Vec<String>,
    pub new_pictures: Vec<LoadedPicture>,
    /// Pictures saved for the message now, old and new.
    pub pictures: usize,
}

/// Reads a message's links and pictures and makes the saved rows match: new ones are added
/// and ones an edit removed are dropped. Pictures already saved (the same file) keep their
/// fingerprint instead of being downloaded again, pictures that fail to load go on the
/// retry list, and pictures already on it are left to the retry loop. Safe to run again on
/// the same message: it only reports what is new. (Messages read from history don't say
/// which server they are in, so it's passed.)
pub async fn sync_message(ctx: &BotCtx, guild: GuildId, msg: &Message) -> anyhow::Result<Synced> {
    let id = msg.id.get();
    let (old_links, saved, retrying) = ctx
        .db
        .call(move |conn| {
            Ok((
                store::message_links(conn, id)?,
                store::message_pictures(conn, id)?,
                store::waiting_failures(conn, id)?,
            ))
        })
        .await?;
    let links = link_keys(msg).await;

    let mut kept = Vec::new();
    let mut to_load = Vec::new();
    let mut waiting = Vec::new();
    for (position, picture) in collect::pictures(msg).into_iter().enumerate() {
        let source = picture.source();
        // Pictures saved before sources were kept are matched by their place instead.
        let same = saved.iter().find(|s| match &s.source {
            Some(saved_source) => *saved_source == source,
            None => s.position == position,
        });
        match same {
            Some(s) => kept.push(StoredPicture {
                position,
                source,
                fp: s.fp.clone(),
            }),
            // It failed before and the retry loop is on it: downloading it again now (like
            // the re-read for link previews) would count as another try within seconds.
            None if retrying.contains(&source) => waiting.push(source),
            None => to_load.push((position, picture)),
        }
    }
    let (new_pictures, failed) = load_all(msg, &to_load).await;

    let mut pictures = kept;
    pictures.extend(new_pictures.iter().map(LoadedPicture::stored));
    let count = pictures.len();
    let new_links = links
        .iter()
        .filter(|key| !old_links.contains(key))
        .cloned()
        .collect();
    let found = Indexed {
        guild: guild.get(),
        channel: msg.channel_id.get(),
        message: id,
        author: msg.author.id.get(),
        links,
        pictures,
        failed,
        waiting,
    };
    let now = Utc::now().timestamp();
    ctx.db
        .call(move |conn| {
            let tx = conn.transaction()?;
            store::save(&tx, &found, now)?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    Ok(Synced {
        new_links,
        new_pictures,
        pictures: count,
    })
}

/// Indexes one message from history unless it was read before. Returns how many pictures
/// were saved.
pub async fn index_message(ctx: &BotCtx, guild: GuildId, msg: &Message) -> anyhow::Result<usize> {
    let id = msg.id.get();
    if ctx
        .db
        .call(move |conn| Ok(store::is_indexed(conn, id)?))
        .await?
    {
        return Ok(0);
    }
    Ok(sync_message(ctx, guild, msg).await?.pictures)
}
