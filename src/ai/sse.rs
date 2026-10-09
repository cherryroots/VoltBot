//! Server-sent events: the streaming format OpenAI and Claude both use.
//!
//! The response body is text made of events separated by a blank line. Each event has
//! `field: value` lines; only `event` (the name) and `data` (the JSON) matter here:
//!
//! ```text
//! event: response.output_text.delta
//! data: {"type":"response.output_text.delta","delta":"Hel"}
//!
//! ```
//!
//! The network hands over the body in chunks that can end anywhere, even inside a
//! character, so [`SseParser`] keeps the unfinished end until the next chunk arrives.

#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Default)]
pub struct SseParser {
    buffer: Vec<u8>,
}

impl SseParser {
    /// Adds a chunk of the body and returns the events it completed.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some((end, separator)) = find_blank_line(&self.buffer) {
            let block: Vec<u8> = self.buffer.drain(..end + separator).collect();
            if let Some(event) = parse_block(&String::from_utf8_lossy(&block[..end])) {
                events.push(event);
            }
        }
        events
    }
}

/// Where the first blank line is, and how many bytes it takes ("\n\n" or "\r\n\r\n").
fn find_blank_line(buffer: &[u8]) -> Option<(usize, usize)> {
    for i in 0..buffer.len() {
        if buffer[i..].starts_with(b"\n\n") {
            return Some((i, 2));
        }
        if buffer[i..].starts_with(b"\r\n\r\n") {
            return Some((i, 4));
        }
    }
    None
}

fn parse_block(block: &str) -> Option<SseEvent> {
    let mut event = None;
    let mut data = Vec::new();
    for line in block.lines() {
        // Lines starting with ":" are comments (keep-alives).
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = Some(value.to_string()),
            "data" => data.push(value),
            _ => {}
        }
    }
    if data.is_empty() {
        return None;
    }
    Some(SseEvent {
        event,
        data: data.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_split_across_chunks() {
        let mut parser = SseParser::default();
        assert!(parser.push(b"event: a\ndata: {\"x\"").is_empty());
        let events = parser.push(b":1}\n\n: keep-alive\n\ndata: two\ndata: lines\r\n\r\n");
        assert_eq!(
            events,
            [
                SseEvent {
                    event: Some("a".into()),
                    data: "{\"x\":1}".into()
                },
                SseEvent {
                    event: None,
                    data: "two\nlines".into()
                },
            ]
        );
    }

    #[test]
    fn characters_split_across_chunks() {
        let mut parser = SseParser::default();
        let text = "data: 🦀\n\n".as_bytes();
        assert!(parser.push(&text[..8]).is_empty());
        assert_eq!(parser.push(&text[8..])[0].data, "🦀");
    }
}
