//! What a Claude response costs, from its token counts, at Anthropic's list prices.
//!
//! Prices: <https://platform.claude.com/docs/en/about-claude/pricing> (checked 2026-10-10).
//! Writing to the prompt cache costs 1.25 times the input price on every model (the
//! 5-minute cache the bot uses). Reading from it is usually a tenth of the input price, but
//! less on the newest models, so the table lists it. Every model since Claude 4.6 costs the
//! same per token over its whole context window, except Haiku 5.5, whose long prompts cost
//! more. Code execution is left out: it's free while web search is in the request, which it
//! always is here. Web fetch costs only its tokens.

use super::collect::Usage;

/// USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Prices {
    input: f64,
    cache_read: f64,
    output: f64,
}

const fn prices(input: f64, cache_read: f64, output: f64) -> Prices {
    Prices {
        input,
        cache_read,
        output,
    }
}

/// Model ID prefixes and their prices. The first match wins, so a longer ID comes before a
/// shorter one it starts with ("claude-opus-5-5" before "claude-opus-5").
const TABLE: &[(&str, Prices)] = &[
    ("claude-fable-5-1", prices(10.0, 0.25, 50.0)),
    ("claude-mythos-5-1", prices(10.0, 0.25, 50.0)),
    ("claude-fable-5", prices(10.0, 1.0, 50.0)),
    ("claude-mythos-5", prices(10.0, 1.0, 50.0)),
    ("claude-opus-5-5", prices(4.0, 0.20, 20.0)),
    ("claude-opus-5", prices(5.0, 0.50, 25.0)),
    ("claude-opus-4-8", prices(5.0, 0.50, 25.0)),
    ("claude-opus-4-7", prices(5.0, 0.50, 25.0)),
    ("claude-opus-4-6", prices(5.0, 0.50, 25.0)),
    ("claude-opus-4-5", prices(5.0, 0.50, 25.0)),
    ("claude-sonnet-5-5", prices(2.0, 0.10, 10.0)),
    ("claude-sonnet-5", prices(2.0, 0.20, 10.0)),
    ("claude-sonnet-4", prices(3.0, 0.30, 15.0)),
    ("claude-haiku-5-5", prices(0.10, 0.01, 0.50)),
    ("claude-haiku-4-5", prices(1.0, 0.10, 5.0)),
];

/// Haiku 5.5 when the prompt (all input, cached or not) is over [`HAIKU_LONG_PROMPT`].
const HAIKU_5_5_LONG: Prices = prices(0.50, 0.05, 2.50);
const HAIKU_LONG_PROMPT: u64 = 100_000;

/// A model the table doesn't know is priced like Opus 5.5 (the default model), so the
/// total is still close.
const UNKNOWN: Prices = prices(4.0, 0.20, 20.0);

/// Each web search costs this, on top of the tokens its results add.
const WEB_SEARCH: f64 = 10.0 / 1000.0;

fn prices_for(model: &str, usage: &Usage) -> Prices {
    let prompt = usage.input + usage.cache_read + usage.cache_write;
    if model.starts_with("claude-haiku-5-5") && prompt > HAIKU_LONG_PROMPT {
        return HAIKU_5_5_LONG;
    }
    TABLE
        .iter()
        .find(|(prefix, _)| model.starts_with(prefix))
        .map_or(UNKNOWN, |(_, prices)| *prices)
}

/// What one response cost, in USD.
pub fn cost(usage: &Usage, model: &str) -> f64 {
    let p = prices_for(model, usage);
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

    fn usage() -> Usage {
        Usage {
            input: 1_000,
            cache_write: 2_000,
            cache_read: 100_000,
            output: 1_500,
            web_searches: 2,
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn prices_a_response() {
        let usage = usage();
        // 0.004 + 0.01 + 0.02 + 0.03 + 0.02
        let usd = cost(&usage, "claude-opus-5-5");
        assert!(close(usd, 0.084), "{usd}");
        // 0.002 + 0.005 + 0.01 + 0.015 + 0.02
        assert!(close(cost(&usage, "claude-sonnet-5-5"), 0.052));
        // Opus 5 isn't priced like Opus 5.5, though its ID starts the same.
        // 0.005 + 0.0125 + 0.05 + 0.0375 + 0.02
        assert!(close(cost(&usage, "claude-opus-5"), 0.125));
        // 0.01 + 0.025 + 0.025 + 0.075 + 0.02
        assert!(close(cost(&usage, "claude-fable-5-1"), 0.155));
        assert_eq!(cost(&usage, "claude-new-model"), usd);
        assert_eq!(cost(&Usage::default(), "claude-opus-5-5"), 0.0);
    }

    #[test]
    fn long_haiku_prompts_cost_more() {
        let short = Usage {
            input: 50_000,
            ..Usage::default()
        };
        assert!(close(cost(&short, "claude-haiku-5-5"), 0.005));
        // Cache reads count toward the prompt's length.
        let long = Usage {
            input: 50_000,
            cache_read: 60_000,
            ..Usage::default()
        };
        // 0.025 + 0.003
        assert!(close(cost(&long, "claude-haiku-5-5"), 0.028));
    }
}
