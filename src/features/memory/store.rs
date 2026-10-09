//! The memory tables and every query on them. Plain synchronous `rusqlite`; the async code
//! runs these through `ctx.db.call(...)`, and the tests run them on an in-memory database.
//!
//! Each server has one memory folder, and each person has a private one for DMs. A folder
//! is named by its scope: `server:<guild id>` or `dm:<user id>`.

use rusqlite::{Connection, params};

use super::folder::Folder;

pub const MIGRATIONS: &[&str] = &[
    // 1
    "CREATE TABLE memory_files (
        scope TEXT NOT NULL,              -- server:<guild id> or dm:<user id>
        path TEXT NOT NULL,               -- /memories/...
        content TEXT NOT NULL,
        updated_at INTEGER NOT NULL,      -- unix seconds
        updated_by INTEGER NOT NULL,      -- the user who asked for the change
        PRIMARY KEY (scope, path)
    );
    -- Every change, so a wrong or mean note can be traced and undone by hand.
    CREATE TABLE memory_changes (
        id INTEGER PRIMARY KEY,
        scope TEXT NOT NULL,
        path TEXT NOT NULL,
        at INTEGER NOT NULL,
        user_id INTEGER NOT NULL,
        before TEXT,                      -- NULL for a new file
        after TEXT                        -- NULL for a deleted file
    );
    CREATE INDEX memory_changes_by_scope ON memory_changes (scope, at);",
];

/// The folder of a server, or of a person's DMs.
pub fn scope(guild: Option<u64>, user: u64) -> String {
    match guild {
        Some(guild) => format!("server:{guild}"),
        None => format!("dm:{user}"),
    }
}

/// Every file in a folder.
pub fn load(conn: &Connection, scope: &str) -> rusqlite::Result<Folder> {
    let mut stmt = conn.prepare("SELECT path, content FROM memory_files WHERE scope = ?1")?;
    let rows = stmt.query_map([scope], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}

/// Writes what changed between `before` and `after`, and logs each change.
pub fn save(
    conn: &Connection,
    scope: &str,
    before: &Folder,
    after: &Folder,
    user: u64,
    now: i64,
) -> rusqlite::Result<usize> {
    let changes = super::folder::changes(before, after);
    for (path, content) in &changes {
        match content {
            Some(content) => conn.execute(
                "INSERT INTO memory_files (scope, path, content, updated_at, updated_by)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (scope, path) DO UPDATE SET
                    content = excluded.content,
                    updated_at = excluded.updated_at,
                    updated_by = excluded.updated_by",
                params![scope, path, content, now, user],
            )?,
            None => conn.execute(
                "DELETE FROM memory_files WHERE scope = ?1 AND path = ?2",
                params![scope, path],
            )?,
        };
        conn.execute(
            "INSERT INTO memory_changes (scope, path, at, user_id, before, after)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![scope, path, now, user, before.get(*path), content],
        )?;
    }
    Ok(changes.len())
}

/// The paths and sizes (in bytes) of a folder's files, without their contents.
pub fn list(conn: &Connection, scope: &str) -> rusqlite::Result<Vec<(String, usize)>> {
    let mut stmt = conn.prepare(
        "SELECT path, length(CAST(content AS BLOB)) FROM memory_files
         WHERE scope = ?1 ORDER BY path",
    )?;
    let rows = stmt.query_map([scope], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}

/// Files and folders in use, for the control panel.
pub fn stats(conn: &Connection) -> rusqlite::Result<(i64, i64)> {
    conn.query_row(
        "SELECT count(*), count(DISTINCT scope) FROM memory_files",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::core::db::migrate(&mut conn, "memory", MIGRATIONS).unwrap();
        conn
    }

    #[test]
    fn saves_only_changes_and_logs_them() {
        let conn = db();
        let empty = Folder::new();
        let mut one = Folder::new();
        one.insert("/memories/a.md".into(), "hello".into());
        one.insert("/memories/b.md".into(), "héllo".into());
        assert_eq!(save(&conn, "server:1", &empty, &one, 7, 100).unwrap(), 2);
        assert_eq!(load(&conn, "server:1").unwrap(), one);
        assert!(load(&conn, "server:2").unwrap().is_empty());

        let mut two = one.clone();
        two.remove("/memories/a.md");
        assert_eq!(save(&conn, "server:1", &one, &two, 8, 200).unwrap(), 1);
        assert_eq!(load(&conn, "server:1").unwrap(), two);
        // Sizes are in bytes, like the limits.
        assert_eq!(
            list(&conn, "server:1").unwrap(),
            vec![("/memories/b.md".to_string(), 6)]
        );

        let log: Vec<(String, i64, Option<String>, Option<String>)> = conn
            .prepare("SELECT path, user_id, before, after FROM memory_changes ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(log.len(), 3);
        assert_eq!(
            log[2],
            ("/memories/a.md".into(), 8, Some("hello".into()), None)
        );
        assert_eq!(stats(&conn).unwrap(), (1, 1));
    }

    #[test]
    fn scopes() {
        assert_eq!(scope(Some(5), 9), "server:5");
        assert_eq!(scope(None, 9), "dm:9");
    }
}
