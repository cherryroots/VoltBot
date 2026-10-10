//! Finding images and videos in a Discord message, downloading them, and turning them into
//! images a model can read.
//!
//! A message carries media in three places: attachments, embeds (link previews, GIF pickers),
//! and plain links in the text. [`find_media`] collects all three without duplicates, and
//! [`load_for_model`] downloads one and returns model-ready images: photos as they are, GIFs
//! and videos as grids of frames (see [`frames`](super::frames)).

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::{Context as _, bail};
use base64::Engine as _;
use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use serenity::all::{Http, Message};
use tracing::warn;

use super::frames;

/// Photos bigger than this aren't downloaded. Models refuse very large images anyway.
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
/// GIFs and videos bigger than this aren't downloaded. Discord's own upload limit is 100 MB.
const MAX_VIDEO_BYTES: usize = 100 * 1024 * 1024;
/// How long one download may take in all. The download client only limits connecting and
/// silence, so without this a server that trickles bytes could hold up a reply for long.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);

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

/// An attached file that isn't a picture or video: a PDF, a spreadsheet, a text file...
#[derive(Debug, Clone, PartialEq)]
pub struct FoundFile {
    pub url: String,
    pub name: String,
    /// Like `application/pdf`; `application/octet-stream` when Discord doesn't say.
    pub mime: String,
}

/// The message's attachments that [`find_media`] doesn't take, in order.
pub fn find_files(msg: &Message) -> Vec<FoundFile> {
    msg.attachments
        .iter()
        .filter(|a| classify(a.content_type.as_deref(), &a.url).is_none())
        .map(|a| FoundFile {
            url: a.url.clone(),
            name: a.filename.clone(),
            mime: a
                .content_type
                .as_deref()
                .and_then(|t| t.split(';').next())
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "application/octet-stream".to_string()),
        })
        .collect()
}

/// The http(s) links in a message's text, including `<link>` (no preview) and links in
/// brackets.
pub fn links(text: &str) -> Vec<&str> {
    text.split_whitespace()
        .filter_map(|word| {
            let start = word.find("https://").or_else(|| word.find("http://"))?;
            let link = &word[start..];
            let link = link.split('>').next().unwrap_or(link);
            // Also markdown around the link: ||spoiler||, **bold**, `code`...
            Some(link.trim_end_matches([
                ')', ']', '.', ',', '!', '?', '"', '\'', '|', '*', '_', '~', '`',
            ]))
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

// ---- Safe downloads ----
//
// Links in messages are written by anyone, so a download must not reach the bot's own
// machine or home network (like http://127.0.0.1:8080/admin or the router at 192.168.1.1).
// Three checks, because each closes a different gap:
// - the link itself must not name a local host or a private address (`check_url`);
// - a name is looked up first, and private addresses it points to are dropped
//   (`PublicOnly`), so a public-looking name can't lead home either;
// - every redirect is checked like the first link (the client's redirect policy).

/// The client every download uses, with the checks above. It skips the system proxy: through
/// a proxy, the bot can't see which address a name really leads to.
static SAFE_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent(concat!("Vivy/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        // Same as the main client in main.rs: a server that goes quiet this long fails.
        .read_timeout(Duration::from_secs(5 * 60))
        .dns_resolver(Arc::new(PublicOnly))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                attempt.error("too many redirects")
            } else if let Err(err) = check_url(attempt.url().as_str()) {
                attempt.error(err)
            } else {
                attempt.follow()
            }
        }))
        .no_proxy()
        .build()
        .expect("building the download client")
});

/// The HTTP client for fetching links from messages: refuses local and private addresses,
/// at every redirect too.
pub fn safe_client() -> &'static reqwest::Client {
    &SAFE_CLIENT
}

/// Refuses links that aren't http(s), or that name a local host or a private address.
/// Names are checked again when they're looked up (see `PublicOnly`).
pub fn check_url(url: &str) -> Result<(), String> {
    let url = Url::parse(url).map_err(|err| format!("not a link: {err}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("{} links aren't downloaded", url.scheme()));
    }
    let host = url.host_str().unwrap_or("");
    // IPv6 addresses come in brackets. Odd ways of writing an IPv4 address (2130706433,
    // 0x7f.1) are already turned into the normal form by the parser.
    let refused = match host.trim_start_matches('[').trim_end_matches(']').parse() {
        Ok(ip) => !is_public(ip),
        Err(_) => {
            let name = host.trim_end_matches('.').to_ascii_lowercase();
            name.is_empty() || name == "localhost" || name.ends_with(".localhost")
        }
    };
    if refused {
        return Err(format!("{host} is a local or private address"));
    }
    Ok(())
}

/// Whether an address is on the public internet: not this machine, a home or company
/// network, link-local, multicast or otherwise reserved.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, ..] = ip.octets();
            !(ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_multicast()
                || a == 0
                // Carrier-grade NAT, 100.64.0.0/10.
                || (a == 100 && (64..128).contains(&b))
                // IETF protocol assignments, 192.0.0.0/24.
                || (a == 192 && b == 0 && ip.octets()[2] == 0)
                // Benchmarking, 198.18.0.0/15.
                || (a == 198 && (b == 18 || b == 19))
                // Reserved, 240.0.0.0/4.
                || a >= 240)
        }
        IpAddr::V6(ip) => {
            // An IPv4 address inside an IPv6 one is checked as IPv4, since it can lead to
            // it: mapped (::ffff:127.0.0.1), IPv4-compatible (::127.0.0.1), NAT64
            // (64:ff9b::127.0.0.1) and 6to4 (2002:7f00:1::, the IPv4 in segments 1-2).
            let s = ip.segments();
            let o = ip.octets();
            let tail = Ipv4Addr::new(o[12], o[13], o[14], o[15]);
            if let Some(v4) = ip.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            // IPv4-compatible: the first 96 bits are zero (:: and ::1 are checked below).
            if s[..6] == [0; 6] && !(ip.is_unspecified() || ip.is_loopback()) {
                return is_public(IpAddr::V4(tail));
            }
            // NAT64, 64:ff9b::/96. Translated to the IPv4 it holds, so check that.
            if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                return is_public(IpAddr::V4(tail));
            }
            // 6to4, 2002::/16.
            if s[0] == 0x2002 {
                return is_public(IpAddr::V4(Ipv4Addr::new(o[2], o[3], o[4], o[5])));
            }
            let first = s[0];
            !(ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_multicast()
                // Unique local, fc00::/7.
                || (first & 0xfe00) == 0xfc00
                // Link-local, fe80::/10.
                || (first & 0xffc0) == 0xfe80)
        }
    }
}

