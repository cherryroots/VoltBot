//! The tables and queries. Synchronous; run them with `ctx.db.call`.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::fingerprint::{Fingerprint, HASH_BYTES, HASHES, MATCH_DISTANCE, distance};

pub const MIGRATIONS: &[&str] = &[
    // 1
    "CREATE TABLE snail_messages (
        message_id INTEGER PRIMARY KEY,
        guild_id INTEGER NOT NULL,
        channel_id INTEGER NOT NULL,
        author_id INTEGER NOT NULL
    );
    -- Link keys (see links.rs) in each message.
    CREATE TABLE snail_links (
        guild_id INTEGER NOT NULL,
        key TEXT NOT NULL,
        message_id INTEGER NOT NULL REFERENCES snail_messages (message_id) ON DELETE CASCADE,
        PRIMARY KEY (guild_id, key, message_id)
    );
    -- Fingerprints (see fingerprint.rs) of the pictures in each message. `position` is the
    -- picture's place in `collect::pictures`, to find it again.
    CREATE TABLE snail_pictures (
        message_id INTEGER NOT NULL REFERENCES snail_messages (message_id) ON DELETE CASCADE,
        position INTEGER NOT NULL,
        guild_id INTEGER NOT NULL,
        aspect REAL NOT NULL,
        hashes BLOB NOT NULL,
        PRIMARY KEY (message_id, position)
    );
    CREATE INDEX snail_pictures_guild ON snail_pictures (guild_id);
    -- The backlog crawl: one row per channel or thread, read from newest to oldest.
    CREATE TABLE snail_crawl (
        channel_id INTEGER PRIMARY KEY,
        guild_id INTEGER NOT NULL,
        name TEXT NOT NULL,
        before_id INTEGER,
        finished INTEGER NOT NULL DEFAULT 0,
        messages INTEGER NOT NULL DEFAULT 0,
        pictures INTEGER NOT NULL DEFAULT 0,
        error TEXT
    );
    -- Servers whose crawl an admin started and hasn't paused.
    CREATE TABLE snail_backfills (
        guild_id INTEGER PRIMARY KEY,
        running INTEGER NOT NULL
    );",
    // 2: new messages that turned out to be snails, for the control panel's count.
    "CREATE TABLE snail_caught (
        message_id INTEGER PRIMARY KEY,
        guild_id INTEGER NOT NULL,
        author_id INTEGER NOT NULL,
        caught_at INTEGER NOT NULL        -- unix seconds
    );",
];

/// What was found in one message.
pub struct Indexed {
    pub guild: u64,
    pub channel: u64,
    pub message: u64,
    pub author: u64,
    pub links: Vec<String>,
    /// (position, fingerprint)
    pub pictures: Vec<(usize, Fingerprint)>,
}

pub fn is_indexed(conn: &Connection, message: u64) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM snail_messages WHERE message_id = ?1",
        [message],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
}

/// Saves a message's links and fingerprints. Messages with neither aren't saved.
pub fn save(tx: &Transaction, found: &Indexed) -> rusqlite::Result<()> {
    if found.links.is_empty() && found.pictures.is_empty() {
        return Ok(());
    }
    tx.execute(
        "INSERT OR IGNORE INTO snail_messages (message_id, guild_id, channel_id, author_id)
         VALUES (?1, ?2, ?3, ?4)",
        params![found.message, found.guild, found.channel, found.author],
    )?;
    for key in &found.links {
        tx.execute(
            "INSERT OR IGNORE INTO snail_links (guild_id, key, message_id) VALUES (?1, ?2, ?3)",
            params![found.guild, key, found.message],
        )?;
    }
    for (position, fp) in &found.pictures {
        tx.execute(
            "INSERT OR IGNORE INTO snail_pictures (message_id, position, guild_id, aspect, hashes)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                found.message,
                *position as i64,
                found.guild,
                fp.aspect,
                fp.hashes
            ],
        )?;
    }
    Ok(())
}

