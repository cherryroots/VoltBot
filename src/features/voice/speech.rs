//! ElevenLabs text to speech: one request turns her script into audio.
//! <https://elevenlabs.io/docs/api-reference/text-to-speech/convert>

use anyhow::bail;
use serde_json::json;

use super::Settings;
use crate::util::shorten;

const API: &str = "https://api.elevenlabs.io/v1/text-to-speech";
/// Opus at 48 kHz, the closest to what Discord plays. `audio.rs` turns it into OGG.
const FORMAT: &str = "opus_48000_128";

/// Speaks `text` (audio tags included) in the voice from `settings`. Returns the audio.
pub async fn speak(
    web: &reqwest::Client,
    key: &str,
    settings: &Settings,
    text: &str,
) -> anyhow::Result<Vec<u8>> {
    let mut body = json!({
        "text": text,
        "model_id": settings.model,
        "voice_settings": {
            "stability": settings.stability,
            "similarity_boost": settings.similarity,
        },
    });
    if let Some(language) = &settings.language {
        body["language_code"] = language.clone().into();
    }
    let response = web
        .post(format!("{API}/{}", settings.voice_id))
        .query(&[("output_format", FORMAT)])
        .header("xi-api-key", key)
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        bail!(
            "ElevenLabs answered {status}: {}",
            shorten(text.trim(), 300)
        );
    }
    Ok(response.bytes().await?.to_vec())
}
