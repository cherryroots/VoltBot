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

use std::path::Path;

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
        // A panic would end the database thread for good, and every later call would fail
        // until a restart. Catch it here and turn it into a normal error instead.
        let f = move |conn: &mut Connection| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(conn)))
                .unwrap_or_else(|_| Err(anyhow!("the database code panicked")))
        };
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
    // 2: what the AI costs per calendar month (UTC), see `ai::Spend`. `warned` is the
    // highest share of the budget already warned about, in percent.
    "CREATE TABLE ai_spend (
        month TEXT NOT NULL,
        provider TEXT NOT NULL,
        usd REAL NOT NULL,
        warned INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (month, provider)
    );",
    // 3: the estimate and input tokens per job ("chat", "diary", ...), for the status
    // message's cost per job and cache hit rate.
    "CREATE TABLE ai_spend_jobs (
        month TEXT NOT NULL,
        provider TEXT NOT NULL,
        job TEXT NOT NULL,
        usd REAL NOT NULL,
        input INTEGER NOT NULL,
        cache_read INTEGER NOT NULL,
        cache_write INTEGER NOT NULL,
        PRIMARY KEY (month, provider, job)
    );",
];

/// The bot was called VoltBot before it became Vivy, and its database `voltbot.db`. When
/// the database is the new default `vivy.db` and doesn't exist yet, but `voltbot.db` sits
/// next to it, renames the old file (with its WAL files) so no data is lost. Returns true
/// when it renamed something.
pub fn adopt_old_name(path: &Path) -> std::io::Result<bool> {
    if path.file_name() != Some("vivy.db".as_ref()) || path.exists() {
        return Ok(false);
    }
    let old = path.with_file_name("voltbot.db");
    if !old.exists() {
        return Ok(false);
    }
    // The WAL files go first, so a crash halfway leaves the main file under its old name
    // and the next start tries again.
    for suffix in ["-wal", "-shm"] {
        let old_extra = path.with_file_name(format!("voltbot.db{suffix}"));
        if old_extra.exists() {
            std::fs::rename(&old_extra, path.with_file_name(format!("vivy.db{suffix}")))?;
        }
    }
    std::fs::rename(&old, path)?;
    Ok(true)
}

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
    fn adopts_the_old_database_name() {
        let dir = std::env::temp_dir().join(format!("vivy-adopt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("voltbot.db"), "data").unwrap();
        std::fs::write(dir.join("voltbot.db-wal"), "wal").unwrap();
        let path = dir.join("vivy.db");

        assert!(adopt_old_name(&path).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "data");
        assert_eq!(
            std::fs::read_to_string(dir.join("vivy.db-wal")).unwrap(),
            "wal"
        );
        assert!(!dir.join("voltbot.db").exists());
        // Once it exists, nothing happens; another name is never touched.
        assert!(!adopt_old_name(&path).unwrap());
        std::fs::write(dir.join("voltbot.db"), "x").unwrap();
        assert!(!adopt_old_name(&dir.join("other.db")).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
