//! The text in a message that isn't in its content: text file attachments and embeds.
//! Chat adds both to what the model reads, in the same tagged format voltgpt used.

use serenity::all::Message;
use tracing::warn;

use super::media::download;

/// Text attachments bigger than this are skipped.
const MAX_TEXT_BYTES: usize = 200 * 1024;

/// The text attachments (`.txt`, `.md`, code files...) of a message, downloaded and tagged.
/// Empty when there are none. Files that fail to download are skipped with a warning.
pub async fn attachment_text(client: &reqwest::Client, msg: &Message) -> String {
    let mut blocks = Vec::new();
    let text_files = msg.attachments.iter().filter(|a| {
        a.content_type
            .as_deref()
            .is_some_and(|t| t.starts_with("text/"))
    });
    for attachment in text_files {
        match download(client, &attachment.url, MAX_TEXT_BYTES).await {
            Ok(data) => blocks.push(format!(
                "<attachment>\n<name>{}</name>\n<text>\n{}\n</text>\n</attachment>",
                attachment.filename,
                String::from_utf8_lossy(&data)
            )),
            Err(err) => warn!("couldn't download {}: {err:#}", attachment.filename),
        }
    }
    if blocks.is_empty() {
        return String::new();
    }
    format!("<attachments>\n{}\n</attachments>\n", blocks.join("\n"))
}

/// The titles, descriptions and fields of a message's embeds, tagged. Empty when there are
/// none.
pub fn embed_text(msg: &Message) -> String {
    let mut blocks = Vec::new();
    for embed in &msg.embeds {
        let mut lines = Vec::new();
        if let Some(title) = &embed.title {
            lines.push(format!("<title>{title}</title>"));
        }
        if let Some(description) = &embed.description {
            lines.push(format!("<description>{description}</description>"));
        }
        for field in &embed.fields {
            lines.push(format!(
                "<field>\n<name>{}</name>\n<value>{}</value>\n</field>",
                field.name, field.value
            ));
        }
        // Image-only embeds have no text; the image itself goes through `media`.
        if !lines.is_empty() {
            blocks.push(format!("<embed>\n{}\n</embed>", lines.join("\n")));
        }
    }
    if blocks.is_empty() {
        return String::new();
    }
    format!("<embeds>\n{}\n</embeds>\n", blocks.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn embeds_as_text() {
        let mut msg = Message::default();
        assert_eq!(embed_text(&msg), "");

        msg.embeds = vec![
            serde_json::from_value(json!({
                "title": "Weather",
                "description": "Sunny",
                "fields": [{"name": "High", "value": "21°C", "inline": true}],
            }))
            .unwrap(),
            serde_json::from_value(json!({"image": {"url": "https://x/a.png"}})).unwrap(),
        ];
        assert_eq!(
            embed_text(&msg),
            "<embeds>\n<embed>\n<title>Weather</title>\n<description>Sunny</description>\n\
             <field>\n<name>High</name>\n<value>21°C</value>\n</field>\n</embed>\n</embeds>\n"
        );
    }
}
