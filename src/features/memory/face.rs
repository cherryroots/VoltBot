//! Vivy's face: one picture per mood, like `happy.png` and `sleepy.png`, in the folder
//! `faces_dir` under `[features.memory]`. The daily reflection picks one with the `face:`
//! line of her mood file, and the bot makes it her avatar in that server. The control
//! panel shows the face of her newest mood in the "Vivy's mind" box.
//!
//! Without `faces_dir` (or with an empty folder) nothing here happens, and the reflection
//! isn't asked for a face.

use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use base64::Engine as _;
use base64::prelude::BASE64_STANDARD;
use chrono::Utc;
use image::imageops::FilterType;
use serde::Deserialize;
use serenity::all::GuildId;
use tracing::{info, warn};

use super::{reflect, store};
use crate::core::{BotCtx, Result};

/// Avatars are scaled down to this size, which is plenty for Discord.
const AVATAR_SIZE: u32 = 512;
/// The shortest time between two avatar changes in one server, so a lively conversation
/// can't run into Discord's limits. A face skipped for this waits for the next mood change.
const MIN_GAP_SECS: i64 = 10 * 60;

/// The face on the control panel, in pixels (drawn at half this size, for sharpness).
const PANEL_SIZE: u32 = 128;

/// `[features.memory]` settings for her faces.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Settings {
    /// The folder with one PNG per mood.
    faces_dir: Option<String>,
}

/// The faces folder, if one is set.
fn dir(ctx: &BotCtx) -> Option<PathBuf> {
    let settings: Settings = ctx.config.feature("memory").ok()?;
    settings.faces_dir.map(PathBuf::from)
}

/// The faces she can pick from: the names of the PNGs in `dir`, sorted.
pub fn names_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let png = path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("png"));
            png.then(|| path.file_stem()?.to_str().map(str::to_lowercase))
                .flatten()
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The faces she can pick from, or none without a faces folder.
pub fn names(ctx: &BotCtx) -> Vec<String> {
    dir(ctx).map(|dir| names_in(&dir)).unwrap_or_default()
}

/// The line added to the reflection's instructions when there are faces.
pub fn instruction(names: &[String]) -> Option<String> {
    (!names.is_empty()).then(|| {
        format!(
            "Add a fifth line to mood.md, `face:` and the one word from this list that best fits your mood: {}. It becomes your profile picture in this server.",
            names.join(", ")
        )
    })
}

/// The `face:` line of her mood file, if it names one of `names`.
pub fn parse(mood: &str, names: &[String]) -> Option<String> {
    let face = reflect::mood_line(mood, "face")?.to_lowercase();
    let face = face.trim_end_matches('.').trim();
    names.iter().find(|name| *name == face).cloned()
}

/// Reads a face and scales it down to at most `size` pixels, as a PNG.
fn load(path: &Path, size: u32) -> anyhow::Result<Vec<u8>> {
    let picture = image::open(path).with_context(|| format!("reading {}", path.display()))?;
    let picture = if picture.width() > size || picture.height() > size {
        picture.resize(size, size, FilterType::Lanczos3)
    } else {
        picture
    };
    let mut png = Vec::new();
    picture.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)?;
    Ok(png)
}

