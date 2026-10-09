//! Helpers shared by features: splitting and sending long messages, finding and loading
//! media, and pulling text out of messages. Nothing here knows about a specific feature.

// The chat feature (stage 3) is the first user of these, so until then they look unused.
#[allow(dead_code)]
pub mod frames;
#[allow(dead_code)]
pub mod media;
#[allow(dead_code)]
pub mod reply;
#[allow(dead_code)]
pub mod split;
#[allow(dead_code)]
pub mod text;

/// Cuts `text` to at most `max` characters (not bytes), ending in "…" when cut.
pub fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut short: String = text.chars().take(max.saturating_sub(1)).collect();
    short.push('…');
    short
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorten_counts_characters() {
        assert_eq!(shorten("héllo", 5), "héllo");
        assert_eq!(shorten("héllo wörld", 5), "héll…");
    }
}
