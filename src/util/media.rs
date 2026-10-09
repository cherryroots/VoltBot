//! Finding images and videos in a Discord message, downloading them, and turning them into
//! images a model can read.
//!
//! A message carries media in three places: attachments, embeds (link previews, GIF pickers),
//! and plain links in the text. [`find_media`] collects all three without duplicates, and
//! [`load_for_model`] downloads one and returns model-ready images: photos as they are, GIFs
//! and videos as grids of frames (see [`frames`](super::frames)).

use std::collections::HashSet;

use anyhow::{Context as _, bail};
use base64::Engine as _;
use reqwest::Url;
use serenity::all::{Http, Message};
use std::time::Duration;
use tracing::warn;

use super::frames;

/// Photos bigger than this aren't downloaded. Models refuse very large images anyway.
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
/// GIFs and videos bigger than this aren't downloaded. Discord's own upload limit is 100 MB.
const MAX_VIDEO_BYTES: usize = 100 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MediaKind {
    /// A still image a model reads as it is.
    Image,
    /// An animated GIF, sent to the model as frames.
    Gif,
    Video,
}

/// A piece of media found in a message.
#[derive(Debug, Clone, PartialEq)]
pub struct Media {
    /// Where to download it from.
    pub url: String,
    pub kind: MediaKind,
    /// The file type, like `image/png`.
    pub mime: &'static str,
}

/// The file types the bot understands: extension, MIME type, kind.
const TYPES: &[(&str, &str, MediaKind)] = &[
    ("jpg", "image/jpeg", MediaKind::Image),
    ("jpeg", "image/jpeg", MediaKind::Image),
    ("png", "image/png", MediaKind::Image),
    ("webp", "image/webp", MediaKind::Image),
    ("gif", "image/gif", MediaKind::Gif),
    ("mp4", "video/mp4", MediaKind::Video),
    ("webm", "video/webm", MediaKind::Video),
    ("mov", "video/quicktime", MediaKind::Video),
];

/// Works out what a file is: from the content type Discord reports when there is one,
/// otherwise from the URL's file extension.
fn classify(content_type: Option<&str>, url: &str) -> Option<(MediaKind, &'static str)> {
    if let Some(content_type) = content_type {
        // "image/png; charset=..." -> "image/png"
        let mime = content_type.split(';').next().unwrap_or("").trim();
        return TYPES
            .iter()
            .find(|(_, known, _)| known.eq_ignore_ascii_case(mime))
            .map(|&(_, mime, kind)| (kind, mime));
    }
    let path = Url::parse(url).ok()?.path().to_string();
    let file = path.rsplit('/').next()?;
    let (_, extension) = file.rsplit_once('.')?;
    TYPES
        .iter()
        .find(|(known, _, _)| known.eq_ignore_ascii_case(extension))
        .map(|&(_, mime, kind)| (kind, mime))
}

/// Host and path, without the query string. Discord adds changing query parameters to
/// its CDN links, so this is what identifies a file.
fn dedup_key(url: &str) -> String {
    match Url::parse(url) {
        Ok(url) => format!("{}{}", url.host_str().unwrap_or(""), url.path()),
        Err(_) => url.to_string(),
    }
}

/// Every image, GIF and video in the message, in order: attachments, embeds, then links in
/// the text. Each file appears once.
pub fn find_media(msg: &Message) -> Vec<Media> {
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    // `key` is the file's original link; `url` is where to download it (Discord's proxy for
    // embeds, which also works when the original site blocks bots).
    // Returns whether the file is media the bot understands (even if it was already added).
    let mut add = |key: &str, url: &str, content_type: Option<&str>| -> bool {
        let Some((kind, mime)) = classify(content_type, key) else {
            return false;
        };
        if seen.insert(dedup_key(key)) {
            found.push(Media {
                url: url.to_string(),
                kind,
                mime,
            });
        }
        true
    };

    for attachment in &msg.attachments {
        add(
            &attachment.url,
            &attachment.url,
            attachment.content_type.as_deref(),
        );
    }

    for embed in &msg.embeds {
        // GIFs from Discord's picker (Klipy, Tenor, Giphy) arrive as a link to the GIF's page
        // plus a "gifv" embed: an MP4 in `video` and a still of it in `thumbnail`. Discord
        // documents gifv as a GIF rendered as a video, so its video is an MP4 even when the
        // link has no file extension.
        let is_gifv = embed.kind.as_deref() == Some("gifv");
        let mut has_video = false;
        if let Some(video) = &embed.video {
            let url = video.proxy_url.as_ref().unwrap_or(&video.url);
            has_video = add(&video.url, url, is_gifv.then_some("video/mp4"));
        }
        if let Some(image) = &embed.image {
            add(
                &image.url,
                image.proxy_url.as_ref().unwrap_or(&image.url),
                None,
            );
        }
        // A thumbnail next to a video is a still of that video, so it adds nothing. Alone it's
        // a link preview image (a YouTube thumbnail, an article picture), which is worth seeing.
        if let Some(thumbnail) = &embed.thumbnail
            && !has_video
        {
            let url = thumbnail.proxy_url.as_ref().unwrap_or(&thumbnail.url);
            add(&thumbnail.url, url, None);
        }
    }

    for link in links(&msg.content) {
        add(link, link, None);
    }
    found
}

