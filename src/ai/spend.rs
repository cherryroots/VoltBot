//! What the AI costs this month, added up from the token counts each answer reports.
//!
//! The provider works out what a response cost (see `claude/price.rs`) and calls
//! [`Spend::add`] with the job that asked for it ("chat", "diary", ...). Months are
//! calendar months in UTC. Two core tables keep the numbers:
//! - `ai_spend`: the month's total, which the budget warnings check. At 80% and 100% of the
//!   monthly budget there's one warning each, which shows in the log channel.
//! - `ai_spend_jobs`: the estimate and the input tokens per job, for the status message's
//!   cost per job and cache hit rate.
//!
//! These are list prices, so the numbers are an estimate. With an Admin API key,
//! [`Spend::follow_bill`] reads what Anthropic actually billed once an hour (see
//! `claude/billing.rs`) and puts that in place of the month's total. Answers in between
//! still add their estimate on top, until the next bill replaces it again. Without the
//! key, or while the bill can't be read, the estimate is all there is.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use tracing::{info, warn};

use super::claude::billing::Billing;
use super::display_name;
use crate::core::Timers;
use crate::core::db::Db;

/// Shares of the budget that get a warning, once a month each.
const WARN_AT: [i64; 2] = [80, 100];
/// How often the bill is read.
const BILL_EVERY: Duration = Duration::from_secs(60 * 60);

#[derive(Clone)]
pub struct Spend {
    db: Db,
    /// USD per month; 0 means no warnings.
    budget: f64,
    /// How reading the bill with the Admin API key is going.
    admin_key: Arc<Mutex<AdminKey>>,
    /// The last bill that was read, kept while later reads fail.
    last_read: Arc<Mutex<Option<LastRead>>>,
}

/// One read of Anthropic's bill.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LastRead {
    pub at: DateTime<Utc>,
    /// USD billed this month, as of `at`.
    pub usd: f64,
}

/// Whether the bot reads Anthropic's bill with an Admin API key, for the status message.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum AdminKey {
    /// No `ANTHROPIC_ADMIN_KEY` in `.env`.
    #[default]
    Off,
    /// The key is set, but `billed_spend` under `[ai.claude]` is off.
    Unused,
    /// The key is set; the first read hasn't finished yet.
    Starting,
    /// The last read worked.
    Working,
    /// The last read failed, for this reason.
    Failing(String),
}

/// What one response used.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Used {
    pub usd: f64,
    /// Input tokens that weren't cached, read from the cache, and written to it.
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// This month so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Month {
    /// USD: the bill plus answers since, or the estimate.
    pub usd: f64,
    /// Estimated USD per job, most expensive first.
    pub jobs: Vec<(String, f64)>,
    /// Share of input tokens read from the prompt cache, 0 to 1. `None` before any input.
    pub cache_hits: Option<f64>,
}

/// How a new amount changes the month's total.
#[derive(Debug, Clone, Copy)]
enum Change {
    /// Adds one answer's estimate.
    Add(f64),
    /// Replaces the total with what Anthropic billed.
    Set(f64),
}

impl Spend {
    pub fn new(db: Db, budget: f64) -> Spend {
        Spend {
            db,
            budget,
            admin_key: Arc::default(),
            last_read: Arc::default(),
        }
    }

    /// The bot's database, which the Claude provider also uses for its uploads.
    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn budget(&self) -> f64 {
        self.budget
    }

    pub fn admin_key(&self) -> AdminKey {
        self.admin_key.lock().unwrap().clone()
    }

    pub fn last_read(&self) -> Option<LastRead> {
        *self.last_read.lock().unwrap()
    }

    pub fn set_admin_key(&self, state: AdminKey) {
        *self.admin_key.lock().unwrap() = state;
    }

    /// Adds what one response of `job` used to this month. Errors are only logged: a
    /// failed write shouldn't fail the answer.
    pub async fn add(&self, provider: &'static str, job: &'static str, used: Used) {
        let month = this_month();
        let budget = self.budget;
        let result = self
            .db
            .call(move |conn| {
                add_to_job(conn, &month, provider, job, used)?;
                Ok(save(conn, &month, provider, Change::Add(used.usd), budget)?)
            })
            .await;
        self.warn(provider, result);
    }

