//! Turning ElevenLabs' audio into what a Discord voice message needs: an OGG Opus file,
//! its length, and a waveform (the bars Discord draws). ffmpeg does the decoding.

use std::time::Duration;

use anyhow::{Context as _, bail};
use tokio::process::Command;

/// How long ffmpeg may take for one clip.
const TIMEOUT: Duration = Duration::from_secs(60);
/// The sample rate the waveform is measured at. Plenty for drawing bars.
const WAVE_RATE: u32 = 8000;
/// Discord draws at most 256 bars, about one per tenth of a second.
const MAX_BARS: usize = 256;
const BARS_PER_SECOND: f64 = 10.0;

/// A clip ready to send.
pub struct Clip {
    pub ogg: Vec<u8>,
    pub seconds: f64,
    /// One byte per bar, 0 to 255.
    pub waveform: Vec<u8>,
}

/// Re-encodes `audio` (any format ffmpeg reads) as mono OGG Opus and measures it.
pub async fn prepare(audio: &[u8]) -> anyhow::Result<Clip> {
    // A real file, like `util::frames`: some formats can't be read from a pipe.
    let dir = tempfile::tempdir().context("creating a temporary folder")?;
    let input = dir.path().join("input");
    let output = dir.path().join("voice.ogg");
    tokio::fs::write(&input, audio).await?;

    ffmpeg(&[
        "-i",
        path(&input)?,
        "-ac",
        "1",
        "-ar",
        "48000",
        "-c:a",
        "libopus",
        "-b:a",
        "64k",
        "-f",
        "ogg",
        path(&output)?,
    ])
    .await?;
    let ogg = tokio::fs::read(&output).await?;

    // The same audio as raw 16-bit samples, for the length and the bars.
    let rate = WAVE_RATE.to_string();
    let raw = ffmpeg(&[
        "-i",
        path(&input)?,
        "-ac",
        "1",
        "-ar",
        &rate,
        "-f",
        "s16le",
        "-",
    ])
    .await?;
    let samples: Vec<i16> = raw
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    if samples.is_empty() {
        bail!("the audio was empty");
    }
    let seconds = samples.len() as f64 / f64::from(WAVE_RATE);
    Ok(Clip {
        ogg,
        seconds,
        waveform: waveform(&samples, seconds),
    })
}

fn path(path: &std::path::Path) -> anyhow::Result<&str> {
    path.to_str().context("temporary path isn't UTF-8")
}

/// Runs ffmpeg and returns what it wrote to stdout.
async fn ffmpeg(args: &[&str]) -> anyhow::Result<Vec<u8>> {
    let mut command = Command::new("ffmpeg");
    command
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
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
    Ok(result.stdout)
}

/// The bars Discord draws: the loudest sample in each slice of the clip, scaled so the
/// loudest slice is 255.
fn waveform(samples: &[i16], seconds: f64) -> Vec<u8> {
    let bars = ((seconds * BARS_PER_SECOND).ceil() as usize).clamp(1, MAX_BARS);
    let size = samples.len().div_ceil(bars).max(1);
    let peaks: Vec<u32> = samples
        .chunks(size)
        .map(|slice| {
            slice
                .iter()
                .map(|s| s.unsigned_abs() as u32)
                .max()
                .unwrap_or(0)
        })
        .collect();
    let loudest = peaks.iter().copied().max().unwrap_or(0).max(1);
    peaks
        .into_iter()
        .map(|peak| (peak * 255 / loudest) as u8)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_bars() {
        // One second: ten bars, the loudest at 255.
        let mut samples = vec![0i16; 8000];
        samples[100] = 1000;
        samples[7999] = -2000;
        let bars = waveform(&samples, 1.0);
        assert_eq!(bars.len(), 10);
        assert_eq!(bars[0], 127);
        assert_eq!(bars[9], 255);
        assert_eq!(bars[5], 0);
        // A long clip still has at most 256 bars, and silence doesn't divide by zero.
        assert_eq!(waveform(&vec![0; 8000 * 60], 60.0).len(), 256);
    }

    #[tokio::test]
    async fn prepares_a_clip() {
        // Two seconds of a tone, as WAV from ffmpeg itself.
        let wav = ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-f",
            "wav",
            "-",
        ])
        .await
        .unwrap();
        let clip = prepare(&wav).await.unwrap();
        assert!((clip.seconds - 2.0).abs() < 0.05, "{}", clip.seconds);
        assert_eq!(clip.waveform.len(), 20);
        assert_eq!(&clip.ogg[..4], b"OggS");
    }
}
