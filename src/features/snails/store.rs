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
    // 3: edits, deletes, failed downloads and the startup catch-up.
    "-- Where each picture came from (host and path of the original, without Discord's signed
    -- query), so an edit keeps the fingerprints of pictures that didn't change.
    ALTER TABLE snail_pictures ADD COLUMN source TEXT;
    -- Edits and deletes remove a message's links by message.
    CREATE INDEX snail_links_message ON snail_links (message_id);
    -- The startup catch-up looks for the newest saved message per channel.
    CREATE INDEX snail_messages_channel ON snail_messages (channel_id, message_id);
    -- Pictures that failed to download, retried later. Not tied to snail_messages: a message
    -- whose only picture failed has no row there.
    CREATE TABLE snail_failures (
        message_id INTEGER NOT NULL,
        source TEXT NOT NULL,             -- like snail_pictures.source
        position INTEGER NOT NULL,
        guild_id INTEGER NOT NULL,
        channel_id INTEGER NOT NULL,
        author_id INTEGER NOT NULL,
        host TEXT NOT NULL,               -- who served it, to spot a provider with trouble
        error TEXT NOT NULL,
        first_failed INTEGER NOT NULL,    -- unix seconds
        last_tried INTEGER NOT NULL,
        next_try INTEGER NOT NULL,
        attempts INTEGER NOT NULL,
        PRIMARY KEY (message_id, source)
    );
    CREATE INDEX snail_failures_due ON snail_failures (next_try);
    -- The newest message the backfill or the catch-up read in each channel.
    ALTER TABLE snail_crawl ADD COLUMN newest_id INTEGER;",
    // 4: pictures dropped after their last try.
    "-- Pictures dropped after their last failed try, counted per host for
    -- `/snail_backfill status`.
    CREATE TABLE snail_dropped (
        guild_id INTEGER NOT NULL,
        host TEXT NOT NULL,
        count INTEGER NOT NULL,
        PRIMARY KEY (guild_id, host)
    );",
];

/// A failed picture is tried this many times in all, then dropped: the retry loop takes it
/// off the list and counts it for its host (see [`drop_given_up`]).
pub const MAX_ATTEMPTS: u32 = 3;

/// How long to wait before trying a failed picture again: 10 minutes after the first
/// failure, then 20 (half an hour from the first to the last try).
pub fn retry_delay(attempts: u32) -> i64 {
    600 << attempts.clamp(1, MAX_ATTEMPTS).saturating_sub(1)
}

/// A fingerprinted picture of a message.
#[derive(Debug, Clone)]
pub struct StoredPicture {
    /// Its place in `collect::pictures`.
    pub position: usize,
    /// Host and path of the original (see `collect::Picture::source`).
    pub source: String,
    pub fp: Fingerprint,
}

/// A picture that couldn't be downloaded.
#[derive(Debug, Clone)]
pub struct Failure {
    pub position: usize,
    pub source: String,
    pub host: String,
    pub error: String,
}

/// What was found in one message.
pub struct Indexed {
    pub guild: u64,
    pub channel: u64,
    pub message: u64,
    pub author: u64,
    pub links: Vec<String>,
    pub pictures: Vec<StoredPicture>,
    /// Pictures that failed to download, to try again later.
    pub failed: Vec<Failure>,
    /// Sources of pictures that failed before and weren't tried this time: left to the
    /// retry loop, their rows stay as they are.
    pub waiting: Vec<String>,
}

/// Whether a message was read before: it has saved links or pictures, or a picture waiting
/// to be retried.
pub fn is_indexed(conn: &Connection, message: u64) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM snail_messages WHERE message_id = ?1)
             OR EXISTS (SELECT 1 FROM snail_failures WHERE message_id = ?1)",
        [message],
        |row| row.get(0),
    )
}

