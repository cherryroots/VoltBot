//! What Anthropic actually billed this month, read with an Admin API key.
//!
//! The Cost API reports the whole organization's spend (tokens, web search, code
//! execution), in daily buckets, a few minutes behind. It needs an Admin API key
//! (`ANTHROPIC_ADMIN_KEY` in `.env`), which only organizations have. [`crate::ai::Spend`]
//! reads it once an hour and puts it in place of its own estimate.
//!
//! API reference: <https://platform.claude.com/docs/en/build-with-claude/usage-cost-api>

use anyhow::Context as _;
use chrono::{DateTime, Datelike, TimeZone, Utc};
use serde_json::Value;

use super::{API, VERSION};

#[derive(Clone)]
pub struct Billing {
    http: reqwest::Client,
    key: String,
}

impl Billing {
    pub fn new(http: reqwest::Client, key: String) -> Billing {
        Billing { http, key }
    }

    /// USD billed from the start of `now`'s month (UTC) until now.
    pub async fn this_month(&self, now: DateTime<Utc>) -> anyhow::Result<f64> {
        let start = Utc
            .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let mut cents = 0.0;
        let mut page: Option<String> = None;
        // One page holds up to 31 daily buckets, so this is usually one request.
        loop {
            let mut query = vec![
                ("starting_at", start.clone()),
                ("bucket_width", "1d".to_string()),
                ("limit", "31".to_string()),
            ];
            if let Some(page) = page.take() {
                query.push(("page", page));
            }
            let response = self
                .http
                .get(format!("{API}/organizations/cost_report"))
                .header("x-api-key", &self.key)
                .header("anthropic-version", VERSION)
                .query(&query)
                .send()
                .await
                .context("couldn't reach Anthropic")?;
            let status = response.status();
            let body: Value = response.json().await.unwrap_or_default();
            if !status.is_success() {
                let message = body["error"]["message"]
                    .as_str()
                    .unwrap_or("no reason given");
                anyhow::bail!("Anthropic's cost report answered {status}: {message}");
            }
            cents += total_cents(&body);
            match body["next_page"].as_str() {
                Some(next) if body["has_more"] == true => page = Some(next.to_string()),
                _ => return Ok(cents / 100.0),
            }
        }
    }
}

/// Adds up every amount in one page of the cost report. Amounts are decimal strings, in
/// cents.
fn total_cents(page: &Value) -> f64 {
    let buckets = page["data"].as_array().into_iter().flatten();
    buckets
        .flat_map(|bucket| bucket["results"].as_array().into_iter().flatten())
        .filter_map(|result| result["amount"].as_str()?.parse::<f64>().ok())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn adds_up_a_page() {
        let page = json!({
            "data": [
                {"results": [{"amount": "1234.5", "currency": "USD"}, {"amount": "65.5"}]},
                {"results": [{"amount": "100"}]},
                {"results": []},
            ],
            "has_more": false,
        });
        assert_eq!(total_cents(&page), 1400.0);
        assert_eq!(total_cents(&json!({})), 0.0);
    }
}