    /// Reads Claude's bill now and then every hour, for as long as the bot runs.
    pub async fn follow_bill(self, billing: Billing, timers: Timers) {
        self.set_admin_key(AdminKey::Starting);
        let timer = timers.add("Anthropic bill", "hourly");
        let mut failing = false;
        loop {
            timer.running();
            let now = Utc::now();
            match billing.this_month(now).await {
                Ok(usd) => {
                    if failing {
                        info!("reading Anthropic's bill works again");
                    }
                    failing = false;
                    self.set_admin_key(AdminKey::Working);
                    *self.last_read.lock().unwrap() = Some(LastRead { at: now, usd });
                    // The month the bill was read for, which may have ended during the read.
                    let (month, budget) = (month_of(now), self.budget);
                    let result = self
                        .db
                        .call(move |conn| {
                            Ok(save(conn, &month, "claude", Change::Set(usd), budget)?)
                        })
                        .await;
                    self.warn("claude", result);
                }
                Err(err) => {
                    // Warned once, so a bad key doesn't fill the log channel every hour.
                    if failing {
                        info!("still can't read Anthropic's bill: {err:#}");
                    } else {
                        warn!("couldn't read Anthropic's bill, using the estimate: {err:#}");
                    }
                    failing = true;
                    self.set_admin_key(AdminKey::Failing(format!("{err:#}")));
                }
            }
            timer.sleeping(BILL_EVERY);
            tokio::time::sleep(BILL_EVERY).await;
        }
    }

    /// This month so far for `provider`.
    pub async fn this_month(&self, provider: &'static str) -> anyhow::Result<Month> {
        let month = this_month();
        self.db
            .call(move |conn| Ok(read_month(conn, &month, provider)?))
            .await
    }

    /// Logs the warning [`save`] asked for, or why saving failed.
    fn warn(&self, provider: &str, result: anyhow::Result<(f64, Option<i64>)>) {
        match result {
            Ok((total, Some(percent))) => warn!(
                "{} has cost ${total:.2} this month, {percent}% of the ${:.2} budget",
                display_name(provider),
                self.budget
            ),
            Ok((_, None)) => {}
            Err(err) => warn!("couldn't save what {provider} cost: {err:#}"),
        }
    }
}

/// "2026-10".
fn this_month() -> String {
    month_of(Utc::now())
}

/// The month of `time`, like "2026-10".
fn month_of(time: DateTime<Utc>) -> String {
    time.format("%Y-%m").to_string()
}

