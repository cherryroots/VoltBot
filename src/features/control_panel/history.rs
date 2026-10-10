//! Numbers the status picture draws as graphs, saved every 15 minutes.
//!
//! Each sample is a name ("latency_ms", "spend:claude", "stat:reminders:Pending"), a time
//! and a number, in `control_panel_samples`. Samples older than [`KEEP_DAYS`] are deleted,
//! so the table stays small: about 100 rows per name per day.

use std::collections::HashMap;

use rusqlite::{Connection, params};

/// Seconds between samples.
pub const EVERY_SECS: i64 = 15 * 60;
/// Days of samples kept: enough for a whole month of spend.
const KEEP_DAYS: i64 = 32;

// The names of the samples that aren't a feature's stat.
pub const LATENCY: &str = "latency_ms";
pub const MEMORY: &str = "memory_mb";
pub const DATABASE: &str = "database_mb";
/// Claude's spend this month so far, in USD.
pub const SPEND: &str = "spend:claude";

/// The name for a feature's stat, like "stat:reminders:Pending".
pub fn stat_key(feature: &str, stat: &str) -> String {
    format!("stat:{feature}:{stat}")
}

/// Samples per name, oldest first: `(unix seconds, value)`.
pub type History = HashMap<String, Vec<(i64, f64)>>;

/// Saves one sample per name at `at`, and deletes old ones.
pub fn record(conn: &Connection, at: i64, values: &[(String, f64)]) -> rusqlite::Result<()> {
    let mut insert = conn.prepare_cached(
        "INSERT OR REPLACE INTO control_panel_samples (name, at, value) VALUES (?1, ?2, ?3)",
    )?;
    for (name, value) in values {
        insert.execute(params![name, at, value])?;
    }
    conn.execute(
        "DELETE FROM control_panel_samples WHERE at < ?1",
        [at - KEEP_DAYS * 86_400],
    )?;
    Ok(())
}

/// Every sample since `since`.
pub fn load(conn: &Connection, since: i64) -> rusqlite::Result<History> {
    let mut statement = conn.prepare_cached(
        "SELECT name, at, value FROM control_panel_samples WHERE at >= ?1 ORDER BY name, at",
    )?;
    let rows = statement.query_map([since], |row| {
        Ok((row.get::<_, String>(0)?, row.get(1)?, row.get(2)?))
    })?;
    let mut history = History::new();
    for row in rows {
        let (name, at, value) = row?;
        history.entry(name).or_default().push((at, value));
    }
    Ok(history)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::test_connection;
    use crate::features::control_panel::status::MIGRATIONS;

    #[test]
    fn saves_loads_and_forgets() {
        let conn = test_connection("control_panel", MIGRATIONS);
        let day = 86_400;
        record(&conn, 0, &[("a".into(), 1.0), ("b".into(), 5.0)]).unwrap();
        record(&conn, 10 * day, &[("a".into(), 2.0)]).unwrap();
        let history = load(&conn, 0).unwrap();
        assert_eq!(history["a"], vec![(0, 1.0), (10 * day, 2.0)]);
        assert_eq!(history["b"], vec![(0, 5.0)]);
        assert_eq!(load(&conn, 1).unwrap()["a"], vec![(10 * day, 2.0)]);
        // A sample more than 32 days later deletes the first ones.
        record(&conn, 40 * day, &[("a".into(), 3.0)]).unwrap();
        let history = load(&conn, 0).unwrap();
        assert_eq!(history["a"], vec![(10 * day, 2.0), (40 * day, 3.0)]);
        assert!(!history.contains_key("b"));
    }
}
