//! The memory tables and every query on them. Plain synchronous `rusqlite`; the async code
//! runs these through `ctx.db.call(...)`, and the tests run them on an in-memory database.
//!
//! Each server has one memory folder, and each person has a private one for DMs. A folder
//! is named by its scope: `server:<guild id>` or `dm:<user id>`.

use rusqlite::{Connection, OptionalExtension, params};

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
    // 2: when Vivy last reflected on each server's folder.
    "CREATE TABLE memory_reflections (
        scope TEXT PRIMARY KEY,
        at INTEGER NOT NULL               -- unix seconds
    );",
    // 3: when Vivy last posted her diary in each diary channel.
    "CREATE TABLE memory_diaries (
        channel_id INTEGER PRIMARY KEY,
        at INTEGER NOT NULL               -- unix seconds
    );",
    // 4: the pictures on Vivy's profile in each server (her face and her banner), and when
    // her mood was last checked.
    "CREATE TABLE memory_pictures (
        guild_id INTEGER NOT NULL,
        slot TEXT NOT NULL,               -- avatar or banner
        name TEXT NOT NULL,               -- happy, night, ...
        fingerprint TEXT NOT NULL,        -- of the picture sent, to skip sending it again
        at INTEGER NOT NULL,              -- unix seconds
        PRIMARY KEY (guild_id, slot)
    );
    CREATE TABLE memory_mood_checks (
        scope TEXT PRIMARY KEY,
        at INTEGER NOT NULL               -- unix seconds
    );",
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

/// The files inside directory `dir`, at any depth, in path order.
pub fn files_under(
    conn: &Connection,
    scope: &str,
    dir: &str,
) -> rusqlite::Result<Vec<(String, String)>> {
    // substr instead of LIKE, so a _ or % in a path is just a character.
    let prefix = format!("{dir}/");
    let mut stmt = conn.prepare(
        "SELECT path, content FROM memory_files
         WHERE scope = ?1 AND substr(path, 1, length(?2)) = ?2 ORDER BY path",
    )?;
    let rows = stmt.query_map(params![scope, prefix], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}

/// Server folders due for a reflection: changed since the last one, which was at least
/// `every` seconds ago. Changes by `bot` (Vivy herself, like during a reflection) don't
/// count, or every reflection would make the next one due. Her mood file doesn't count
/// either: mood checks and `set_mood` rewrite it a few times a day, and a quiet server
/// shouldn't get a reflection for that alone.
pub fn due_reflections(
    conn: &Connection,
    now: i64,
    every: i64,
    bot: u64,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT c.scope FROM memory_changes c
         LEFT JOIN memory_reflections r ON r.scope = c.scope
         WHERE c.scope LIKE 'server:%' AND c.user_id != ?2
           AND c.path != '/memories/vivy/mood.md' AND (r.at IS NULL OR r.at <= ?1)
         GROUP BY c.scope
         HAVING max(c.at) > coalesce(max(r.at), 0)",
    )?;
    let rows = stmt.query_map(params![now - every, bot], |row| row.get(0))?;
    rows.collect()
}

/// For the control panel: changes to any folder since `since`, and the last reflection.
pub fn activity(conn: &Connection, since: i64) -> rusqlite::Result<(i64, Option<i64>)> {
    conn.query_row(
        "SELECT (SELECT count(*) FROM memory_changes WHERE at >= ?1),
                (SELECT max(at) FROM memory_reflections)",
        [since],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
}

/// The files changed since the last reflection, with who changed them and how often.
pub fn changes_since_reflection(
    conn: &Connection,
    scope: &str,
) -> rusqlite::Result<Vec<(String, u64, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT path, user_id, count(*) FROM memory_changes
         WHERE scope = ?1 AND at > coalesce((SELECT at FROM memory_reflections WHERE scope = ?1), 0)
         GROUP BY path, user_id ORDER BY path",
    )?;
    let rows = stmt.query_map([scope], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
    rows.collect()
}

pub fn set_reflected(conn: &Connection, scope: &str, at: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO memory_reflections (scope, at) VALUES (?1, ?2)
         ON CONFLICT (scope) DO UPDATE SET at = excluded.at",
        params![scope, at],
    )?;
    Ok(())
}

/// Deletes logged changes from before `before`, to keep `memory_changes` from growing
/// forever. A server's changes since its last reflection stay however old they are: the
/// next reflection still lists them (and a server that never reflected keeps them all).
pub fn prune_changes(conn: &Connection, before: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM memory_changes
         WHERE at < ?1 AND (
            scope NOT LIKE 'server:%'
            OR at <= coalesce(
                (SELECT r.at FROM memory_reflections r WHERE r.scope = memory_changes.scope),
                0
            )
         )",
        [before],
    )
}