/// A stored message.
#[derive(Debug, Clone, PartialEq)]
pub struct Posted {
    pub message: u64,
    pub channel: u64,
    pub author: u64,
}

/// Older messages in the server with one of these link keys.
pub fn same_links(
    conn: &Connection,
    guild: u64,
    keys: &[String],
    before: u64,
) -> rusqlite::Result<Vec<Posted>> {
    let mut found = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT m.message_id, m.channel_id, m.author_id
         FROM snail_links l JOIN snail_messages m USING (message_id)
         WHERE l.guild_id = ?1 AND l.key = ?2 AND l.message_id < ?3
         ORDER BY l.message_id",
    )?;
    for key in keys {
        let rows = stmt.query_map(params![guild, key, before], |row| {
            Ok(Posted {
                message: row.get(0)?,
                channel: row.get(1)?,
                author: row.get(2)?,
            })
        })?;
        for row in rows {
            let posted = row?;
            if !found.contains(&posted) {
                found.push(posted);
            }
        }
    }
    Ok(found)
}

/// A stored picture that is close to one of the pictures being checked.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub posted: Posted,
    pub position: usize,
    /// Which of the checked pictures it is close to.
    pub checked: usize,
    pub distance: u32,
}

/// How many stored pictures [`close_pictures`] reads per call, so other database work can
/// run in between on a big server.
pub const SCAN_CHUNK: i64 = 20_000;

/// One chunk of the scan for older pictures within [`MATCH_DISTANCE`] of `checked`. Reads
/// pictures after `rowid` and returns the candidates plus the rowid to continue from, or
/// None when the scan is done.
pub fn close_pictures(
    conn: &Connection,
    guild: u64,
    checked: &[Fingerprint],
    before: u64,
    after_rowid: i64,
) -> rusqlite::Result<(Vec<Candidate>, Option<i64>)> {
    let mut stmt = conn.prepare_cached(
        "SELECT p.rowid, p.message_id, p.position, p.aspect, p.hashes, m.channel_id, m.author_id
         FROM snail_pictures p JOIN snail_messages m USING (message_id)
         WHERE p.guild_id = ?1 AND p.rowid > ?2 AND p.message_id < ?3
         ORDER BY p.rowid LIMIT ?4",
    )?;
    let mut rows = stmt.query(params![guild, after_rowid, before, SCAN_CHUNK])?;
    let mut found = Vec::new();
    let (mut last, mut count) = (after_rowid, 0);
    while let Some(row) = rows.next()? {
        last = row.get(0)?;
        count += 1;
        let hashes: Vec<u8> = row.get(4)?;
        if hashes.len() != HASHES * HASH_BYTES {
            continue;
        }
        let stored = Fingerprint {
            aspect: row.get(3)?,
            hashes,
        };
        for (i, fp) in checked.iter().enumerate() {
            let d = distance(fp, &stored);
            if d <= MATCH_DISTANCE {
                found.push(Candidate {
                    posted: Posted {
                        message: row.get(1)?,
                        channel: row.get(5)?,
                        author: row.get(6)?,
                    },
                    position: row.get::<_, i64>(2)? as usize,
                    checked: i,
                    distance: d,
                });
            }
        }
    }
    let next = (count == SCAN_CHUNK).then_some(last);
    Ok((found, next))
}

/// Notes that a new message was a snail.
pub fn record_caught(
    conn: &Connection,
    guild: u64,
    message: u64,
    author: u64,
    at: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO snail_caught (message_id, guild_id, author_id, caught_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![message, guild, author, at],
    )?;
    Ok(())
}

/// Snails caught since `since`, in every server.
pub fn caught_since(conn: &Connection, since: i64) -> rusqlite::Result<u64> {
    conn.query_row(
        "SELECT count(*) FROM snail_caught WHERE caught_at >= ?1",
        [since],
        |row| row.get(0),
    )
}

/// Forgets a message that no longer exists.
pub fn forget(conn: &Connection, message: u64) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM snail_messages WHERE message_id = ?1",
        [message],
    )?;
    Ok(())
}

// ---- The backlog crawl ----