/// A short fingerprint of a picture, so a face is sent again only when it changed.
fn fingerprint(png: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    png.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Makes the face in her mood file for `scope` her avatar in that server. Does nothing
/// for DMs, without faces, or when that server already has this picture.
pub async fn update(ctx: &BotCtx, scope: &str, mood: &str) {
    let Some(guild) = scope
        .strip_prefix("server:")
        .and_then(|id| id.parse::<u64>().ok())
        .map(GuildId::new)
    else {
        return;
    };
    let Some(dir) = dir(ctx) else {
        return;
    };
    let Some(face) = parse(mood, &names_in(&dir)) else {
        return;
    };
    if let Err(err) = set_avatar(ctx, guild, &dir, &face).await {
        warn!(%guild, face, "changing her face: {err:#}");
    }
}

async fn set_avatar(ctx: &BotCtx, guild: GuildId, dir: &Path, face: &str) -> Result<()> {
    let path = dir.join(format!("{face}.png"));
    let png = tokio::task::spawn_blocking(move || load(&path, AVATAR_SIZE)).await??;
    let print = fingerprint(&png);
    let id = guild.get();
    let current = ctx.db.call(move |conn| Ok(store::face(conn, id)?)).await?;
    if let Some((current, at)) = current {
        if current == print {
            return Ok(());
        }
        if Utc::now().timestamp() - at < MIN_GAP_SECS {
            info!(%guild, face, "face changed less than 10 minutes ago, keeping it for now");
            return Ok(());
        }
    }

    let avatar = format!("data:image/png;base64,{}", BASE64_STANDARD.encode(&png));
    let mut body = serde_json::Map::new();
    body.insert("avatar".into(), avatar.into());
    ctx.http.edit_member_me(guild, &body, None).await?;
    info!(%guild, face, "changed her face");

    let (name, now) = (face.to_string(), Utc::now().timestamp());
    ctx.db
        .call(move |conn| Ok(store::set_face(conn, id, &name, &print, now)?))
        .await?;
    Ok(())
}

/// At start: puts each server's face back in step with its mood file, for a face picked
/// before the pictures were there, or a picture that was replaced. Servers that already
/// have it are skipped, so this costs nothing most of the time.
pub async fn sync_all(ctx: &BotCtx) {
    if names(ctx).is_empty() {
        return;
    }
    let moods = ctx
        .db
        .call(|conn| Ok(store::every_file(conn, reflect::MOOD_FILE)?))
        .await;
    let moods = match moods {
        Ok(moods) => moods,
        Err(err) => {
            warn!("reading her moods: {err:#}");
            return;
        }
    };
    for (scope, mood) in moods {
        let allowed = scope
            .strip_prefix("server:")
            .and_then(|id| id.parse().ok())
            .is_some_and(|id| ctx.gate("memory").allows_guild(GuildId::new(id)));
        if allowed {
            update(ctx, &scope, &mood).await;
        }
    }
}

/// The face for the control panel, as a small PNG in a `data:` URL, from her newest mood.
pub async fn panel_picture(ctx: &BotCtx, mood: &str) -> Option<String> {
    let dir = dir(ctx)?;
    let face = parse(mood, &names_in(&dir))?;
    let path = dir.join(format!("{face}.png"));
    let png = tokio::task::spawn_blocking(move || load(&path, PANEL_SIZE))
        .await
        .ok()?
        .inspect_err(|err| warn!("reading her face: {err:#}"))
        .ok()?;
    Some(format!(
        "data:image/png;base64,{}",
        BASE64_STANDARD.encode(&png)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_the_pngs() {
        let dir = tempfile::tempdir().unwrap();
        for file in ["Happy.png", "sleepy.PNG", "notes.txt", "sad.jpg"] {
            std::fs::write(dir.path().join(file), b"").unwrap();
        }
        assert_eq!(names_in(dir.path()), ["happy", "sleepy"]);
        assert!(names_in(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn reads_the_face_line() {
        let names = vec!["happy".to_string(), "sleepy".to_string()];
        assert_eq!(
            parse("mood: tired\n- **Face**: Sleepy.", &names),
            Some("sleepy".into())
        );
        assert_eq!(parse("face: furious", &names), None);
        assert_eq!(parse("mood: fine", &names), None);
        assert!(instruction(&[]).is_none());
        assert!(instruction(&names).unwrap().contains("happy, sleepy"));
    }

    #[test]
    fn scales_faces_down() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("happy.png");
        image::RgbaImage::new(1024, 1024).save(&path).unwrap();
        let png = load(&path, AVATAR_SIZE).unwrap();
        let small = image::load_from_memory(&png).unwrap();
        assert_eq!((small.width(), small.height()), (512, 512));
        assert_eq!(fingerprint(&png), fingerprint(&png));
    }
}