/// Looks names up like normal, but keeps only public addresses.
struct PublicOnly;

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            // The port is replaced by the link's own; lookup_host just needs one.
            let found: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|address| is_public(address.ip()))
                .collect();
            if found.is_empty() {
                return Err(format!("{host} only leads to local or private addresses").into());
            }
            let addrs: Addrs = Box::new(found.into_iter());
            Ok(addrs)
        })
    }
}

/// Downloads `url`, giving up once it is bigger than `max_bytes`. Local and private
/// addresses are refused (see [`safe_client`]).
pub async fn download(url: &str, max_bytes: usize) -> anyhow::Result<Vec<u8>> {
    check_url(url).map_err(anyhow::Error::msg)?;
    fetch(safe_client(), url, max_bytes).await
}

/// The download itself, without the address checks (tests serve files from 127.0.0.1).
async fn fetch(client: &reqwest::Client, url: &str, max_bytes: usize) -> anyhow::Result<Vec<u8>> {
    let mut response = client
        .get(url)
        .timeout(DOWNLOAD_TIMEOUT)
        .send()
        .await?
        .error_for_status()?;
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
pub async fn load_for_model(media: &Media) -> anyhow::Result<Vec<ModelImage>> {
    let max_bytes = match media.kind {
        MediaKind::Image => MAX_IMAGE_BYTES,
        MediaKind::Gif | MediaKind::Video => MAX_VIDEO_BYTES,
    };
    let data = download(&media.url, max_bytes)
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
        assert_eq!(
            links("||https://x.com/a/status/123|| **https://b.com/c** `https://d.com/e`"),
            [
                "https://x.com/a/status/123",
                "https://b.com/c",
                "https://d.com/e"
            ]
        );
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

    #[test]
    fn finds_other_files() {
        let mut msg = Message::default();
        msg.attachments = vec![
            attachment("https://cdn.discordapp.com/a/1/pic.png", Some("image/png")),
            attachment(
                "https://cdn.discordapp.com/a/2/notes.txt",
                Some("text/plain; charset=utf-8"),
            ),
            attachment("https://cdn.discordapp.com/a/3/data.bin", None),
        ];
        let files = find_files(&msg);
        let found: Vec<(&str, &str)> = files
            .iter()
            .map(|f| (f.url.as_str(), f.mime.as_str()))
            .collect();
        assert_eq!(
            found,
            [
                ("https://cdn.discordapp.com/a/2/notes.txt", "text/plain"),
                (
                    "https://cdn.discordapp.com/a/3/data.bin",
                    "application/octet-stream"
                ),
            ]
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
        assert_eq!(fetch(&client, &url, 100).await.unwrap(), vec![7; 100]);

        let url = serve_once(vec![7; 101]).await;
        let err = fetch(&client, &url, 100).await.unwrap_err();
        assert!(err.to_string().contains("limit"), "{err}");
    }

    #[tokio::test]
    async fn download_refuses_local_addresses() {
        let url = serve_once(vec![7; 10]).await;
        let err = download(&url, 100).await.unwrap_err();
        assert!(err.to_string().contains("private"), "{err}");
        // A name that leads to this machine is refused when it's looked up.
        let err = fetch(safe_client(), "http://localhost:1/x.png", 100)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("private"), "{err:#}");
    }

    #[test]
    fn checks_addresses() {
        for bad in [
            "http://127.0.0.1/x",
            "http://localhost:8080/",
            "http://LOCALHOST./",
            "http://admin.localhost/",
            "http://10.0.0.5/",
            "http://192.168.1.1/",
            "http://172.16.0.1/",
            "http://169.254.169.254/latest/meta-data",
            "http://100.64.0.1/",
            "http://0.0.0.0/",
            "http://2130706433/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[fd00::1]/",
            "http://[fe80::1]/",
            "http://192.0.0.8/",
            "http://198.18.0.1/",
            "http://198.19.255.255/",
            "http://[::127.0.0.1]/",
            "http://[::a00:1]/",
            "http://[64:ff9b::127.0.0.1]/",
            "http://[64:ff9b::a9fe:a9fe]/",
            "http://[2002:7f00:1::]/",
            "http://[2002:c0a8:101::1]/",
            "file:///etc/passwd",
            "ftp://example.com/x",
        ] {
            assert!(check_url(bad).is_err(), "{bad} should be refused");
        }
        for good in [
            "https://cdn.discordapp.com/a.png",
            "http://8.8.8.8/",
            "https://[2606:4700::1111]/",
            "http://198.20.0.1/",
            "http://192.0.1.1/",
            "http://[64:ff9b::808:808]/",
            "http://[2002:808:808::]/",
        ] {
            assert!(check_url(good).is_ok(), "{good} should be allowed");
        }
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
