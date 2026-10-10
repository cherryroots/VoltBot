//! Link keys: the same post shared twice gives the same key, whichever mirror it came
//! through and whatever tracking junk it carries.
//!
//! Known sites key on the post's id: `x:1844012345678901234`, `youtube:dQw4w9WgXcQ`.
//! Everything else keys on host + path + the query parameters that matter.
//! Pure functions, tested below.

use reqwest::Url;

/// Sites whose posts are pictures. Their link previews are hashed too, because people often
/// repost just the picture from a tweet. Other sites (video, music, articles) only get the
/// link key.
const PICTURE_SITES: &[&str] = &[
    "x",
    "bsky",
    "threads",
    "instagram",
    "reddit",
    "pixiv",
    "tumblr",
    "imgur",
    "pinterest",
];

/// Links that are never snails: GIFs from Discord's picker get reused all the time, and a
/// link to another Discord message is a reference, not a repost.
const IGNORED_SITES: &[&str] = &["gif", "discord"];

/// Embed fixers that also answer on one-letter subdomains: d.fxtwitter.com (just the media),
/// g.fxbsky.app (gallery), c.vxtwitter.com...
const MIRRORS: &[&str] = &[
    "fxtwitter.com",
    "vxtwitter.com",
    "fixupx.com",
    "fixvx.com",
    "fxbsky.app",
    "vxbsky.app",
    "ddinstagram.com",
    "kkinstagram.com",
    "uuinstagram.com",
    "vxinstagram.com",
    "instagramez.com",
    "vxtiktok.com",
    "rxddit.com",
    "vxreddit.com",
    "phixiv.net",
];

/// Which site a host belongs to. Embed-fixer mirrors count as the site they mirror.
fn site_of_host(host: &str) -> Option<&'static str> {
    // d.fxtwitter.com -> fxtwitter.com
    let host = match host.split_once('.') {
        Some((sub, rest)) if sub.len() == 1 && MIRRORS.contains(&rest) => rest,
        _ => host,
    };
    Some(match host {
        "twitter.com" | "x.com" | "fxtwitter.com" | "vxtwitter.com" | "fixupx.com"
        | "fixvx.com" | "twittpr.com" | "girlcockx.com" | "nitter.net" | "xcancel.com"
        | "nitter.poast.org" | "mobile.twitter.com" | "mobile.x.com" => "x",
        "bsky.app" | "fxbsky.app" | "vxbsky.app" | "bskx.app" | "bskyx.app" | "bsyy.app" => "bsky",
        "threads.net" | "threads.com" | "fixthreads.net" | "vxthreads.net" => "threads",
        "instagram.com" | "ddinstagram.com" | "instagramez.com" | "kkinstagram.com"
        | "uuinstagram.com" | "vxinstagram.com" | "kirkstagram.com" => "instagram",
        "tiktok.com" | "vxtiktok.com" | "tnktok.com" | "tfxktok.com" | "tiktxk.com" => "tiktok",
        "youtube.com" | "music.youtube.com" | "youtube-nocookie.com" | "youtu.be" => "youtube",
        "reddit.com" | "old.reddit.com" | "new.reddit.com" | "np.reddit.com" | "sh.reddit.com"
        | "rxddit.com" | "vxreddit.com" | "redditez.com" | "redd.it" => "reddit",
        "facebook.com" | "fb.com" | "facebookez.com" => "facebook",
        "pixiv.net" | "phixiv.net" | "ppxiv.net" => "pixiv",
        "tumblr.com" | "tpmblr.com" => "tumblr",
        "twitch.tv" | "clips.twitch.tv" => "twitch",
        "imgur.com" | "i.imgur.com" => "imgur",
        "streamable.com" => "streamable",
        "open.spotify.com" => "spotify",
        "pinterest.com" => "pinterest",
        "discord.com" | "ptb.discord.com" | "canary.discord.com" | "discordapp.com" => "discord",
        "cdn.discordapp.com" | "media.discordapp.net" => "discord-cdn",
        "klipy.com" | "tenor.com" | "giphy.com" => "gif",
        _ if host.ends_with(".tumblr.com") => "tumblr",
        _ => return None,
    })
}

/// The host without `www.` or `m.`, lowercase.
fn host(url: &Url) -> Option<String> {
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.trim_start_matches("www.").trim_start_matches("m.");
    Some(host.to_string())
}

/// The path segments, without empty ones.
fn segments(url: &Url) -> Vec<&str> {
    url.path_segments()
        .map(|s| s.filter(|p| !p.is_empty()).collect())
        .unwrap_or_default()
}

