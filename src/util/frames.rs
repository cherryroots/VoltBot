//! Turning GIFs and videos into a few grid images a model can look at.
//!
//! Models read still images, not video. Like voltgpt, the bot takes about three frames per
//! second and tiles them into grids. ffmpeg does all of it in one run (`fps` picks the frames,
//! `scale` + `pad` fit each one into a square cell, `tile` lays them out), so there's no image
//! code here. `ffmpeg` and `ffprobe` must be installed on the server.

use std::time::Duration;

use anyhow::{Context as _, bail};
use tokio::process::Command;

use super::media::MediaKind;

/// Frames per second to take from short clips. Long ones get fewer, spread over the clip.
const FPS: f64 = 3.0;
/// How long ffmpeg may take for one file.
const TIMEOUT: Duration = Duration::from_secs(60);
/// Used when ffprobe can't tell how long the file is.
const UNKNOWN_DURATION_SECS: f64 = 10.0;

/// How the frames of one file are laid out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    /// Width and height of one frame in the grid, in pixels.
    pub cell: u32,
    /// The most columns (and rows) in one grid.
    pub max_side: u32,
    /// The most grids for one file.
    pub max_grids: u32,
}

impl Layout {
    /// Videos: up to 10 grids of 9×9 small frames (voltgpt's setting). GIFs: grids of 3×3
    /// larger frames, since they're usually short and the detail matters more.
    pub fn for_kind(kind: MediaKind) -> Layout {
        match kind {
            MediaKind::Video => Layout {
                cell: 100,
                max_side: 9,
                max_grids: 10,
            },
            _ => Layout {
                cell: 400,
                max_side: 3,
                max_grids: 10,
            },
        }
    }

    fn max_frames(&self) -> u32 {
        self.max_side * self.max_side * self.max_grids
    }
}

/// Picks the frame rate and the grid shape (columns, rows) for a clip of `duration` seconds.
/// Short clips get a grid just big enough for their frames, instead of a mostly empty 9×9.
fn plan(duration: f64, layout: Layout) -> (f64, u32, u32) {
    let duration = if duration > 0.0 {
        duration
    } else {
        UNKNOWN_DURATION_SECS
    };
    let fps = FPS.min(f64::from(layout.max_frames()) / duration);
    let frames = ((duration * fps).ceil() as u32).clamp(1, layout.max_frames());
    let per_grid = frames.min(layout.max_side * layout.max_side);
    let columns = (f64::from(per_grid).sqrt().ceil() as u32).max(1);
    let rows = per_grid.div_ceil(columns);
    (fps, columns, rows)
}

/// Returns the frame grids of a GIF or video as PNG files.
pub async fn frame_grids(data: &[u8], layout: Layout) -> anyhow::Result<Vec<Vec<u8>>> {
    // ffmpeg needs a real file: MP4s can't always be read from a pipe. The folder and
    // everything in it is deleted when `dir` is dropped.
    let dir = tempfile::tempdir().context("creating a temporary folder")?;
    let input = dir.path().join("input");
    tokio::fs::write(&input, data).await?;

    let duration = probe_duration(&input).await.unwrap_or(0.0);
    let (fps, columns, rows) = plan(duration, layout);
    let cell = layout.cell;
    let filter = format!(
        "fps={fps},\
         scale={cell}:{cell}:force_original_aspect_ratio=decrease,\
         pad={cell}:{cell}:(ow-iw)/2:(oh-ih)/2:color=white,\
         tile={columns}x{rows}:color=white"
    );
    let output = dir.path().join("grid_%03d.png");
    let mut command = Command::new("ffmpeg");
    command
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(&input)
        .args(["-vf", &filter, "-frames:v", &layout.max_grids.to_string()])
        .arg(&output)
        .kill_on_drop(true);
    let result = tokio::time::timeout(TIMEOUT, command.output())
        .await
        .context("ffmpeg took too long")?
        .context("running ffmpeg (is it installed?)")?;
    if !result.status.success() {
        bail!(
            "ffmpeg failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        );
    }

    // ffmpeg numbers the grids grid_001.png, grid_002.png, ...
    let mut grids = Vec::new();
    for n in 1..=layout.max_grids {
        let path = dir.path().join(format!("grid_{n:03}.png"));
        match tokio::fs::read(&path).await {
            Ok(png) => grids.push(png),
            Err(_) => break,
        }
    }
    if grids.is_empty() {
        bail!("ffmpeg found no frames");
    }
    Ok(grids)
}

/// The length of a GIF or video in seconds, from ffprobe.
async fn probe_duration(path: &std::path::Path) -> anyhow::Result<f64> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration"])
        .args(["-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(path)
        .kill_on_drop(true)
        .output()
        .await?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim()
        .parse()
        .with_context(|| format!("ffprobe printed {text:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIDEO: Layout = Layout {
        cell: 100,
        max_side: 9,
        max_grids: 10,
    };

    #[test]
    fn plans_grids() {
        // 2 s at 3 fps = 6 frames: a 3×2 grid.
        assert_eq!(plan(2.0, VIDEO), (3.0, 3, 2));
        // 0.1 s: still one frame.
        assert_eq!(plan(0.1, VIDEO), (3.0, 1, 1));
        // An hour: 810 frames spread over it, in full 9×9 grids.
        let (fps, columns, rows) = plan(3600.0, VIDEO);
        assert!((fps - 0.225).abs() < 1e-9);
        assert_eq!((columns, rows), (9, 9));
        // Unknown length: treated as 10 s.
        assert_eq!(plan(0.0, VIDEO), (3.0, 6, 5));
    }

    /// Makes a short test clip with ffmpeg's built-in test pattern.
    async fn test_clip(seconds: u32, extension: &str) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("clip.{extension}"));
        let status = Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=10",
            ])
            .args(["-t", &seconds.to_string()])
            .arg(&path)
            .status()
            .await
            .expect("ffmpeg must be installed to run this test");
        assert!(status.success());
        tokio::fs::read(&path).await.unwrap()
    }

    /// Width and height of a PNG, read from its header.
    fn png_size(png: &[u8]) -> (u32, u32) {
        let number = |at: usize| u32::from_be_bytes(png[at..at + 4].try_into().unwrap());
        (number(16), number(20))
    }

    #[tokio::test]
    async fn video_becomes_one_grid() {
        let clip = test_clip(2, "mp4").await;
        let grids = frame_grids(&clip, VIDEO).await.unwrap();
        assert_eq!(grids.len(), 1);
        assert_eq!(png_size(&grids[0]), (300, 200));
    }

    #[tokio::test]
    async fn long_gif_becomes_several_grids() {
        // 4 s at 3 fps = 12 frames: one full 3×3 grid and one with the last 3 frames.
        let clip = test_clip(4, "gif").await;
        let grids = frame_grids(&clip, Layout::for_kind(MediaKind::Gif))
            .await
            .unwrap();
        assert_eq!(grids.len(), 2);
        assert_eq!(png_size(&grids[0]), (1200, 1200));
    }

    #[tokio::test]
    async fn garbage_is_an_error() {
        assert!(frame_grids(b"not a video", VIDEO).await.is_err());
    }
}