/// Adds channels to the crawl. Channels already in it keep their progress.
pub fn add_crawl_channels(
    tx: &Transaction,
    guild: u64,
    channels: &[(u64, String)],
) -> rusqlite::Result<usize> {
    let mut added = 0;
    for (channel, name) in channels {
        added += tx.execute(
            "INSERT OR IGNORE INTO snail_crawl (channel_id, guild_id, name) VALUES (?1, ?2, ?3)",
            params![channel, guild, name],
        )?;
    }
    Ok(added)
}

pub fn set_running(conn: &Connection, guild: u64, running: bool) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO snail_backfills (guild_id, running) VALUES (?1, ?2)
         ON CONFLICT (guild_id) DO UPDATE SET running = excluded.running",
        params![guild, running],
    )?;
    Ok(())
}

pub fn is_running(conn: &Connection, guild: u64) -> rusqlite::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT running FROM snail_backfills WHERE guild_id = ?1",
            [guild],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(false))
}

pub fn running_guilds(conn: &Connection) -> rusqlite::Result<Vec<u64>> {
    let mut stmt = conn.prepare("SELECT guild_id FROM snail_backfills WHERE running = 1")?;
    stmt.query_map([], |row| row.get(0))?.collect()
}

/// The next channel to read: (channel, read messages older than this).
pub fn next_crawl_channel(
    conn: &Connection,
    guild: u64,
) -> rusqlite::Result<Option<(u64, Option<u64>)>> {
    conn.query_row(
        "SELECT channel_id, before_id FROM snail_crawl
         WHERE guild_id = ?1 AND finished = 0 ORDER BY channel_id LIMIT 1",
        [guild],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
}

/// Records one page read from a channel.
pub fn crawl_progress(
    conn: &Connection,
    channel: u64,
    before: Option<u64>,
    messages: usize,
    pictures: usize,
    finished: bool,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE snail_crawl SET before_id = coalesce(?2, before_id), messages = messages + ?3,
             pictures = pictures + ?4, finished = ?5
         WHERE channel_id = ?1",
        params![channel, before, messages as i64, pictures as i64, finished],
    )?;
    Ok(())
}

/// Gives up on a channel the bot can't read.
pub fn crawl_failed(conn: &Connection, channel: u64, error: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE snail_crawl SET finished = 1, error = ?2 WHERE channel_id = ?1",
        params![channel, error],
    )?;
    Ok(())
}

#[derive(Debug, Default, PartialEq)]
pub struct CrawlStatus {
    pub running: bool,
    pub channels: u64,
    pub finished: u64,
    pub failed: u64,
    pub messages: u64,
    pub pictures: u64,
}

pub fn crawl_status(conn: &Connection, guild: u64) -> rusqlite::Result<CrawlStatus> {
    let mut status = conn.query_row(
        "SELECT count(*), coalesce(sum(finished), 0), count(error),
                coalesce(sum(messages), 0), coalesce(sum(pictures), 0)
         FROM snail_crawl WHERE guild_id = ?1",
        [guild],
        |row| {
            Ok(CrawlStatus {
                running: false,
                channels: row.get(0)?,
                finished: row.get(1)?,
                failed: row.get(2)?,
                messages: row.get(3)?,
                pictures: row.get(4)?,
            })
        },
    )?;
    status.running = is_running(conn, guild)?;
    Ok(status)
}