/// The site a link belongs to, like `"x"`, or None for sites we don't know.
pub fn site(link: &str) -> Option<&'static str> {
    let url = Url::parse(link).ok()?;
    site_of_host(&host(&url)?)
}

/// Whether a link preview from this site is worth hashing. See [`PICTURE_SITES`].
pub fn has_pictures(link: &str) -> bool {
    site(link).is_some_and(|s| PICTURE_SITES.contains(&s))
}

/// The path segment right after `marker`: `after(.., "status")` in /user/status/123 is "123".
fn after<'a>(segs: &[&'a str], marker: &str) -> Option<&'a str> {
    let i = segs.iter().position(|s| *s == marker)?;
    segs.get(i + 1).copied()
}

/// Whether `s` is all digits, like a post id on many sites.
fn is_number(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// The id of a post, video or file on a known site. None when the link isn't one post
/// (a profile, a search...); it then gets the plain host + path key.
fn post_id(site: &str, host: &str, segs: &[&str], url: &Url) -> Option<String> {
    let param = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.to_string())
    };
    let first = segs.first().copied();
    let id = match site {
        "x" => after(segs, "status")?.to_string(),
        // Bluesky post ids are only unique per account, so keep the handle.
        "bsky" => format!("{}/{}", after(segs, "profile")?, after(segs, "post")?),
        "threads" => after(segs, "post")?.to_string(),
        "instagram" => {
            let i = segs
                .iter()
                .position(|s| matches!(*s, "p" | "reel" | "reels" | "tv"))?;
            segs.get(i + 1)?.to_string()
        }
        "tiktok" => after(segs, "video").or(after(segs, "photo"))?.to_string(),
        "youtube" if host == "youtu.be" => first?.to_string(),
        "youtube" => match first {
            Some("shorts" | "live" | "embed" | "v") => segs.get(1)?.to_string(),
            _ => param("v")?,
        },
        "reddit" if host == "redd.it" => first?.to_string(),
        "reddit" => after(segs, "comments")?.to_string(),
        "facebook" => after(segs, "posts")
            .or(after(segs, "videos"))
            .or(after(segs, "reel"))
            .map(str::to_string)
            .or_else(|| param("v"))
            .or_else(|| param("story_fbid"))?,
        "pixiv" => after(segs, "artworks")?.to_string(),
        // <blog>.tumblr.com/post/<id> or tumblr.com/<blog>/<id>. Post ids are global numbers.
        // Anything else (tumblr.com/tagged/cats, the media CDN...) isn't one post.
        "tumblr" => {
            let id = if host == "tumblr.com" || host == "tpmblr.com" {
                segs.get(1).copied()
            } else {
                after(segs, "post")
            };
            id.filter(|id| is_number(id))?.to_string()
        }
        // clips.twitch.tv/<slug>, or clips.twitch.tv/embed?clip=<slug> in a player.
        "twitch" if host == "clips.twitch.tv" => match first? {
            "embed" => format!("clip/{}", param("clip")?),
            slug => format!("clip/{slug}"),
        },
        "twitch" => match (after(segs, "clip"), after(segs, "videos")) {
            (Some(clip), _) => format!("clip/{clip}"),
            (None, Some(video)) => format!("video/{video}"),
            _ => return None,
        },
        // i.imgur.com/<id>.jpg and imgur.com/<id> are the same picture. Longer paths
        // (imgur.com/t/<tag>, imgur.com/user/<name>...) aren't one picture.
        "imgur" => match segs {
            ["a" | "gallery", id, ..] => format!("album/{id}"),
            [file] => file.split('.').next()?.to_string(),
            _ => return None,
        },
        // streamable.com/<id>, or streamable.com/e/<id> in a player.
        "streamable" => match segs {
            [id] | ["e", id] => id.to_string(),
            _ => return None,
        },
        // open.spotify.com/track/<id>, also with /intl-de/ or /embed/ in front, and old
        // playlist links: /user/<name>/playlist/<id>.
        "spotify" => {
            let s: Vec<&str> = segs
                .iter()
                .copied()
                .filter(|s| !s.starts_with("intl-") && *s != "embed")
                .collect();
            match s.as_slice() {
                [
                    kind @ ("track" | "album" | "playlist" | "artist" | "episode" | "show"),
                    id,
                ] => {
                    format!("{kind}/{id}")
                }
                ["user", _, "playlist", id] => format!("playlist/{id}"),
                _ => return None,
            }
        }
        "pinterest" => after(segs, "pin")?.to_string(),
        "discord" => {
            after(segs, "channels")?;
            segs[1..].join("/")
        }
        // The same file from either host; the query is a signature that changes daily.
        "discord-cdn" => segs.join("/"),
        // klipy.com/gifs/<slug>, tenor.com/view/<slug>-<id>, giphy.com/gifs/<slug>-<id>
        "gif" => segs.last()?.to_string(),
        _ => return None,
    };
    Some(format!("{site}:{id}"))
}