/// The newest version of `path` in any server's folder: (scope, content). Whoever asked
/// for it, Vivy decided to write it through the memory tool, so every version counts.
pub fn newest_file(conn: &Connection, path: &str) -> rusqlite::Result<Option<(String, String)>> {
    conn.query_row(
        "SELECT scope, content FROM memory_files
         WHERE path = ?1 AND scope LIKE 'server:%'
         ORDER BY updated_at DESC LIMIT 1",
        [path],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
}

/// `path` in every server's folder that has it: (scope, content).
pub fn every_file(conn: &Connection, path: &str) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT scope, content FROM memory_files
         WHERE path = ?1 AND scope LIKE 'server:%' ORDER BY scope",
    )?;
    let rows = stmt.query_map([path], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}

/// The name of the picture in `slot` ("avatar" or "banner") Vivy has in `guild`, like
/// "happy", and when it was set.
pub fn picture(
    conn: &Connection,
    guild: u64,
    slot: &str,
) -> rusqlite::Result<Option<(String, i64)>> {
    conn.query_row(
        "SELECT name, at FROM memory_pictures WHERE guild_id = ?1 AND slot = ?2",
        params![guild, slot],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
}

pub fn set_picture(
    conn: &Connection,
    guild: u64,
    slot: &str,
    name: &str,
    fingerprint: &str,
    at: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO memory_pictures (guild_id, slot, name, fingerprint, at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (guild_id, slot) DO UPDATE SET
            name = excluded.name, fingerprint = excluded.fingerprint, at = excluded.at",
        params![guild, slot, name, fingerprint, at],
    )?;
    Ok(())
}

/// When the mood in `scope` was last checked (or rewritten by the reflection).
pub fn mood_checked_at(conn: &Connection, scope: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT at FROM memory_mood_checks WHERE scope = ?1",
        [scope],
        |row| row.get(0),
    )
    .optional()
}

pub fn set_mood_checked(conn: &Connection, scope: &str, at: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO memory_mood_checks (scope, at) VALUES (?1, ?2)
         ON CONFLICT (scope) DO UPDATE SET at = excluded.at",
        params![scope, at],
    )?;
    Ok(())
}

/// The files changed since `since`, with their current text (`None` if deleted).
pub fn changed_since(
    conn: &Connection,
    scope: &str,
    since: i64,
) -> rusqlite::Result<Vec<(String, Option<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT c.path, f.content FROM memory_changes c
         LEFT JOIN memory_files f ON f.scope = c.scope AND f.path = c.path
         WHERE c.scope = ?1 AND c.at > ?2 ORDER BY c.path",
    )?;
    let rows = stmt.query_map(params![scope, since], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}

/// When the diary was last posted in `channel`.
pub fn diary_posted_at(conn: &Connection, channel: u64) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT at FROM memory_diaries WHERE channel_id = ?1",
        [channel],
        |row| row.get(0),
    )
    .optional()
}