/// Changes the month's total. Returns the new total, and the budget share to warn about
/// when it passed one it hadn't warned about yet.
fn save(
    conn: &rusqlite::Connection,
    month: &str,
    provider: &str,
    change: Change,
    budget: f64,
) -> rusqlite::Result<(f64, Option<i64>)> {
    let (sql, usd) = match change {
        Change::Add(usd) => ("usd + excluded.usd", usd),
        Change::Set(usd) => ("excluded.usd", usd),
    };
    let (total, warned): (f64, i64) = conn.query_row(
        &format!(
            "INSERT INTO ai_spend (month, provider, usd) VALUES (?1, ?2, ?3)
             ON CONFLICT (month, provider) DO UPDATE SET usd = {sql}
             RETURNING usd, warned"
        ),
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

/// Adds one response to its job's row.
fn add_to_job(
    conn: &rusqlite::Connection,
    month: &str,
    provider: &str,
    job: &str,
    used: Used,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO ai_spend_jobs (month, provider, job, usd, input, cache_read, cache_write)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (month, provider, job) DO UPDATE SET
             usd = usd + excluded.usd,
             input = input + excluded.input,
             cache_read = cache_read + excluded.cache_read,
             cache_write = cache_write + excluded.cache_write",
        params![
            month,
            provider,
            job,
            used.usd,
            used.input,
            used.cache_read,
            used.cache_write
        ],
    )?;
    Ok(())
}

fn read_month(conn: &rusqlite::Connection, month: &str, provider: &str) -> rusqlite::Result<Month> {
    let usd = conn
        .query_row(
            "SELECT usd FROM ai_spend WHERE month = ?1 AND provider = ?2",
            params![month, provider],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0.0);
    let mut statement = conn.prepare(
        "SELECT job, usd, input, cache_read, cache_write FROM ai_spend_jobs
         WHERE month = ?1 AND provider = ?2 ORDER BY usd DESC",
    )?;
    let rows = statement.query_map(params![month, provider], |row| {
        let tokens: (u64, u64, u64) = (row.get(2)?, row.get(3)?, row.get(4)?);
        Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?, tokens))
    })?;
    let mut jobs = Vec::new();
    let (mut read, mut input) = (0, 0);
    for row in rows {
        let (job, usd, (uncached, cache_read, cache_write)) = row?;
        jobs.push((job, usd));
        read += cache_read;
        input += uncached + cache_read + cache_write;
    }
    Ok(Month {
        usd,
        jobs,
        cache_hits: (input > 0).then(|| read as f64 / input as f64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::test_connection;

    #[test]
    fn adds_up_and_warns_once_per_share() {
        let conn = test_connection("test", &[]);
        let add = |usd| save(&conn, "2026-10", "claude", Change::Add(usd), 100.0).unwrap();
        assert_eq!(add(50.0), (50.0, None));
        assert_eq!(add(31.0), (81.0, Some(80)));
        assert_eq!(add(1.0), (82.0, None));
        assert_eq!(add(20.0), (102.0, Some(100)));
        assert_eq!(add(1.0), (103.0, None));
        // A new month starts from zero, and one jump past both shares warns once.
        assert_eq!(
            save(&conn, "2026-11", "claude", Change::Add(120.0), 100.0).unwrap(),
            (120.0, Some(100))
        );
        // No budget, no warnings.
        assert_eq!(
            save(&conn, "2026-11", "openai", Change::Add(500.0), 0.0).unwrap(),
            (500.0, None)
        );
    }

    #[test]
    fn names_the_month() {
        use chrono::TimeZone;
        let last_second = Utc.with_ymd_and_hms(2026, 9, 30, 23, 59, 59).unwrap();
        assert_eq!(month_of(last_second), "2026-09");
    }

    #[test]
    fn the_bill_replaces_the_estimate() {
        let conn = test_connection("test", &[]);
        let save = |change| save(&conn, "2026-10", "claude", change, 100.0).unwrap();
        assert_eq!(save(Change::Add(10.0)), (10.0, None));
        // The bill was higher (code execution, other keys): it wins, and warns.
        assert_eq!(save(Change::Set(85.0)), (85.0, Some(80)));
        assert_eq!(save(Change::Add(1.0)), (86.0, None));
        // A lower bill brings it back down without warning again.
        assert_eq!(save(Change::Set(84.0)), (84.0, None));
    }

    #[test]
    fn splits_the_month_by_job() {
        let conn = test_connection("test", &[]);
        let used = |usd, input, cache_read, cache_write| Used {
            usd,
            input,
            cache_read,
            cache_write,
        };
        for (job, used) in [
            ("chat", used(1.0, 100, 700, 100)),
            ("diary", used(0.5, 100, 0, 0)),
            ("chat", used(2.0, 0, 0, 0)),
        ] {
            add_to_job(&conn, "2026-10", "claude", job, used).unwrap();
            save(&conn, "2026-10", "claude", Change::Add(used.usd), 0.0).unwrap();
        }
        let month = read_month(&conn, "2026-10", "claude").unwrap();
        assert_eq!(month.usd, 3.5);
        assert_eq!(
            month.jobs,
            vec![("chat".to_string(), 3.0), ("diary".to_string(), 0.5)]
        );
        assert_eq!(month.cache_hits, Some(0.7));
        assert_eq!(
            read_month(&conn, "2026-11", "claude").unwrap(),
            Month::default()
        );
    }
}
