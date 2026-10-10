//! What a Claude response costs, from its token counts, at Anthropic's list prices.
//!
//! Prices: <https://platform.claude.com/docs/en/about-claude/pricing>. Writing to the
//! prompt cache costs 1.25 times the input price (the 5-minute cache the bot uses).
//! Code execution is left out: it's free while web search is in the request, which it
//! always is here.

use super::collect::Usage;

/// USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Prices {
    input: f64,
    cache_read: f64,
    output: f64,
}

/// Each web search costs this, on top of the tokens its results add.
const WEB_SEARCH: f64 = 10.0 / 1000.0;

/// A model the table doesn't know is priced like Opus 5.5, so the total is still close.
fn prices(model: &str) -> Prices {
    let (input, cache_read, output) = match model {
        m if m.starts_with("claude-opus-5-5") => (4.0, 0.20, 20.0),
        m if m.starts_with("claude-opus-5") => (5.0, 0.50, 25.0),
        m if m.starts_with("claude-sonnet-5-5") => (2.0, 0.20, 10.0),
        m if m.starts_with("claude-fable-5-1") => (10.0, 0.25, 50.0),
        _ => (4.0, 0.20, 20.0),
    };
    Prices {
        input,
        cache_read,
        output,
    }
}

/// What one response cost, in USD.
pub fn cost(usage: &Usage, model: &str) -> f64 {
    let p = prices(model);
    let per_token = |count: u64, price: f64| count as f64 * price / 1_000_000.0;
    per_token(usage.input, p.input)
        + per_token(usage.cache_write, p.input * 1.25)
        + per_token(usage.cache_read, p.cache_read)
        + per_token(usage.output, p.output)
        + usage.web_searches as f64 * WEB_SEARCH
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_a_response() {
        let usage = Usage {
            input: 1_000,
            cache_write: 2_000,
            cache_read: 100_000,
            output: 1_500,
            web_searches: 2,
        };
        // 0.004 + 0.01 + 0.02 + 0.03 + 0.02
        let usd = cost(&usage, "claude-opus-5-5");
        assert!((usd - 0.084).abs() < 1e-9, "{usd}");
        assert!(cost(&usage, "claude-sonnet-5-5") < usd);
        assert_eq!(cost(&usage, "claude-new-model"), usd);
        assert_eq!(cost(&Usage::default(), "claude-opus-5-5"), 0.0);
    }
}