/// The http(s) links in a message's text, including `<link>` (no preview) and links in
/// brackets.
pub fn links(text: &str) -> Vec<&str> {
    text.split_whitespace()
        .filter_map(|word| {
            let start = word.find("https://").or_else(|| word.find("http://"))?;
            let link = &word[start..];
            let link = link.split('>').next().unwrap_or(link);
            Some(link.trim_end_matches([')', ']', '.', ',', '!', '?', '"', '\'']))
        })
        .collect()
}

/// Discord sometimes adds link previews (and the GIF of a GIF link) a moment after the
/// message arrives. When a message has links but no previews yet, wait and read it again.
pub async fn with_previews(http: &Http, msg: &Message) -> Message {
    let has_link = msg.content.contains("https://") || msg.content.contains("http://");
    if !has_link || !msg.embeds.is_empty() {
        return msg.clone();
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    match msg.channel_id.message(http, msg.id).await {
        Ok(fresh) => fresh,
        Err(err) => {
            warn!("couldn't read the message again for link previews: {err}");
            msg.clone()
        }
    }
}

/// Downloads `url`, giving up once it is bigger than `max_bytes`.
pub async fn download(
    client: &reqwest::Client,
    url: &str,
    max_bytes: usize,
) -> anyhow::Result<Vec<u8>> {
    let mut response = client.get(url).send().await?.error_for_status()?;
    if let Some(length) = response.content_length()
        && length > max_bytes as u64
    {
        bail!(
            "the file is {} MB, over the {} MB limit",
            length >> 20,
            max_bytes >> 20
        );
    }
    // The reported length can be missing or wrong, so count while reading too.
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        data.extend_from_slice(&chunk);
        if data.len() > max_bytes {
            bail!("the file is over the {} MB limit", max_bytes >> 20);
        }
    }
    Ok(data)
}

/// An image ready to send to a model.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelImage {
    pub mime: &'static str,
    pub data: Vec<u8>,
}

impl ModelImage {
    /// The image as a `data:` URL, the form OpenAI accepts inline.
    pub fn data_url(&self) -> String {
        let encoded = base64::engine::general_purpose::STANDARD.encode(&self.data);
        format!("data:{};base64,{encoded}", self.mime)
    }
}

