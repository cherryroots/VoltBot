//! The SQLite database: one file, opened once, shared by every feature.
//!
//! `rusqlite` is a blocking library. `tokio-rusqlite` runs it on its own thread, and
//! [`Db::call`] sends a closure to that thread and awaits the answer. The closure gets a
//! plain `&mut rusqlite::Connection`, so the SQL code is ordinary synchronous Rust:
//!
//! ```ignore
//! let count = ctx.db.call(|conn| {
//!     Ok(conn.query_row("SELECT count(*) FROM reminders", [], |row| row.get(0))?)
//! }).await?;
//! ```

use anyhow::anyhow;
use rusqlite::Connection;

#[derive(Clone)]
pub struct Db {
    conn: tokio_rusqlite::Connection,
}

impl Db {
    pub async fn open(path: &str) -> anyhow::Result<Db> {
        let conn = tokio_rusqlite::Connection::open(path).await?;
        let db = Db { conn };
        db.call(configure).await?;
        Ok(db)
    }

    #[cfg(test)]
    pub async fn open_in_memory() -> anyhow::Result<Db> {
        let conn = tokio_rusqlite::Connection::open_in_memory().await?;
        let db = Db { conn };
        db.call(configure).await?;
        Ok(db)
    }

    /// Runs `f` on the database thread and returns its result.
    pub async fn call<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&mut Connection) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        self.conn.call(f).await.map_err(|err| match err {
            tokio_rusqlite::Error::Error(err) => err,
            _ => anyhow!("the database connection is closed"),
        })
    }
}

/// Settings for every connection: WAL lets reads run while a write is in progress,
/// and foreign keys make `ON DELETE CASCADE` work.
pub fn configure(conn: &mut Connection) -> anyhow::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

/// Brings one owner's tables up to date.
///
/// Every feature (and the core) owns a list of SQL migrations. Migration `n` in the list is
/// version `n + 1`. The `schema_migrations` table remembers the highest version that ran
/// for each owner, so only new migrations run. Never edit a migration that has shipped:
/// add a new one to the end of the list instead.
///
/// Returns how many migrations ran.
pub fn migrate(conn: &mut Connection, owner: &str, migrations: &[&str]) -> anyhow::Result<usize> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            owner TEXT NOT NULL,
            version INTEGER NOT NULL,
            applied_at INTEGER NOT NULL,
            PRIMARY KEY (owner, version)
        )",
    )?;
    let current: usize = conn.query_row(
        "SELECT coalesce(max(version), 0) FROM schema_migrations WHERE owner = ?1",
        [owner],
        |row| row.get(0),
    )?;

    let mut ran = 0;
    for (index, sql) in migrations.iter().enumerate().skip(current) {
        let version = index + 1;
        let tx = conn.transaction()?;
        tx.execute_batch(sql)
            .map_err(|err| anyhow!("{owner} migration {version} failed: {err}"))?;
        tx.execute(
            "INSERT INTO schema_migrations (owner, version, applied_at) VALUES (?1, ?2, unixepoch())",
            (owner, version),
        )?;
        tx.commit()?;
        ran += 1;
    }
    Ok(ran)
}

/// Tables that belong to the core rather than to one feature.
pub const CORE_MIGRATIONS: &[&str] = &[
    // 1
    "CREATE TABLE user_settings (
        user_id INTEGER PRIMARY KEY,
        timezone TEXT
    );
    CREATE TABLE legacy_imports (
        part TEXT PRIMARY KEY,
        imported_at INTEGER NOT NULL,
        rows INTEGER NOT NULL
    );",
];

/// An in-memory database with the core tables and the given migrations, for tests.
#[cfg(test)]
pub fn test_connection(owner: &str, migrations: &[&str]) -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    configure(&mut conn).unwrap();
    migrate(&mut conn, "core", CORE_MIGRATIONS).unwrap();
    migrate(&mut conn, owner, migrations).unwrap();
    conn
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_run_once() {
        let mut conn = Connection::open_in_memory().unwrap();
        let v1 = ["CREATE TABLE a (x INTEGER)"];
        assert_eq!(migrate(&mut conn, "test", &v1).unwrap(), 1);
        assert_eq!(migrate(&mut conn, "test", &v1).unwrap(), 0);

        let v2 = [
            "CREATE TABLE a (x INTEGER)",
            "ALTER TABLE a ADD COLUMN y INTEGER",
        ];
        assert_eq!(migrate(&mut conn, "test", &v2).unwrap(), 1);
        conn.execute("INSERT INTO a (x, y) VALUES (1, 2)", [])
            .unwrap();

        // Owners are tracked separately.
        assert_eq!(
            migrate(&mut conn, "other", &["CREATE TABLE b (x)"]).unwrap(),
            1
        );
    }

    #[test]
    fn failed_migration_rolls_back() {
        let mut conn = Connection::open_in_memory().unwrap();
        let bad = ["CREATE TABLE a (x); NOT SQL"];
        assert!(migrate(&mut conn, "test", &bad).is_err());
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0);
    }

    #[tokio::test]
    async fn call_returns_errors() {
        let db = Db::open_in_memory().await.unwrap();
        let err = db
            .call(|conn| Ok(conn.execute("SELECT * FROM missing", [])?))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("missing"), "{err}");
    }
}
