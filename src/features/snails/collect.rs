//! What gets indexed from a message: its links, and the pictures worth hashing.
//! Pure functions, tested below.

use reqwest::Url;
use serenity::all::Message;

use super::links;
use crate::util::media;

/// Pictures are fetched with this longest side. The hash works on far fewer pixels.
const FETCH_SIZE: u32 = 256;

/// A picture to fingerprint.
#[derive(Debug, Clone, PartialEq)]
pub struct Picture {
    /// A small WebP of it from Discord's media proxy. For a video this is its first frame.
    pub url: String,
    /// The full file, for when the proxy can't serve it.
    pub original: String,
}

impl Picture {
    /// Host and path of the original file, without the query. Discord signs its CDN links
    /// with a query that expires and changes on every read, so this is what stays the same.
    pub fn source(&self) -> String {
        match Url::parse(&self.original) {
            Ok(url) => format!("{}{}", url.host_str().unwrap_or(""), url.path()),
            Err(_) => self.original.clone(),
        }
    }

    /// Who serves the original, like `cdn.discordapp.com` or `pbs.twimg.com`.
    pub fn host(&self) -> String {
        Url::parse(&self.original)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
            .unwrap_or_else(|| "unknown".to_string())
    }
}

/// Whether a message could hold anything to index: links, attachments or previews.
pub fn has_content(msg: &Message) -> bool {
    !message_links(msg).is_empty() || !msg.attachments.is_empty() || !msg.embeds.is_empty()
}

/// The links in a message's text.
pub fn message_links(msg: &Message) -> Vec<String> {
    let mut found = Vec::new();
    for link in media::links(&msg.content) {
        if !found.iter().any(|l| l == link) {
            found.push(link.to_string());
        }
    }
    found
}

/// The pictures in a message, always in the same order (it is stored as `position`):
///
/// - attachments with a size: images, GIFs and videos;
/// - link previews from sites where the post is a picture (X, Reddit, Instagram...), and
///   direct links to images.
///
/// GIFs from Discord's picker (`gifv` previews) are skipped: everyone reuses them. So are
/// previews of videos, music and articles; their link is enough.
pub fn pictures(msg: &Message) -> Vec<Picture> {
    let mut found = Vec::new();
    for a in &msg.attachments {
        let (Some(w), Some(h)) = (a.width, a.height) else {
            continue;
        };
        let small = small_url(&a.proxy_url, w, h);
        push(&mut found, small, &a.url);
    }
    for embed in &msg.embeds {
        let kind = embed.kind.as_deref().unwrap_or("");
        if kind == "gifv" {
            continue;
        }
        // A direct link to an image arrives as an "image" preview with only a thumbnail.
        let from_picture_site = embed.url.as_deref().is_some_and(links::has_pictures);
        if !(kind == "image" || from_picture_site) {
            continue;
        }
        if let Some(image) = &embed.image {
            let proxy = image.proxy_url.as_deref().unwrap_or(&image.url);
            if let (Some(w), Some(h)) = (image.width, image.height) {
                push(&mut found, small_url(proxy, w, h), &image.url);
            }
        } else if let Some(thumb) = &embed.thumbnail
            && kind == "image"
        {
            // Only a direct image link's thumbnail is the picture. On a picture site, a preview
            // without an image is a text post, and its thumbnail is the author's avatar or the
            // subreddit's icon.
            let proxy = thumb.proxy_url.as_deref().unwrap_or(&thumb.url);
            if let (Some(w), Some(h)) = (thumb.width, thumb.height) {
                push(&mut found, small_url(proxy, w, h), &thumb.url);
            }
        }
    }
    found
}

fn push(found: &mut Vec<Picture>, url: String, original: &str) {
    if !found.iter().any(|p| p.url == url) {
        found.push(Picture {
            url,
            original: original.to_string(),
        });
    }
}

/// A link to a small WebP of the picture from Discord's media proxy, with the picture's own
/// shape. The proxy stretches the picture to whatever size is asked, so both sides are given.
/// For a video it returns the first frame.
pub fn small_url(proxy_url: &str, width: u32, height: u32) -> String {
    let (w, h) = fit(width, height, FETCH_SIZE);
    let proxy_url = proxy_url.replacen(
        "https://cdn.discordapp.com/",
        "https://media.discordapp.net/",
        1,
    );
    let Ok(mut url) = Url::parse(&proxy_url) else {
        return proxy_url;
    };
    // Drop size and format parameters the link may already carry, keep the signature.
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !matches!(k.as_ref(), "width" | "height" | "format" | "quality"))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    url.query_pairs_mut()
        .clear()
        .extend_pairs(kept)
        .append_pair("format", "webp")
        .append_pair("width", &w.to_string())
        .append_pair("height", &h.to_string());
    url.to_string()
}