/// Downloads `media` and returns the images a model should see: the image itself, or frame
/// grids for a GIF or video.
pub async fn load_for_model(
    client: &reqwest::Client,
    media: &Media,
) -> anyhow::Result<Vec<ModelImage>> {
    let max_bytes = match media.kind {
        MediaKind::Image => MAX_IMAGE_BYTES,
        MediaKind::Gif | MediaKind::Video => MAX_VIDEO_BYTES,
    };
    let data = download(client, &media.url, max_bytes)
        .await
        .with_context(|| format!("downloading {}", media.url))?;
    if media.kind == MediaKind::Image {
        return Ok(vec![ModelImage {
            mime: media.mime,
            data,
        }]);
    }
    let grids = frames::frame_grids(&data, frames::Layout::for_kind(media.kind))
        .await
        .with_context(|| format!("reading frames from {}", media.url))?;
    Ok(grids
        .into_iter()
        .map(|data| ModelImage {
            mime: "image/png",
            data,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use serenity::all::{Attachment, Embed};

    fn attachment(url: &str, content_type: Option<&str>) -> Attachment {
        serde_json::from_value(json!({
            "id": "1", "filename": "file", "size": 10, "url": url, "proxy_url": url,
            "content_type": content_type,
        }))
        .unwrap()
    }

    fn embed(value: serde_json::Value) -> Embed {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn classifies_by_content_type_then_extension() {
        assert_eq!(
            classify(Some("image/png"), "https://x/file"),
            Some((MediaKind::Image, "image/png"))
        );
        assert_eq!(
            classify(Some("video/quicktime"), "https://x/a.png"),
            Some((MediaKind::Video, "video/quicktime"))
        );
        // Discord's content type wins over a misleading name.
        assert_eq!(classify(Some("text/plain"), "https://x/a.png"), None);
        assert_eq!(
            classify(None, "https://cdn.x/a/B.JPG?ex=1&hm=2"),
            Some((MediaKind::Image, "image/jpeg"))
        );
        assert_eq!(
            classify(None, "https://x/a.gif"),
            Some((MediaKind::Gif, "image/gif"))
        );
        assert_eq!(classify(None, "https://x/watch?v=1"), None);
        assert_eq!(classify(None, "not a url.png"), None);
    }

    #[test]
    fn finds_links_in_text() {
        assert_eq!(
            links("see <https://a.com/x.png> and (http://b.com/y.mp4). also https://c.com/z.gif!"),
            [
                "https://a.com/x.png",
                "http://b.com/y.mp4",
                "https://c.com/z.gif"
            ]
        );
        assert!(links("no links here").is_empty());
    }

    #[test]
    fn finds_media_everywhere_once() {
        let mut msg = Message::default();
        msg.content =
            "look https://site.com/cat.png https://cdn.discordapp.com/a/1/pic.png?ex=2".into();
        msg.attachments = vec![
            attachment(
                "https://cdn.discordapp.com/a/1/pic.png?ex=1",
                Some("image/png"),
            ),
            attachment(
                "https://cdn.discordapp.com/a/2/clip.mov",
                Some("video/quicktime"),
            ),
            attachment(
                "https://cdn.discordapp.com/a/3/notes.txt",
                Some("text/plain"),
            ),
        ];
        msg.embeds = vec![
            // The preview of the cat link: same file, downloaded through Discord's proxy.
            embed(json!({
                "url": "https://site.com/cat.png",
                "image": {"url": "https://site.com/cat.png", "proxy_url": "https://proxy/cat.png"},
            })),
            // A GIF from the picker: Klipy's video link has no extension, but gifv means MP4.
            embed(json!({
                "type": "gifv",
                "url": "https://klipy.com/gifs/dance",
                "provider": {"name": "KLIPY", "url": "https://klipy.com"},
                "thumbnail": {"url": "https://static.klipy.com/ii/dance.webp"},
                "video": {"url": "https://static.klipy.com/ii/dance", "proxy_url": "https://proxy/dance.mp4"},
            })),
            // A YouTube link: the video is a player page, so the thumbnail is what's kept.
            embed(json!({
                "type": "video",
                "thumbnail": {"url": "https://i.ytimg.com/vi/x/hqdefault.jpg"},
                "video": {"url": "https://www.youtube.com/embed/x"},
            })),
        ];

        let found = find_media(&msg);
        let urls: Vec<&str> = found.iter().map(|m| m.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://cdn.discordapp.com/a/1/pic.png?ex=1",
                "https://cdn.discordapp.com/a/2/clip.mov",
                "https://proxy/cat.png",
                "https://proxy/dance.mp4",
                "https://i.ytimg.com/vi/x/hqdefault.jpg",
            ]
        );
        assert_eq!(found[1].kind, MediaKind::Video);
        assert_eq!(
            (found[3].kind, found[3].mime),
            (MediaKind::Video, "video/mp4")
        );
    }

    /// Serves `body` once over plain HTTP and returns its URL.
    async fn serve_once(body: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let _ = socket.read(&mut request).await;
            let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(&body).await;
        });
        format!("http://{address}/file.png")
    }

    #[tokio::test]
    async fn download_respects_the_limit() {
        let client = reqwest::Client::new();
        let url = serve_once(vec![7; 100]).await;
        assert_eq!(download(&client, &url, 100).await.unwrap(), vec![7; 100]);

        let url = serve_once(vec![7; 101]).await;
        let err = download(&client, &url, 100).await.unwrap_err();
        assert!(err.to_string().contains("limit"), "{err}");
    }

    #[test]
    fn data_url() {
        let image = ModelImage {
            mime: "image/png",
            data: vec![1, 2, 3],
        };
        assert_eq!(image.data_url(), "data:image/png;base64,AQID");
    }
}