/// Saves what a message holds now. It replaces what was saved for it before, so links and
/// pictures an edit removed stop counting. A message with no links or pictures left has no
/// row at all. Failed pictures keep their attempt count across saves; ones that are no
/// longer failing (or no longer in the message) are dropped from the retry list.
pub fn save(tx: &Transaction, found: &Indexed, now: i64) -> rusqlite::Result<()> {
    let id = found.message;
    tx.execute("DELETE FROM snail_links WHERE message_id = ?1", [id])?;
    tx.execute("DELETE FROM snail_pictures WHERE message_id = ?1", [id])?;
    if found.links.is_empty() && found.pictures.is_empty() {
        tx.execute("DELETE FROM snail_messages WHERE message_id = ?1", [id])?;
    } else {
        tx.execute(
            "INSERT OR IGNORE INTO snail_messages (message_id, guild_id, channel_id, author_id)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, found.guild, found.channel, found.author],
        )?;
    }
    for key in &found.links {
        tx.execute(
            "INSERT OR IGNORE INTO snail_links (guild_id, key, message_id) VALUES (?1, ?2, ?3)",
            params![found.guild, key, id],
        )?;
    }
    for picture in &found.pictures {
        insert_picture(tx, id, found.guild, picture)?;
    }

    // Earlier failures of this message: (source, first failed, attempts).
    let earlier: Vec<(String, i64, u32)> = tx
        .prepare("SELECT source, first_failed, attempts FROM snail_failures WHERE message_id = ?1")?
        .query_map([id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    // Rows of pictures no longer failing (or no longer in the message) go. Waiting ones stay
    // untouched; failed ones are written again below.
    for (source, _, _) in &earlier {
        if !found.waiting.contains(source) {
            drop_failure(tx, id, source)?;
        }
    }
    for failure in &found.failed {
        let (first, attempts) = earlier
            .iter()
            .find(|(source, _, _)| *source == failure.source)
            .map(|(_, first, attempts)| (*first, attempts + 1))
            .unwrap_or((now, 1));
        tx.execute(
            "INSERT OR REPLACE INTO snail_failures (message_id, source, position, guild_id,
                 channel_id, author_id, host, error, first_failed, last_tried, next_try, attempts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                id,
                failure.source,
                failure.position as i64,
                found.guild,
                found.channel,
                found.author,
                failure.host,
                failure.error,
                first,
                now,
                now + retry_delay(attempts),
                attempts
            ],
        )?;
    }
    Ok(())
}

fn insert_picture(
    conn: &Connection,
    message: u64,
    guild: u64,
    picture: &StoredPicture,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO snail_pictures (message_id, position, guild_id, aspect, hashes, source)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            message,
            picture.position as i64,
            guild,
            picture.fp.aspect,
            picture.fp.hashes,
            picture.source
        ],
    )?;
    Ok(())
}

/// The link keys saved for a message.
pub fn message_links(conn: &Connection, message: u64) -> rusqlite::Result<Vec<String>> {
    conn.prepare("SELECT key FROM snail_links WHERE message_id = ?1")?
        .query_map([message], |row| row.get(0))?
        .collect()
}

/// A picture saved for a message. `source` is None for pictures saved before it was kept.
pub struct SavedPicture {
    pub position: usize,
    pub source: Option<String>,
    pub fp: Fingerprint,
}

pub fn message_pictures(conn: &Connection, message: u64) -> rusqlite::Result<Vec<SavedPicture>> {
    conn.prepare(
        "SELECT position, source, aspect, hashes FROM snail_pictures WHERE message_id = ?1",
    )?
    .query_map([message], |row| {
        Ok(SavedPicture {
            position: row.get::<_, i64>(0)? as usize,
            source: row.get(1)?,
            fp: Fingerprint {
                aspect: row.get(2)?,
                hashes: row.get(3)?,
            },
        })
    })?
    .collect()
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

/// Whether a message was already counted as a snail.
pub fn is_caught(conn: &Connection, message: u64) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM snail_caught WHERE message_id = ?1)",
        [message],
        |row| row.get(0),
    )
}

/// Snails caught since `since`, in every server.
pub fn caught_since(conn: &Connection, since: i64) -> rusqlite::Result<u64> {
    conn.query_row(
        "SELECT count(*) FROM snail_caught WHERE caught_at >= ?1",
        [since],
        |row| row.get(0),
    )
}

/// Forgets a message that no longer exists: its links, pictures and failed pictures. A
/// snail it was counted as stays counted (it was posted, even if it's gone now).
pub fn forget(conn: &Connection, message: u64) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM snail_messages WHERE message_id = ?1",
        [message],
    )?;
    conn.execute(
        "DELETE FROM snail_failures WHERE message_id = ?1",
        [message],
    )?;
    Ok(())
}

// ---- Failed pictures ----

