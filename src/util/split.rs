//! Splitting long text into Discord-sized messages.
//!
//! Discord allows 2000 characters per message. The `text-splitter` crate does the hard part:
//! it cuts at the biggest Markdown unit that fits (a section, then a paragraph, then a line,
//! a sentence, a word), and it counts characters, not bytes, so it never cuts an emoji in
//! half. What it can't know is that Discord renders each message on its own, so a code block
//! cut in two would lose its formatting. [`split_message`] closes the block at the end of one
//! part and opens it again, with the same language, at the start of the next.

use text_splitter::{ChunkConfig, MarkdownSplitter};

/// The most characters Discord allows in one message.
pub const DISCORD_LIMIT: usize = 2000;

/// Room kept free in each part for a closing fence and a reopened one ("```" + language).
const FENCE_ROOM: usize = 32;
/// Languages longer than this aren't carried over to the next part.
const MAX_LANGUAGE: usize = 20;

/// Splits `text` into parts of at most `max` characters, keeping code blocks intact.
/// Empty text gives no parts.
pub fn split_message(text: &str, max: usize) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    if text.chars().count() <= max {
        return vec![text.to_string()];
    }

    // Untrimmed chunks keep the indentation of code that starts a chunk.
    let config = ChunkConfig::new(max.saturating_sub(FENCE_ROOM).max(1)).with_trim(false);
    let splitter = MarkdownSplitter::new(config);

    let mut parts = Vec::new();
    // The language of a code block that is still open at the end of the previous chunk.
    let mut open: Option<String> = None;
    for chunk in splitter.chunks(text) {
        let mut chunk = chunk.trim_end();
        let mut part = String::new();
        if let Some(language) = &open {
            let rest = chunk.trim_start_matches('\n');
            if is_fence(first_line(rest)) {
                // The chunk starts by closing the block, which the previous part already did.
                chunk = rest.split_once('\n').map_or("", |(_, after)| after);
                open = None;
            } else {
                part.push_str(&format!("```{language}\n"));
                chunk = rest;
            }
        } else {
            chunk = chunk.trim_start();
        }
        open = fence_state_after(chunk, open);
        let mut close = open.is_some();
        // A chunk that ends right after an opening fence: leave the fence to the next part,
        // which opens the block anyway, instead of sending an empty block.
        let last_line = chunk.lines().last().unwrap_or("");
        if close && is_fence(last_line) {
            chunk = chunk[..chunk.len() - last_line.len()].trim_end();
            close = false;
        }
        part.push_str(chunk);
        if close {
            part.push_str("\n```");
        }
        if !part.trim().is_empty() {
            parts.push(part);
        }
    }
    parts
}

/// Walks the lines of `chunk` and returns which code block, if any, is open at its end.
fn fence_state_after(chunk: &str, mut open: Option<String>) -> Option<String> {
    for line in chunk.lines() {
        if is_fence(line) {
            open = match open {
                Some(_) => None,
                None => Some(
                    line.trim_start()
                        .trim_start_matches('`')
                        .trim()
                        .chars()
                        .take(MAX_LANGUAGE)
                        .collect(),
                ),
            };
        }
    }
    open
}

fn is_fence(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lengths_ok(parts: &[String], max: usize) {
        for part in parts {
            let len = part.chars().count();
            assert!(len <= max, "part has {len} characters, max {max}:\n{part}");
        }
    }

    #[test]
    fn short_text_is_one_part() {
        assert_eq!(split_message("  hello  ", 2000), ["hello"]);
        assert!(split_message(" \n ", 2000).is_empty());
    }

    #[test]
    fn prefers_paragraph_breaks() {
        let first = "a".repeat(60);
        let second = "b".repeat(60);
        let parts = split_message(&format!("{first}\n\n{second}"), 100);
        assert_eq!(parts, [first, second]);
    }

    #[test]
    fn never_cuts_inside_a_character() {
        // Every emoji is 4 bytes; byte-based cutting would panic or corrupt it.
        let text = "🦀".repeat(500);
        let parts = split_message(&text, 100);
        lengths_ok(&parts, 100);
        assert_eq!(parts.concat(), text);
    }

    #[test]
    fn reopens_cut_code_blocks() {
        let code: Vec<String> = (0..40).map(|n| format!("    let x{n} = {n};")).collect();
        let text = format!("Here:\n\n```rust\n{}\n```\n\nDone.", code.join("\n"));
        let parts = split_message(&text, 200);
        assert!(parts.len() > 2, "{parts:#?}");
        lengths_ok(&parts, 200);
        for part in &parts {
            // Every part renders on its own, so fences must pair up within it.
            assert_eq!(part.matches("```").count() % 2, 0, "unbalanced:\n{part}");
        }
        // Parts after the first code part reopen it with the language, and keep indentation.
        assert!(parts[1].starts_with("```rust\n    let"), "{}", parts[1]);
        assert!(parts.last().unwrap().ends_with("Done."));

        // Nothing is lost: removing the added fences gives back every line of code.
        let joined = parts.join("\n");
        for line in &code {
            assert!(joined.contains(line.as_str()), "missing {line}");
        }
    }

    #[test]
    fn no_empty_code_blocks() {
        let code: Vec<String> = (0..30).map(|n| format!("line {n}")).collect();
        let text = format!("Intro.\n```\n{}\n```", code.join("\n"));
        let parts = split_message(&text, 80);
        assert_eq!(parts[0], "Intro.", "{parts:#?}");
        for part in &parts[1..] {
            assert!(part.starts_with("```\nline"), "{part}");
            assert!(part.ends_with("\n```"), "{part}");
        }
    }

    #[test]
    fn fence_state() {
        assert_eq!(fence_state_after("```py\nx", None), Some("py".into()));
        assert_eq!(fence_state_after("x\n```", Some("py".into())), None);
        assert_eq!(
            fence_state_after("```\na\n```\n```js", None),
            Some("js".into())
        );
        assert_eq!(fence_state_after("no code", None), None);
    }
}