/// Query parameters that are tracking on every site. Parameters starting with `utm_` too.
const JUNK_PARAMS: &[&str] = &["fbclid", "gclid", "igsh", "igshid", "mibextid", "ref_src"];

/// Query parameters that are junk only on some sites. Elsewhere a short name like `t` can
/// matter: a forum's `?t=123` is a thread.
fn site_junk(site: Option<&str>, host: &str) -> &'static [&'static str] {
    match site {
        Some("x") => &["s", "t"],
        Some("youtube") => &["si", "feature", "pp", "t"],
        Some("spotify") => &["si", "context"],
        Some("reddit") => &["rdt", "share_id", "context", "ref", "ref_source"],
        Some("threads") => &["xmt"],
        Some("tiktok") => &["is_from_webapp", "sender_device", "web_id", "_r", "_t"],
        // X's picture server: ?format=jpg&name=large
        _ if host == "pbs.twimg.com" => &["format", "name"],
        _ => &[],
    }
}

/// The key a link is stored under, or None when it isn't a web link or should never count
/// as a snail (see [`IGNORED_SITES`]).
pub fn link_key(link: &str) -> Option<String> {
    let url = Url::parse(link).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = host(&url)?;
    let segs = segments(&url);
    let site = site_of_host(&host);
    if site.is_some_and(|s| IGNORED_SITES.contains(&s)) {
        return None;
    }
    if let Some(site) = site
        && let Some(key) = post_id(site, &host, &segs, &url)
    {
        return Some(key);
    }

    let junk = site_junk(site, &host);
    let mut params: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| {
            !JUNK_PARAMS.contains(&k.as_ref())
                && !junk.contains(&k.as_ref())
                && !k.starts_with("utm_")
        })
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    params.sort();
    let mut key = format!("{host}/{}", segs.join("/"));
    for (i, (k, v)) in params.iter().enumerate() {
        key.push(if i == 0 { '?' } else { '&' });
        key.push_str(&format!("{k}={v}"));
    }
    Some(key)
}