/// Totals for the control panel: (links, pictures, crawl channels finished, crawl channels).
pub fn stats(conn: &Connection) -> rusqlite::Result<(u64, u64, u64, u64)> {
    conn.query_row(
        "SELECT (SELECT count(*) FROM snail_links), (SELECT count(*) FROM snail_pictures),
                (SELECT coalesce(sum(finished), 0) FROM snail_crawl),
                (SELECT count(*) FROM snail_crawl)",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::test_connection;

    fn fp(byte: u8) -> Fingerprint {
        Fingerprint {
            aspect: 1.0,
            hashes: vec![byte; HASHES * HASH_BYTES],
        }
    }

    fn indexed(message: u64, links: &[&str], pictures: Vec<Fingerprint>) -> Indexed {
        Indexed {
            guild: 1,
            channel: 10,
            message,
            author: 100 + message,
            links: links.iter().map(|s| s.to_string()).collect(),
            pictures: pictures.into_iter().enumerate().collect(),
        }
    }

    #[test]
    fn links_and_pictures() {
        let mut conn = test_connection("snails", MIGRATIONS);
        let tx = conn.transaction().unwrap();
        save(&tx, &indexed(5, &["x:1"], vec![fp(0)])).unwrap();
        save(&tx, &indexed(7, &["x:1", "youtube:a"], vec![fp(0b1111)])).unwrap();
        // Nothing to store: not saved.
        save(&tx, &indexed(8, &[], vec![])).unwrap();
        tx.commit().unwrap();
        assert!(is_indexed(&conn, 5).unwrap());
        assert!(!is_indexed(&conn, 8).unwrap());

        let keys = vec!["x:1".to_string()];
        let older = same_links(&conn, 1, &keys, 9).unwrap();
        assert_eq!(older.iter().map(|p| p.message).collect::<Vec<_>>(), [5, 7]);
        // Only older messages, only this server.
        assert_eq!(same_links(&conn, 1, &keys, 7).unwrap().len(), 1);
        assert!(same_links(&conn, 2, &keys, 9).unwrap().is_empty());

        // fp(0) vs fp(0b1111): 4 bits per byte differ, far over the limit.
        let (found, next) = close_pictures(&conn, 1, &[fp(0)], 9, 0).unwrap();
        assert_eq!(next, None);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].posted.message, found[0].distance), (5, 0));

        forget(&conn, 5).unwrap();
        assert!(
            same_links(&conn, 1, &keys, 9)
                .unwrap()
                .iter()
                .all(|p| p.message == 7)
        );
        assert!(
            close_pictures(&conn, 1, &[fp(0)], 9, 0)
                .unwrap()
                .0
                .is_empty()
        );
    }

    #[test]
    fn counts_caught_snails() {
        let conn = test_connection("snails", MIGRATIONS);
        record_caught(&conn, 1, 10, 100, 1_000).unwrap();
        record_caught(&conn, 1, 11, 100, 5_000).unwrap();
        // The same message counts once.
        record_caught(&conn, 1, 11, 100, 5_000).unwrap();
        assert_eq!(caught_since(&conn, 0).unwrap(), 2);
        assert_eq!(caught_since(&conn, 2_000).unwrap(), 1);
    }

    #[test]
    fn crawl_bookkeeping() {
        let mut conn = test_connection("snails", MIGRATIONS);
        let tx = conn.transaction().unwrap();
        let channels = vec![(20, "general".to_string()), (30, "memes".to_string())];
        assert_eq!(add_crawl_channels(&tx, 1, &channels).unwrap(), 2);
        assert_eq!(add_crawl_channels(&tx, 1, &channels).unwrap(), 0);
        tx.commit().unwrap();

        set_running(&conn, 1, true).unwrap();
        assert_eq!(running_guilds(&conn).unwrap(), [1]);
        assert_eq!(next_crawl_channel(&conn, 1).unwrap(), Some((20, None)));
        crawl_progress(&conn, 20, Some(500), 100, 3, false).unwrap();
        assert_eq!(next_crawl_channel(&conn, 1).unwrap(), Some((20, Some(500))));
        crawl_progress(&conn, 20, None, 40, 1, true).unwrap();
        crawl_failed(&conn, 30, "Missing Access").unwrap();
        assert_eq!(next_crawl_channel(&conn, 1).unwrap(), None);

        let status = crawl_status(&conn, 1).unwrap();
        assert_eq!(
            status,
            CrawlStatus {
                running: true,
                channels: 2,
                finished: 2,
                failed: 1,
                messages: 140,
                pictures: 4
            }
        );
        set_running(&conn, 1, false).unwrap();
        assert!(running_guilds(&conn).unwrap().is_empty());
    }
}
