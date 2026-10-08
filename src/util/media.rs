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
use serenity::all::Message;

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
    let mut add = |key: &str, url: &str, content_type: Option<&str>| {
        if let Some((kind, mime)) = classify(content_type, key)
            && seen.insert(dedup_key(key))
        {
            found.push(Media {
                url: url.to_string(),
                kind,
                mime,
            });
        }
    };

    for attachment in &msg.attachments {
        add(
            &attachment.url,
            &attachment.url,
            attachment.content_type.as_deref(),
        );
    }

    for embed in &msg.embeds {
        if let Some(image) = &embed.image {
            add(
                &image.url,
                image.proxy_url.as_ref().unwrap_or(&image.url),
                None,
            );
        }
        // Tenor's preview image is a still of the GIF, which comes as the video below.
        let is_tenor = embed
            .provider
            .as_ref()
            .and_then(|p| p.name.as_deref())
            .is_some_and(|name| name.eq_ignore_ascii_case("tenor"));
        if let Some(thumbnail) = &embed.thumbnail
            && !is_tenor
        {
            let url = thumbnail.proxy_url.as_ref().unwrap_or(&thumbnail.url);
            add(&thumbnail.url, url, None);
        }
        if let Some(video) = &embed.video {
            add(
                &video.url,
                video.proxy_url.as_ref().unwrap_or(&video.url),
                None,
            );
        }
    }

    for link in links(&msg.content) {
        add(link, link, None);
    }
    found
}

/// The http(s) links in a message's text, including `<link>` (no preview) and links in
/// brackets.
fn links(text: &str) -> Vec<&str> {
    text.split_whitespace()
        .filter_map(|word| {
            let start = word.find("https://").or_else(|| word.find("http://"))?;
            let link = &word[start..];
            let link = link.split('>').next().unwrap_or(link);
            Some(link.trim_end_matches([')', ']', '.', ',', '!', '?', '"', '\'']))
        })
        .collect()
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
            embed(json!({
                "provider": {"name": "Tenor"},
                "thumbnail": {"url": "https://media.tenor.com/still.png"},
                "video": {"url": "https://media.tenor.com/dance.mp4"},
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
                "https://media.tenor.com/dance.mp4",
            ]
        );
        assert_eq!(found[1].kind, MediaKind::Video);
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
