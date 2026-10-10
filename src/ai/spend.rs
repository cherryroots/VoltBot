//! What the AI costs this month, added up from the token counts each answer reports.
//!
//! The provider works out what a response cost (see `claude/price.rs`) and calls
//! [`Spend::add`]. Months are calendar months in UTC, kept in `ai_spend` (a core table).
//! At 80% and 100% of the monthly budget there's one warning each, which shows in the log
//! channel. These are list prices, so the numbers are an estimate of what Anthropic bills.

use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use tracing::warn;

use super::display_name;
use crate::core::db::Db;

/// Shares of the budget that get a warning, once a month each.
const WARN_AT: [i64; 2] = [80, 100];

#[derive(Clone)]
pub struct Spend {
    db: Db,
    /// USD per month; 0 means no warnings.
    budget: f64,
}

impl Spend {
    pub fn new(db: Db, budget: f64) -> Spend {
        Spend { db, budget }
    }

    pub fn budget(&self) -> f64 {
        self.budget
    }

    /// Adds `usd` to this month's total for `provider`, and warns when the total passes
    /// a share of the budget for the first time this month. Errors are only logged: a
    /// failed write shouldn't fail the answer.
    pub async fn add(&self, provider: &'static str, usd: f64) {
        if usd <= 0.0 {
            return;
        }
        let month = this_month();
        let budget = self.budget;
        let result = self
            .db
            .call(move |conn| Ok(add(conn, &month, provider, usd, budget)?))
            .await;
        match result {
            Ok((total, Some(percent))) => warn!(
                "{} has cost ${total:.2} this month, {percent}% of the ${budget:.2} budget",
                display_name(provider)
            ),
            Ok((_, None)) => {}
            Err(err) => warn!("couldn't save what {provider} cost: {err:#}"),
        }
    }

    /// This month's total for `provider`, in USD.
    pub async fn this_month(&self, provider: &'static str) -> anyhow::Result<f64> {
        let month = this_month();
        self.db
            .call(move |conn| {
                let usd = conn
                    .query_row(
                        "SELECT usd FROM ai_spend WHERE month = ?1 AND provider = ?2",
                        params![month, provider],
                        |row| row.get(0),
                    )
                    .optional()?;
                Ok(usd.unwrap_or(0.0))
            })
            .await
    }
}

/// "2026-10".
fn this_month() -> String {
    Utc::now().format("%Y-%m").to_string()
}

/// Adds to the month's total. Returns the new total, and the budget share to warn about
/// when it passed one it hadn't warned about yet.
fn add(
    conn: &rusqlite::Connection,
    month: &str,
    provider: &str,
    usd: f64,
    budget: f64,
) -> rusqlite::Result<(f64, Option<i64>)> {
    let (total, warned): (f64, i64) = conn.query_row(
        "INSERT INTO ai_spend (month, provider, usd) VALUES (?1, ?2, ?3)
         ON CONFLICT (month, provider) DO UPDATE SET usd = usd + excluded.usd
         RETURNING usd, warned",
        params![month, provider, usd],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if budget <= 0.0 {
        return Ok((total, None));
    }
    let percent = (total / budget * 100.0) as i64;
    let Some(&passed) = WARN_AT
        .iter()
        .rev()
        .find(|&&at| percent >= at && at > warned)
    else {
        return Ok((total, None));
    };
    conn.execute(
        "UPDATE ai_spend SET warned = ?3 WHERE month = ?1 AND provider = ?2",
        params![month, provider, passed],
    )?;
    Ok((total, Some(passed)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::test_connection;

    #[test]
    fn adds_up_and_warns_once_per_share() {
        let conn = test_connection("test", &[]);
        let add = |usd| add(&conn, "2026-10", "claude", usd, 100.0).unwrap();
        assert_eq!(add(50.0), (50.0, None));
        assert_eq!(add(31.0), (81.0, Some(80)));
        assert_eq!(add(1.0), (82.0, None));
        assert_eq!(add(20.0), (102.0, Some(100)));
        assert_eq!(add(1.0), (103.0, None));
        // A new month starts from zero, and one jump past both shares warns once.
        assert_eq!(
            super::add(&conn, "2026-11", "claude", 120.0, 100.0).unwrap(),
            (120.0, Some(100))
        );
        // No budget, no warnings.
        assert_eq!(
            super::add(&conn, "2026-11", "openai", 500.0, 0.0).unwrap(),
            (500.0, None)
        );
    }
}
