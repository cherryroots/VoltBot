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
/// Banners are scaled down to fit this width (and height).
const BANNER_SIZE: u32 = 1500;
/// The shortest time between two changes of one picture in one server, so a lively conversation
/// can't run into Discord's limits. A face skipped for this waits for the next mood change.
const MIN_GAP_SECS: i64 = 10 * 60;

/// The face on the control panel, in pixels (drawn at half this size, for sharpness).
const PANEL_SIZE: u32 = 128;

/// `[features.memory]` settings for her faces.
#[derive(Debug, Deserialize)]
#[serde(default)]
struct Settings {
    /// The folder with one PNG per mood.
    faces_dir: Option<String>,
    /// How far to zoom in on a face: 1 keeps the whole square, 1.5 shows the middle two
    /// thirds.
    face_zoom: f32,
    /// Where the middle of the crop is, from the top (0) to the bottom (1) of the picture.
    face_center: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            faces_dir: None,
            face_zoom: 1.0,
            face_center: 0.5,
        }
    }
}

/// The part of a face picture that is used: a square, `zoom` times smaller than the
/// picture's short side, centered across and at `center` down.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Crop {
    zoom: f32,
    center: f32,
}

impl Crop {
    /// The square to cut from a `width` × `height` picture: left, top and side.
    fn square(self, width: u32, height: u32) -> (u32, u32, u32) {
        let short = width.min(height) as f32;
        let side = (short / self.zoom.max(1.0)).round().max(1.0) as u32;
        let left = (width - side) / 2;
        let top = (height as f32 * self.center.clamp(0.0, 1.0) - side as f32 / 2.0)
            .clamp(0.0, (height - side) as f32)
            .round() as u32;
        (left, top, side)
    }
}

/// The crop for faces from the settings, or none when nothing is cut.
fn crop(ctx: &BotCtx) -> Option<Crop> {
    let settings: Settings = ctx.config.feature("memory").ok()?;
    let crop = Crop {
        zoom: settings.face_zoom,
        center: settings.face_center,
    };
    (crop.zoom > 1.0).then_some(crop)
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
            "Add a fifth line to mood.md, `face:` and the one word from this list that best fits your mood or what you're doing right now: {}. It becomes your profile picture in this server.",
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

/// Reads a picture, cuts out `crop` if given, and scales it down to at most `size` pixels,
/// as a PNG.
fn load(path: &Path, size: u32, crop: Option<Crop>) -> anyhow::Result<Vec<u8>> {
    let mut picture = image::open(path).with_context(|| format!("reading {}", path.display()))?;
    if let Some(crop) = crop {
        let (left, top, side) = crop.square(picture.width(), picture.height());
        picture = picture.crop_imm(left, top, side, side);
    }
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
    let path = dir.join(format!("{face}.png"));
    if let Err(err) = set_picture(ctx, guild, Slot::Avatar, path, &face).await {
        warn!(%guild, face, "changing her face: {err:#}");
    }
}

/// A picture on her profile in a server.
#[derive(Debug, Clone, Copy)]
pub enum Slot {
    Avatar,
    Banner,
}

impl Slot {
    /// Its field in Discord's API, and its name in `memory_pictures`.
    fn field(self) -> &'static str {
        match self {
            Slot::Avatar => "avatar",
            Slot::Banner => "banner",
        }
    }

    /// The largest size it's sent at.
    fn size(self) -> u32 {
        match self {
            Slot::Avatar => AVATAR_SIZE,
            Slot::Banner => BANNER_SIZE,
        }
    }
}

/// Sets the picture at `path` (named `name`, like "happy") in `slot` of her profile in
/// `guild`, unless it's already there or that slot changed in the last 10 minutes.
pub async fn set_picture(
    ctx: &BotCtx,
    guild: GuildId,
    slot: Slot,
    path: PathBuf,
    name: &str,
) -> Result<()> {
    let crop = match slot {
        Slot::Avatar => crop(ctx),
        Slot::Banner => None,
    };
    let png = tokio::task::spawn_blocking(move || load(&path, slot.size(), crop)).await??;
    let print = fingerprint(&png);
    let id = guild.get();
    let current = ctx
        .db
        .call(move |conn| Ok(store::picture(conn, id, slot.field())?))
        .await?;
    if let Some((current, at)) = current {
        if current == print {
            return Ok(());
        }
        if Utc::now().timestamp() - at < MIN_GAP_SECS {
            info!(%guild, name, "{} changed less than 10 minutes ago, keeping it for now", slot.field());
            return Ok(());
        }
    }

    let data = format!("data:image/png;base64,{}", BASE64_STANDARD.encode(&png));
    let mut body = serde_json::Map::new();
    body.insert(slot.field().into(), data.into());
    ctx.http.edit_member_me(guild, &body, None).await?;
    info!(%guild, name, "changed her {}", slot.field());

    let (name, now) = (name.to_string(), Utc::now().timestamp());
    ctx.db
        .call(move |conn| {
            Ok(store::set_picture(
                conn,
                id,
                slot.field(),
                &name,
                &print,
                now,
            )?)
        })
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
    let crop = crop(ctx);
    let png = tokio::task::spawn_blocking(move || load(&path, PANEL_SIZE, crop))
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
    fn crops_closer() {
        let crop = Crop {
            zoom: 2.0,
            center: 0.4,
        };
        // A 1000 px square: a 500 px square centered across, its middle at 400 px down.
        assert_eq!(crop.square(1000, 1000), (250, 150, 500));
        // Near the top it stops at the edge.
        let top = Crop {
            center: 0.0,
            ..crop
        };
        assert_eq!(top.square(1000, 1000), (250, 0, 500));
        // A tall picture uses its width.
        assert_eq!(crop.square(800, 1200), (200, 280, 400));
    }

    #[test]
    fn scales_faces_down() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("happy.png");
        image::RgbaImage::new(1024, 1024).save(&path).unwrap();
        let png = load(&path, AVATAR_SIZE, None).unwrap();
        let small = image::load_from_memory(&png).unwrap();
        assert_eq!((small.width(), small.height()), (512, 512));
        assert_eq!(fingerprint(&png), fingerprint(&png));
    }
}