/// Short links that hide the post id. Following the redirect gives the full link, which is
/// far cheaper than downloading and hashing a picture.
pub fn needs_resolving(link: &str) -> bool {
    let Ok(url) = Url::parse(link) else {
        return false;
    };
    let Some(host) = host(&url) else {
        return false;
    };
    let segs = segments(&url);
    matches!(
        host.as_str(),
        "vm.tiktok.com"
            | "vt.tiktok.com"
            | "pin.it"
            | "fb.watch"
            | "t.co"
            | "bit.ly"
            | "on.soundcloud.com"
            | "spotify.link"
    ) || (host == "tiktok.com" && segs.first() == Some(&"t"))
        || (host.ends_with("reddit.com") && segs.get(2) == Some(&"s"))
        || (host == "facebook.com" && segs.first() == Some(&"share"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn same(a: &str, b: &str) -> bool {
        link_key(a).unwrap() == link_key(b).unwrap()
    }

    #[test]
    fn mirrors_share_a_key() {
        let pairs = [
            (
                "https://x.com/user/status/1844012345678901234",
                "https://fxtwitter.com/user/status/1844012345678901234/photo/1",
            ),
            (
                "https://twitter.com/user/status/1844012345678901234?s=20",
                "https://vxtwitter.com/other/status/1844012345678901234",
            ),
            (
                "https://fixupx.com/a/status/1844012345678901234",
                "https://xcancel.com/a/status/1844012345678901234",
            ),
            (
                "https://girlcockx.com/a/status/1844012345678901234",
                "https://x.com/a/status/1844012345678901234",
            ),
            (
                "https://bsky.app/profile/cherry.bsky.social/post/3l5abcxyz",
                "https://fxbsky.app/profile/cherry.bsky.social/post/3l5abcxyz",
            ),
            (
                "https://www.threads.net/@user/post/C9xYz12AbCd",
                "https://www.threads.com/@user/post/C9xYz12AbCd?xmt=abc",
            ),
            (
                "https://www.instagram.com/reel/C1a2B3c4D5e/?igsh=xyz",
                "https://ddinstagram.com/reel/C1a2B3c4D5e/",
            ),
            (
                "https://www.instagram.com/p/C1a2B3c4D5e/",
                "https://kkinstagram.com/reels/C1a2B3c4D5e",
            ),
            (
                "https://kirkstagram.com/p/C1a2B3c4D5e/",
                "https://www.instagram.com/p/C1a2B3c4D5e/",
            ),
            (
                "https://www.tiktok.com/@user/video/7412345678901234567?is_from_webapp=1",
                "https://vxtiktok.com/@user/video/7412345678901234567",
            ),
            (
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=42s",
                "https://youtu.be/dQw4w9WgXcQ?si=abc",
            ),
            (
                "https://youtube.com/shorts/abcdEFGhijk",
                "https://m.youtube.com/watch?v=abcdEFGhijk&feature=share",
            ),
            (
                "https://music.youtube.com/watch?v=dQw4w9WgXcQ",
                "https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ",
            ),
            (
                "https://www.reddit.com/r/memes/comments/1abcde/funny_title/",
                "https://old.reddit.com/r/memes/comments/1abcde/",
            ),
            (
                "https://redd.it/1abcde",
                "https://rxddit.com/r/memes/comments/1abcde/x",
            ),
            (
                "https://www.facebook.com/user/posts/123456789?mibextid=abc",
                "https://m.facebook.com/user/posts/123456789",
            ),
            (
                "https://www.facebook.com/watch/?v=987654321",
                "https://www.facebook.com/user/videos/987654321/",
            ),
            (
                "https://www.pixiv.net/en/artworks/12345678",
                "https://phixiv.net/artworks/12345678",
            ),
            (
                "https://someblog.tumblr.com/post/7012345678/title-here",
                "https://www.tumblr.com/someblog/7012345678",
            ),
            (
                "https://clips.twitch.tv/FunnyClipSlug-abc",
                "https://www.twitch.tv/streamer/clip/FunnyClipSlug-abc?filter=clips",
            ),
            (
                "https://i.imgur.com/AbCdEf1.jpeg",
                "https://imgur.com/AbCdEf1",
            ),
            (
                "https://streamable.com/abc123",
                "https://streamable.com/abc123?src=player",
            ),
            (
                "https://open.spotify.com/intl-de/track/4uLU6hMCjMI75M1A2tKUQC?si=x",
                "https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC",
            ),
            (
                "https://www.pinterest.com/pin/1234567890/",
                "https://pinterest.com/pin/1234567890",
            ),
            (
                "https://cdn.discordapp.com/attachments/1/2/cat.png?ex=a&is=b&hm=c",
                "https://media.discordapp.net/attachments/1/2/cat.png?ex=d&is=e&hm=f&width=400",
            ),
            (
                "https://example.com/cat.jpeg",
                "https://example.com/cat.jpeg?utm_source=discord",
            ),
            (
                "https://example.com/page?fbclid=abc",
                "https://example.com/page?gclid=def",
            ),
            (
                "https://www.youtube.com/@channel?si=abc",
                "https://www.youtube.com/@channel",
            ),
            (
                "https://pbs.twimg.com/media/A.jpg?format=jpg&name=small",
                "https://pbs.twimg.com/media/A.jpg?name=large",
            ),
            ("https://x.com/user?s=20&t=abc", "https://x.com/user"),
            (
                "https://d.fxtwitter.com/a/status/1844012345678901234",
                "https://x.com/a/status/1844012345678901234",
            ),
            (
                "https://c.vxtwitter.com/a/status/1844012345678901234",
                "https://i.fxtwitter.com/a/status/1844012345678901234",
            ),
            (
                "https://g.fxbsky.app/profile/cherry.bsky.social/post/3l5abcxyz",
                "https://bsky.app/profile/cherry.bsky.social/post/3l5abcxyz",
            ),
            (
                "https://d.ddinstagram.com/p/C1a2B3c4D5e/",
                "https://www.instagram.com/p/C1a2B3c4D5e/",
            ),
            (
                "https://clips.twitch.tv/embed?clip=FunnyClipSlug-abc&parent=example.com",
                "https://clips.twitch.tv/FunnyClipSlug-abc",
            ),
            (
                "https://streamable.com/e/abc123",
                "https://streamable.com/abc123",
            ),
            (
                "https://open.spotify.com/embed/track/4uLU6hMCjMI75M1A2tKUQC",
                "https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC",
            ),
            (
                "https://open.spotify.com/user/someone/playlist/37i9dQZF1DXcBWIGoYBM5M",
                "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M",
            ),
        ];
        for (a, b) in pairs {
            assert!(
                same(a, b),
                "{a} and {b} should match: {:?} {:?}",
                link_key(a),
                link_key(b)
            );
        }
    }

    #[test]
    fn different_posts_differ() {
        let pairs = [
            (
                "https://x.com/user/status/1844012345678901234",
                "https://x.com/user/status/1844012345678909999",
            ),
            (
                "https://bsky.app/profile/a.bsky.social/post/3l5abcxyz",
                "https://bsky.app/profile/b.bsky.social/post/3l5abcxyz",
            ),
            (
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
                "https://www.youtube.com/watch?v=oHg5SJYRHA0",
            ),
            (
                "https://cdn.discordapp.com/attachments/1/2/cat.png",
                "https://cdn.discordapp.com/attachments/1/3/cat.png",
            ),
            (
                "https://example.com/article?id=5",
                "https://example.com/article?id=6",
            ),
            ("https://example.com/a.png", "https://other.com/a.png"),
            // Short parameters only count as junk on the sites that use them that way.
            (
                "https://forum.example.com/viewtopic.php?t=123",
                "https://forum.example.com/viewtopic.php?t=456",
            ),
            (
                "https://example.com/search?s=cats",
                "https://example.com/search?s=dogs",
            ),
            (
                "https://example.com/img?width=100&name=a",
                "https://example.com/img?width=200&name=b",
            ),
            // Pages that aren't one post mustn't share a key.
            (
                "https://www.tumblr.com/tagged/cats",
                "https://www.tumblr.com/tagged/dogs",
            ),
            (
                "https://www.tumblr.com/search/cats",
                "https://www.tumblr.com/tagged/cats",
            ),
            (
                "https://64.media.tumblr.com/abc/s640x960/one.jpg",
                "https://64.media.tumblr.com/def/s640x960/two.jpg",
            ),
            (
                "https://clips.twitch.tv/embed?clip=OneClip",
                "https://clips.twitch.tv/embed?clip=OtherClip",
            ),
            ("https://imgur.com/t/cats", "https://imgur.com/t/dogs"),
            ("https://imgur.com/user/alice", "https://imgur.com/user/bob"),
            (
                "https://imgur.com/r/aww/AbCdEf1",
                "https://imgur.com/r/aww/XyZ9876",
            ),
            (
                "https://streamable.com/e/abc123",
                "https://streamable.com/e/xyz789",
            ),
            (
                "https://streamable.com/o/abc123",
                "https://streamable.com/o/xyz789",
            ),
            (
                "https://open.spotify.com/embed/track/4uLU6hMCjMI75M1A2tKUQC",
                "https://open.spotify.com/embed/track/7ouMYWpwJ422jRcDASZB7P",
            ),
            (
                "https://open.spotify.com/user/alice/playlist/37i9dQZF1DXcBWIGoYBM5M",
                "https://open.spotify.com/user/alice/playlist/37i9dQZF1DX0XUsuxWHRQd",
            ),
        ];
        for (a, b) in pairs {
            assert!(!same(a, b), "{a} and {b} should differ");
        }
    }

    #[test]
    fn ignored_links() {
        assert_eq!(link_key("https://klipy.com/gifs/dance-party-1"), None);
        assert_eq!(link_key("https://tenor.com/view/cat-12345"), None);
        assert_eq!(link_key("https://discord.com/channels/1/2/3"), None);
        assert_eq!(link_key("mailto:someone@example.com"), None);
    }

    #[test]
    fn picture_sites() {
        assert!(has_pictures("https://fxtwitter.com/a/status/1"));
        assert!(has_pictures("https://www.reddit.com/r/a/comments/b"));
        assert!(!has_pictures("https://youtu.be/abc"));
        assert!(!has_pictures("https://example.com/article"));
    }

    #[test]
    fn short_links() {
        assert!(needs_resolving("https://vm.tiktok.com/ZMabc123/"));
        assert!(needs_resolving("https://www.reddit.com/r/memes/s/AbC123"));
        assert!(needs_resolving("https://pin.it/abc"));
        assert!(!needs_resolving("https://x.com/u/status/1"));
    }
}