/// A failed picture whose next try is due.
#[derive(Debug, Clone, PartialEq)]
pub struct DueFailure {
    pub message: u64,
    pub channel: u64,
    pub guild: u64,
    pub author: u64,
    pub source: String,
}

/// Up to `limit` failed pictures due for another try, the longest waiting first.
pub fn due_failures(conn: &Connection, now: i64, limit: i64) -> rusqlite::Result<Vec<DueFailure>> {
    conn.prepare(
        "SELECT message_id, channel_id, guild_id, author_id, source FROM snail_failures
         WHERE next_try <= ?1 AND attempts < ?2 ORDER BY next_try LIMIT ?3",
    )?
    .query_map(params![now, MAX_ATTEMPTS, limit], |row| {
        Ok(DueFailure {
            message: row.get(0)?,
            channel: row.get(1)?,
            guild: row.get(2)?,
            author: row.get(3)?,
            source: row.get(4)?,
        })
    })?
    .collect()
}

/// Sources of a message's failed pictures that the retry loop still tries (not given up).
pub fn waiting_failures(conn: &Connection, message: u64) -> rusqlite::Result<Vec<String>> {
    conn.prepare("SELECT source FROM snail_failures WHERE message_id = ?1 AND attempts < ?2")?
        .query_map(params![message, MAX_ATTEMPTS], |row| row.get(0))?
        .collect()
}

/// A retried picture downloaded: saves its fingerprint and takes it off the list. Returns
/// false (and saves nothing) when the failure is gone: the message was deleted or edited
/// meanwhile, and saving would bring a deleted message back.
pub fn retry_succeeded(
    tx: &Transaction,
    failure: &DueFailure,
    picture: &StoredPicture,
) -> rusqlite::Result<bool> {
    let still_failing: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM snail_failures WHERE message_id = ?1 AND source = ?2)",
        params![failure.message, failure.source],
        |row| row.get(0),
    )?;
    if !still_failing {
        return Ok(false);
    }
    tx.execute(
        "INSERT OR IGNORE INTO snail_messages (message_id, guild_id, channel_id, author_id)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            failure.message,
            failure.guild,
            failure.channel,
            failure.author
        ],
    )?;
    insert_picture(tx, failure.message, failure.guild, picture)?;
    drop_failure(tx, failure.message, &failure.source)?;
    Ok(true)
}

/// A retry failed again: count it and wait longer before the next one.
pub fn retry_failed(
    conn: &Connection,
    message: u64,
    source: &str,
    error: &str,
    now: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE snail_failures SET attempts = attempts + 1, error = ?3, last_tried = ?4,
             next_try = ?4 + (600 << (min(attempts + 1, ?5) - 1))
         WHERE message_id = ?1 AND source = ?2",
        params![message, source, error, now, MAX_ATTEMPTS],
    )?;
    Ok(())
}

/// Moves a message's failed pictures back to `until` without counting a try (used while
/// snails is turned off in their channel).
pub fn postpone_failures(conn: &Connection, message: u64, until: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE snail_failures SET next_try = ?2 WHERE message_id = ?1",
        params![message, until],
    )?;
    Ok(())
}

/// Takes a picture off the retry list, like one an edit removed.
pub fn drop_failure(conn: &Connection, message: u64, source: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM snail_failures WHERE message_id = ?1 AND source = ?2",
        params![message, source],
    )?;
    Ok(())
}

/// Takes every picture that failed [`MAX_ATTEMPTS`] times off the list and counts it in
/// `snail_dropped` for its server and host. Returns how many were dropped.
pub fn drop_given_up(tx: &Transaction) -> rusqlite::Result<usize> {
    tx.execute(
        "INSERT INTO snail_dropped (guild_id, host, count)
         SELECT guild_id, host, count(*) FROM snail_failures WHERE attempts >= ?1
         GROUP BY guild_id, host
         ON CONFLICT (guild_id, host) DO UPDATE SET count = count + excluded.count",
        [MAX_ATTEMPTS],
    )?;
    tx.execute(
        "DELETE FROM snail_failures WHERE attempts >= ?1",
        [MAX_ATTEMPTS],
    )
}