pub fn set_diary_posted(conn: &Connection, channel: u64, at: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO memory_diaries (channel_id, at) VALUES (?1, ?2)
         ON CONFLICT (channel_id) DO UPDATE SET at = excluded.at",
        params![channel, at],
    )?;
    Ok(())
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
        assert_eq!(
            files_under(&conn, "server:1", "/memories").unwrap(),
            vec![("/memories/b.md".to_string(), "héllo".to_string())]
        );
        assert!(
            files_under(&conn, "server:1", "/memories/b.md")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn reflections_are_due_after_changes_once_a_day() {
        let conn = db();
        let day = 24 * 60 * 60;
        let mut one = Folder::new();
        one.insert("/memories/a.md".into(), "x".into());
        save(&conn, "server:1", &Folder::new(), &one, 7, 100).unwrap();
        save(&conn, "dm:9", &Folder::new(), &one, 9, 100).unwrap();
        // DMs never reflect; the server does, since it changed.
        assert_eq!(
            due_reflections(&conn, 200, day, 0).unwrap(),
            vec!["server:1"]
        );
        assert_eq!(
            changes_since_reflection(&conn, "server:1").unwrap(),
            vec![("/memories/a.md".to_string(), 7, 1)]
        );

        set_reflected(&conn, "server:1", 200).unwrap();
        assert!(
            changes_since_reflection(&conn, "server:1")
                .unwrap()
                .is_empty()
        );
        // No changes since: not due, even a day later.
        assert!(
            due_reflections(&conn, 200 + day, day, 0)
                .unwrap()
                .is_empty()
        );
        // Her own changes (bot id 5) don't make it due either.
        let mut mine = one.clone();
        mine.insert("/memories/vivy/notes.md".into(), "likes soup".into());
        save(&conn, "server:1", &one, &mine, 5, 250).unwrap();
        assert!(
            due_reflections(&conn, 200 + day, day, 5)
                .unwrap()
                .is_empty()
        );
        // A mood change from someone else (set_mood in a chat) doesn't either.
        let mut moody = mine.clone();
        moody.insert("/memories/vivy/mood.md".into(), "mood: sleepy".into());
        save(&conn, "server:1", &mine, &moody, 7, 260).unwrap();
        save(&conn, "server:1", &moody, &mine, 7, 270).unwrap();
        assert!(
            due_reflections(&conn, 200 + day, day, 5)
                .unwrap()
                .is_empty()
        );
        // A change, but less than a day after the last reflection: not yet.
        let mut two = mine.clone();
        two.insert("/memories/b.md".into(), "y".into());
        save(&conn, "server:1", &mine, &two, 7, 300).unwrap();
        assert!(due_reflections(&conn, 400, day, 5).unwrap().is_empty());
        assert_eq!(
            due_reflections(&conn, 200 + day, day, 5).unwrap(),
            vec!["server:1"]
        );
    }

    #[test]
    fn newest_files_changes_and_diaries() {
        let conn = db();
        let mood = |text: &str| {
            let mut f = Folder::new();
            f.insert("/memories/vivy/mood.md".into(), text.into());
            f
        };
        save(
            &conn,
            "server:1",
            &Folder::new(),
            &mood("status: old"),
            0,
            100,
        )
        .unwrap();
        save(
            &conn,
            "server:2",
            &Folder::new(),
            &mood("status: new"),
            0,
            200,
        )
        .unwrap();
        // DM folders don't count.
        save(&conn, "dm:3", &Folder::new(), &mood("status: dm"), 3, 300).unwrap();
        assert_eq!(
            newest_file(&conn, "/memories/vivy/mood.md").unwrap(),
            Some(("server:2".to_string(), "status: new".to_string()))
        );
        assert_eq!(newest_file(&conn, "/memories/none").unwrap(), None);

        let mut gone = mood("status: old");
        gone.insert("/memories/a.md".into(), "a".into());
        save(&conn, "server:1", &mood("status: old"), &gone, 0, 150).unwrap();
        save(&conn, "server:1", &gone, &mood("status: old"), 0, 160).unwrap();
        assert_eq!(
            changed_since(&conn, "server:1", 120).unwrap(),
            vec![("/memories/a.md".to_string(), None)]
        );
        assert_eq!(changed_since(&conn, "server:1", 0).unwrap().len(), 2);

        assert_eq!(diary_posted_at(&conn, 5).unwrap(), None);
        set_diary_posted(&conn, 5, 1000).unwrap();
        assert_eq!(diary_posted_at(&conn, 5).unwrap(), Some(1000));

        assert_eq!(
            every_file(&conn, "/memories/vivy/mood.md").unwrap(),
            vec![
                ("server:1".to_string(), "status: old".to_string()),
                ("server:2".to_string(), "status: new".to_string()),
            ]
        );
    }

    #[test]
    fn faces_and_mood_checks() {
        let conn = db();
        assert_eq!(picture(&conn, 1, "avatar").unwrap(), None);
        set_picture(&conn, 1, "avatar", "happy", "aa", 10).unwrap();
        set_picture(&conn, 1, "avatar", "sleepy", "bb", 20).unwrap();
        set_picture(&conn, 1, "banner", "night", "cc", 30).unwrap();
        assert_eq!(
            picture(&conn, 1, "avatar").unwrap(),
            Some(("sleepy".to_string(), 20))
        );
        assert_eq!(
            picture(&conn, 1, "banner").unwrap(),
            Some(("night".to_string(), 30))
        );

        assert_eq!(mood_checked_at(&conn, "server:1").unwrap(), None);
        set_mood_checked(&conn, "server:1", 5).unwrap();
        set_mood_checked(&conn, "server:1", 7).unwrap();
        assert_eq!(mood_checked_at(&conn, "server:1").unwrap(), Some(7));
    }

    #[test]
    fn prunes_old_changes_but_not_unreflected_ones() {
        let conn = db();
        let mut one = Folder::new();
        one.insert("/memories/a.md".into(), "x".into());
        for scope in ["server:1", "server:2", "dm:3"] {
            save(&conn, scope, &Folder::new(), &one, 7, 100).unwrap();
        }
        save(&conn, "server:1", &one, &Folder::new(), 7, 500).unwrap();
        // Server 1 reflected at 200: its change at 100 can go, the one at 500 stays.
        // Server 2 never reflected, so it keeps everything; the DM has no reflections.
        set_reflected(&conn, "server:1", 200).unwrap();
        assert_eq!(prune_changes(&conn, 1000).unwrap(), 2);
        let left: Vec<(String, i64)> = conn
            .prepare("SELECT scope, at FROM memory_changes ORDER BY scope, at")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            left,
            vec![("server:1".to_string(), 500), ("server:2".to_string(), 100)]
        );
        // Nothing is older than the cutoff: nothing goes.
        assert_eq!(prune_changes(&conn, 50).unwrap(), 0);
    }

    #[test]
    fn scopes() {
        assert_eq!(scope(Some(5), 9), "server:5");
        assert_eq!(scope(None, 9), "dm:9");
    }
}