/// `width`×`height` scaled so the longest side is at most `max`, at least 1×1.
fn fit(width: u32, height: u32, max: u32) -> (u32, u32) {
    let longest = width.max(height).max(1);
    if longest <= max {
        return (width.max(1), height.max(1));
    }
    let scale = |side: u32| {
        ((side as u64 * max as u64 + longest as u64 / 2) / longest as u64).max(1) as u32
    };
    (scale(width), scale(height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message(value: serde_json::Value) -> Message {
        let mut base = json!({
            "id": "1", "channel_id": "2", "author": {"id": "3", "username": "u", "discriminator": "0", "avatar": null},
            "content": "", "timestamp": "2026-10-09T00:00:00Z", "edited_timestamp": null, "tts": false,
            "mention_everyone": false, "mentions": [], "mention_roles": [], "attachments": [],
            "embeds": [], "pinned": false, "type": 0
        });
        for (k, v) in value.as_object().unwrap() {
            base[k] = v.clone();
        }
        serde_json::from_value(base).unwrap()
    }

    #[test]
    fn sizes_keep_the_shape() {
        assert_eq!(fit(480, 640, 256), (192, 256));
        assert_eq!(fit(1080, 1086, 256), (255, 256));
        assert_eq!(fit(100, 50, 256), (100, 50));
        assert_eq!(fit(5000, 10, 256), (256, 1));
    }

    #[test]
    fn small_url_asks_the_proxy() {
        let url = small_url(
            "https://cdn.discordapp.com/attachments/1/2/a.mov?ex=6a&is=6b&hm=6c&",
            480,
            640,
        );
        assert_eq!(
            url,
            "https://media.discordapp.net/attachments/1/2/a.mov?ex=6a&is=6b&hm=6c&format=webp&width=192&height=256"
        );
        let url = small_url(
            "https://media.discordapp.net/x.png?width=10&height=10",
            100,
            100,
        );
        assert_eq!(
            url,
            "https://media.discordapp.net/x.png?format=webp&width=100&height=100"
        );
    }

    #[test]
    fn what_gets_hashed() {
        let msg = message(json!({
            "content": "look https://fxtwitter.com/a/status/1 and https://youtu.be/abc https://youtu.be/abc",
            "attachments": [
                {"id": "10", "filename": "cat.png", "size": 1, "url": "https://cdn.discordapp.com/attachments/2/10/cat.png",
                 "proxy_url": "https://media.discordapp.net/attachments/2/10/cat.png", "width": 512, "height": 256},
                {"id": "11", "filename": "notes.txt", "size": 1, "url": "https://cdn.discordapp.com/attachments/2/11/notes.txt",
                 "proxy_url": "https://media.discordapp.net/attachments/2/11/notes.txt"}
            ],
            "embeds": [
                {"type": "rich", "url": "https://fxtwitter.com/a/status/1",
                 "image": {"url": "https://pbs.twimg.com/media/A.jpg", "proxy_url": "https://media.discordapp.net/external/A/A.jpg", "width": 1200, "height": 600}},
                {"type": "video", "url": "https://youtu.be/abc",
                 "thumbnail": {"url": "https://i.ytimg.com/vi/abc/hq.jpg", "proxy_url": "https://media.discordapp.net/external/yt/hq.jpg", "width": 480, "height": 360}},
                {"type": "gifv", "url": "https://klipy.com/gifs/dance",
                 "thumbnail": {"url": "https://static.klipy.com/a.webp", "width": 200, "height": 200}},
                {"type": "rich", "url": "https://www.reddit.com/r/a/comments/b/text_post",
                 "thumbnail": {"url": "https://styles.redditmedia.com/icon.png", "proxy_url": "https://media.discordapp.net/external/i/icon.png", "width": 256, "height": 256}},
                {"type": "image", "url": "https://example.com/meme.png",
                 "thumbnail": {"url": "https://example.com/meme.png", "proxy_url": "https://media.discordapp.net/external/m/meme.png", "width": 300, "height": 300}}
            ]
        }));
        assert_eq!(
            message_links(&msg),
            ["https://fxtwitter.com/a/status/1", "https://youtu.be/abc"]
        );
        assert!(has_content(&msg));
        assert!(!has_content(&message(json!({"content": "just text"}))));
        let found: Vec<String> = pictures(&msg).into_iter().map(|p| p.original).collect();
        assert_eq!(
            found,
            [
                "https://cdn.discordapp.com/attachments/2/10/cat.png",
                "https://pbs.twimg.com/media/A.jpg",
                "https://example.com/meme.png",
            ]
        );
    }

    #[test]
    fn picture_source_ignores_the_signature() {
        let picture = Picture {
            url: String::new(),
            original: "https://cdn.discordapp.com/attachments/2/10/cat.png?ex=1&hm=2".into(),
        };
        assert_eq!(
            picture.source(),
            "cdn.discordapp.com/attachments/2/10/cat.png"
        );
        assert_eq!(picture.host(), "cdn.discordapp.com");
    }
}