/// How many pictures of each host were dropped in a server, the most first.
pub fn dropped_by_host(conn: &Connection, guild: u64) -> rusqlite::Result<Vec<(String, u64)>> {
    conn.prepare(
        "SELECT host, count FROM snail_dropped WHERE guild_id = ?1 ORDER BY count DESC, host",
    )?
    .query_map([guild], |row| Ok((row.get(0)?, row.get(1)?)))?
    .collect()
}

/// Failed pictures of one host in a server, waiting for another try.
#[derive(Debug, Clone, PartialEq)]
pub struct HostFailures {
    pub host: String,
    pub waiting: u64,
    /// The newest error, as an example.
    pub error: String,
}

/// The failed pictures of a server grouped by host, the most first.
pub fn failures_by_host(conn: &Connection, guild: u64) -> rusqlite::Result<Vec<HostFailures>> {
    // SQLite takes the bare `error` from the row with the max(last_tried).
    conn.prepare(
        "SELECT host, count(*), error, max(last_tried)
         FROM snail_failures WHERE guild_id = ?1 AND attempts < ?2
         GROUP BY host ORDER BY count(*) DESC, host",
    )?
    .query_map(params![guild, MAX_ATTEMPTS], |row| {
        Ok(HostFailures {
            host: row.get(0)?,
            waiting: row.get(1)?,
            error: row.get(2)?,
        })
    })?
    .collect()
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

/// Notes the newest message read in a channel (by the backfill or the catch-up).
pub fn crawl_newest(conn: &Connection, channel: u64, newest: u64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE snail_crawl SET newest_id = max(coalesce(newest_id, 0), ?2) WHERE channel_id = ?1",
        params![channel, newest],
    )?;
    Ok(())
}

/// The channels of a server the backfill has read before (all of it, or at least one
/// page), each with the message to catch up after: the newest one read or saved there. A
/// channel with neither starts after the channel's own ID, which is older than any message
/// in it.
pub fn catch_up_channels(conn: &Connection, guild: u64) -> rusqlite::Result<Vec<(u64, u64)>> {
    conn.prepare(
        "SELECT c.channel_id, max(c.channel_id, coalesce(c.newest_id, 0),
                coalesce((SELECT max(m.message_id) FROM snail_messages m
                          WHERE m.channel_id = c.channel_id), 0))
         FROM snail_crawl c
         WHERE c.guild_id = ?1 AND c.error IS NULL AND (c.finished = 1 OR c.before_id IS NOT NULL)
         ORDER BY c.channel_id",
    )?
    .query_map([guild], |row| Ok((row.get(0)?, row.get(1)?)))?
    .collect()
}

/// Servers with any backfill history, for the startup catch-up.
pub fn crawled_guilds(conn: &Connection) -> rusqlite::Result<Vec<u64>> {
    conn.prepare("SELECT DISTINCT guild_id FROM snail_crawl")?
        .query_map([], |row| row.get(0))?
        .collect()
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
            pictures: pictures
                .into_iter()
                .enumerate()
                .map(|(position, fp)| StoredPicture {
                    position,
                    source: format!("cdn/{message}/{position}.png"),
                    fp,
                })
                .collect(),
            failed: Vec::new(),
            waiting: Vec::new(),
        }
    }

    fn failure(source: &str, host: &str) -> Failure {
        Failure {
            position: 0,
            source: source.to_string(),
            host: host.to_string(),
            error: "403 Forbidden".to_string(),
        }
    }

    fn save_now(conn: &mut Connection, found: &Indexed, now: i64) {
        let tx = conn.transaction().unwrap();
        save(&tx, found, now).unwrap();
        tx.commit().unwrap();
    }

    #[test]
    fn links_and_pictures() {
        let mut conn = test_connection("snails", MIGRATIONS);
        save_now(&mut conn, &indexed(5, &["x:1"], vec![fp(0)]), 0);
        save_now(
            &mut conn,
            &indexed(7, &["x:1", "youtube:a"], vec![fp(0b1111)]),
            0,
        );
        // Nothing to store: not saved.
        save_now(&mut conn, &indexed(8, &[], vec![]), 0);
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
    fn edits_replace_what_was_saved() {
        let mut conn = test_connection("snails", MIGRATIONS);
        save_now(&mut conn, &indexed(5, &["x:1", "x:2"], vec![fp(0)]), 0);
        assert_eq!(message_pictures(&conn, 5).unwrap().len(), 1);
        // The edit dropped x:1 and the picture and added x:3.
        save_now(&mut conn, &indexed(5, &["x:2", "x:3"], vec![]), 0);
        let mut links = message_links(&conn, 5).unwrap();
        links.sort();
        assert_eq!(links, ["x:2", "x:3"]);
        assert!(message_pictures(&conn, 5).unwrap().is_empty());
        let keys = vec!["x:1".to_string()];
        assert!(same_links(&conn, 1, &keys, 9).unwrap().is_empty());
        // Nothing left: the message is gone.
        save_now(&mut conn, &indexed(5, &[], vec![]), 0);
        assert!(!is_indexed(&conn, 5).unwrap());
    }

    #[test]
    fn failed_pictures_are_retried_and_catalogued() {
        let mut conn = test_connection("snails", MIGRATIONS);
        // A message whose only picture failed is still "read before".
        let mut found = indexed(5, &[], vec![]);
        found.failed = vec![failure("pbs.twimg.com/a.jpg", "pbs.twimg.com")];
        save_now(&mut conn, &found, 1_000);
        assert!(is_indexed(&conn, 5).unwrap());
        // Not due before the first wait is over.
        assert!(due_failures(&conn, 1_000, 10).unwrap().is_empty());
        let due = due_failures(&conn, 1_000 + retry_delay(1), 10).unwrap();
        assert_eq!(
            due,
            [DueFailure {
                message: 5,
                channel: 10,
                guild: 1,
                author: 105,
                source: "pbs.twimg.com/a.jpg".into()
            }]
        );

        // Saving the message again (an edit) keeps counting attempts.
        save_now(&mut conn, &found, 2_000);
        let attempts: u32 = conn
            .query_row("SELECT attempts FROM snail_failures", [], |r| r.get(0))
            .unwrap();
        assert_eq!(attempts, 2);
        // Fails until it's given up on.
        for _ in 2..MAX_ATTEMPTS {
            retry_failed(&conn, 5, "pbs.twimg.com/a.jpg", "still 403", 3_000).unwrap();
        }
        assert!(due_failures(&conn, i64::MAX, 10).unwrap().is_empty());
        assert!(failures_by_host(&conn, 1).unwrap().is_empty());
        let drop = |conn: &mut Connection| {
            let tx = conn.transaction().unwrap();
            let dropped = drop_given_up(&tx).unwrap();
            tx.commit().unwrap();
            dropped
        };
        assert_eq!(drop(&mut conn), 1);
        assert_eq!(drop(&mut conn), 0);
        assert_eq!(
            dropped_by_host(&conn, 1).unwrap(),
            [("pbs.twimg.com".to_string(), 1)]
        );
        // Back on the list, to test the rest.
        save_now(&mut conn, &found, 3_000);

        // Another message with a Discord picture that later downloads.
        let mut other = indexed(7, &["x:1"], vec![]);
        other.failed = vec![failure("cdn.discordapp.com/b.png", "cdn.discordapp.com")];
        save_now(&mut conn, &other, 1_000);
        let hosts = failures_by_host(&conn, 1).unwrap();
        // One each, so sorted by name.
        let summary: Vec<(&str, u64)> =
            hosts.iter().map(|h| (h.host.as_str(), h.waiting)).collect();
        assert_eq!(summary, [("cdn.discordapp.com", 1), ("pbs.twimg.com", 1)]);

        let due = due_failures(&conn, i64::MAX, 10).unwrap();
        let picture = StoredPicture {
            position: 0,
            source: "cdn.discordapp.com/b.png".into(),
            fp: fp(0),
        };
        let tx = conn.transaction().unwrap();
        assert!(retry_succeeded(&tx, &due[0], &picture).unwrap());
        tx.commit().unwrap();
        assert_eq!(message_pictures(&conn, 7).unwrap().len(), 1);
        assert_eq!(failures_by_host(&conn, 1).unwrap().len(), 1);

        // Deleting the message forgets its failures too.
        let due5 = DueFailure {
            message: 5,
            channel: 10,
            guild: 1,
            author: 105,
            source: "pbs.twimg.com/a.jpg".into(),
        };
        forget(&conn, 5).unwrap();
        assert!(failures_by_host(&conn, 1).unwrap().is_empty());
        assert!(!is_indexed(&conn, 5).unwrap());
        // A retry that finishes after the delete doesn't bring the message back.
        let tx = conn.transaction().unwrap();
        assert!(!retry_succeeded(&tx, &due5, &picture).unwrap());
        tx.commit().unwrap();
        assert!(!is_indexed(&conn, 5).unwrap());
    }

    #[test]
    fn waiting_failures_are_left_alone() {
        let mut conn = test_connection("snails", MIGRATIONS);
        let mut found = indexed(5, &["x:1"], vec![]);
        found.failed = vec![failure("cdn/a.png", "cdn")];
        save_now(&mut conn, &found, 1_000);
        assert_eq!(waiting_failures(&conn, 5).unwrap(), ["cdn/a.png"]);
        // Saved again (the re-read) with the picture left to the retry loop: no try counted,
        // and its next try doesn't move.
        found.failed = Vec::new();
        found.waiting = vec!["cdn/a.png".to_string()];
        save_now(&mut conn, &found, 2_000);
        let (attempts, next): (u32, i64) = conn
            .query_row("SELECT attempts, next_try FROM snail_failures", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((attempts, next), (1, 1_000 + retry_delay(1)));
        // Gone from the message (neither failed nor waiting): dropped.
        found.waiting = Vec::new();
        save_now(&mut conn, &found, 3_000);
        assert!(waiting_failures(&conn, 5).unwrap().is_empty());
    }

    #[test]
    fn postponed_failures_go_to_the_back() {
        let mut conn = test_connection("snails", MIGRATIONS);
        for (message, at) in [(5, 1_000), (7, 2_000)] {
            let mut found = indexed(message, &[], vec![]);
            found.failed = vec![failure("cdn/x.png", "cdn")];
            save_now(&mut conn, &found, at);
        }
        let now = 10_000;
        let first = |conn: &Connection| due_failures(conn, now, 1).unwrap()[0].message;
        assert_eq!(first(&conn), 5);
        // Postponed (its channel is turned off): the other one is next, and no try counted.
        postpone_failures(&conn, 5, now + 3_600).unwrap();
        assert_eq!(first(&conn), 7);
        let attempts: u32 = conn
            .query_row(
                "SELECT attempts FROM snail_failures WHERE message_id = 5",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(attempts, 1);
    }

    #[test]
    fn retry_waits_grow() {
        assert_eq!(retry_delay(1), 600);
        assert_eq!(retry_delay(2), 1_200);
        assert_eq!(retry_delay(MAX_ATTEMPTS), 600 << (MAX_ATTEMPTS - 1));
        assert_eq!(retry_delay(100), retry_delay(MAX_ATTEMPTS));
    }

    #[test]
    fn catch_up_starts_after_the_newest_message() {
        let mut conn = test_connection("snails", MIGRATIONS);
        let tx = conn.transaction().unwrap();
        let channels = vec![
            (2, "read".to_string()),
            (3, "never read".to_string()),
            (4, "no access".to_string()),
        ];
        add_crawl_channels(&tx, 1, &channels).unwrap();
        tx.commit().unwrap();
        // Channel 10 (from `indexed`) isn't in the crawl; channel 2 is.
        crawl_progress(&conn, 2, Some(50), 100, 0, false).unwrap();
        crawl_newest(&conn, 2, 900).unwrap();
        crawl_failed(&conn, 4, "Missing Access").unwrap();
        assert_eq!(catch_up_channels(&conn, 1).unwrap(), [(2, 900)]);
        // A newer saved message moves the start forward; an older newest_id doesn't move back.
        let mut found = indexed(950, &["x:1"], vec![]);
        found.channel = 2;
        save_now(&mut conn, &found, 0);
        crawl_newest(&conn, 2, 100).unwrap();
        assert_eq!(catch_up_channels(&conn, 1).unwrap(), [(2, 950)]);
        assert_eq!(crawled_guilds(&conn).unwrap(), [1]);
    }

    #[test]
    fn counts_caught_snails() {
        let conn = test_connection("snails", MIGRATIONS);
        record_caught(&conn, 1, 10, 100, 1_000).unwrap();
        record_caught(&conn, 1, 11, 100, 5_000).unwrap();
        // The same message counts once.
        record_caught(&conn, 1, 11, 100, 5_000).unwrap();
        assert_eq!(caught_since(&conn, 0).unwrap(), 2);
        assert!(is_caught(&conn, 11).unwrap() && !is_caught(&conn, 12).unwrap());
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
