//! The reminder tables and every query on them. Plain synchronous `rusqlite`; the async code
//! runs these through `ctx.db.call(...)`, and the tests run them on an in-memory database.

use rusqlite::{Connection, OptionalExtension, Row, params};

pub const MIGRATIONS: &[&str] = &[
    // 1
    "CREATE TABLE reminders (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        user_id INTEGER NOT NULL,
        channel_id INTEGER NOT NULL,
        guild_id INTEGER,                 -- NULL in DMs
        message TEXT NOT NULL,
        fire_at INTEGER NOT NULL,         -- unix seconds
        created_at INTEGER NOT NULL,
        next_try_at INTEGER NOT NULL,     -- fire_at, moved later after a failed send
        attempts INTEGER NOT NULL DEFAULT 0,
        sent_at INTEGER                   -- set once delivered, kept a while for snoozing
    );
    CREATE INDEX reminders_due ON reminders (sent_at, next_try_at);
    CREATE INDEX reminders_user ON reminders (user_id, sent_at, fire_at);
    CREATE TABLE reminder_images (
        reminder_id INTEGER NOT NULL REFERENCES reminders (id) ON DELETE CASCADE,
        position INTEGER NOT NULL,
        filename TEXT NOT NULL,
        data BLOB NOT NULL,
        PRIMARY KEY (reminder_id, position)
    );",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub filename: String,
    pub data: Vec<u8>,
}

/// What's needed to create a reminder.
#[derive(Debug, Clone)]
pub struct NewReminder {
    pub user_id: u64,
    pub channel_id: u64,
    pub guild_id: Option<u64>,
    pub message: String,
    pub fire_at: i64,
    pub created_at: i64,
    pub images: Vec<Image>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Reminder {
    pub id: i64,
    pub user_id: u64,
    pub channel_id: u64,
    pub guild_id: Option<u64>,
    pub message: String,
    pub fire_at: i64,
    pub created_at: i64,
    pub attempts: u32,
    pub images: Vec<Image>,
}

/// A pending reminder in `/reminders`.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub id: i64,
    pub message: String,
    pub fire_at: i64,
    pub image_count: usize,
}

/// Stats for the control panel.
#[derive(Debug, Clone, PartialEq)]
pub struct Stats {
    pub pending: usize,
    pub failing: usize,
    pub next_fire_at: Option<i64>,
}

/// Adds a reminder and its images. Call inside a transaction (see [`add`]).
pub fn insert(conn: &Connection, new: &NewReminder) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO reminders (user_id, channel_id, guild_id, message, fire_at, created_at, next_try_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?5)",
        params![new.user_id, new.channel_id, new.guild_id, new.message, new.fire_at, new.created_at],
    )?;
    let id = conn.last_insert_rowid();
    for (position, image) in new.images.iter().enumerate() {
        conn.execute(
            "INSERT INTO reminder_images (reminder_id, position, filename, data) VALUES (?1, ?2, ?3, ?4)",
            params![id, position, image.filename, image.data],
        )?;
    }
    Ok(id)
}

/// Adds a reminder in its own transaction, so it's saved with all its images or not at all.
pub fn add(conn: &mut Connection, new: &NewReminder) -> rusqlite::Result<i64> {
    let tx = conn.transaction()?;
    let id = insert(&tx, new)?;
    tx.commit()?;
    Ok(id)
}

/// When the scheduler should wake up next.
pub fn next_try_at(conn: &Connection) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT min(next_try_at) FROM reminders WHERE sent_at IS NULL",
        [],
        |row| row.get(0),
    )
}

/// Reminders that should be sent now, oldest first.
pub fn due(conn: &Connection, now: i64) -> rusqlite::Result<Vec<Reminder>> {
    let mut stmt = conn.prepare(
        "SELECT id, user_id, channel_id, guild_id, message, fire_at, created_at, attempts
         FROM reminders WHERE sent_at IS NULL AND next_try_at <= ?1
         ORDER BY next_try_at LIMIT 50",
    )?;
    let reminders = stmt
        .query_map([now], reminder_from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    with_images(conn, reminders)
}

/// Any reminder by ID, sent or not.
pub fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<Reminder>> {
    let reminder = conn
        .query_row(
            "SELECT id, user_id, channel_id, guild_id, message, fire_at, created_at, attempts
             FROM reminders WHERE id = ?1",
            [id],
            reminder_from_row,
        )
        .optional()?;
    match reminder {
        Some(reminder) => Ok(with_images(conn, vec![reminder])?.pop()),
        None => Ok(None),
    }
}

pub fn mark_sent(conn: &Connection, id: i64, now: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE reminders SET sent_at = ?2 WHERE id = ?1",
        params![id, now],
    )?;
    Ok(())
}

/// Records a failed send and when to try again.
pub fn mark_failed(conn: &Connection, id: i64, next_try_at: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE reminders SET attempts = attempts + 1, next_try_at = ?2 WHERE id = ?1",
        params![id, next_try_at],
    )?;
    Ok(())
}

/// Deletes a pending reminder, but only if it belongs to `user_id`.
/// Returns whether anything was deleted.
pub fn delete_pending(conn: &Connection, id: i64, user_id: u64) -> rusqlite::Result<bool> {
    let deleted = conn.execute(
        "DELETE FROM reminders WHERE id = ?1 AND user_id = ?2 AND sent_at IS NULL",
        params![id, user_id],
    )?;
    Ok(deleted > 0)
}

/// Deletes a reminder outright (after giving up on sending it).
pub fn delete(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM reminders WHERE id = ?1", [id])?;
    Ok(())
}

/// Forgets reminders that were sent before `before`. They're only kept for the snooze
/// buttons. Returns how many were removed.
pub fn purge_sent(conn: &Connection, before: i64) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM reminders WHERE sent_at < ?1", [before])
}

/// A user's pending reminders, soonest first.
pub fn list_pending(conn: &Connection, user_id: u64) -> rusqlite::Result<Vec<Summary>> {
    let mut stmt = conn.prepare(
        "SELECT r.id, r.message, r.fire_at,
                (SELECT count(*) FROM reminder_images i WHERE i.reminder_id = r.id)
         FROM reminders r WHERE r.user_id = ?1 AND r.sent_at IS NULL
         ORDER BY r.fire_at",
    )?;
    stmt.query_map([user_id], |row| {
        Ok(Summary {
            id: row.get(0)?,
            message: row.get(1)?,
            fire_at: row.get(2)?,
            image_count: row.get(3)?,
        })
    })?
    .collect()
}

pub fn stats(conn: &Connection) -> rusqlite::Result<Stats> {
    conn.query_row(
        "SELECT count(*), count(*) FILTER (WHERE attempts > 0), min(fire_at)
         FROM reminders WHERE sent_at IS NULL",
        [],
        |row| {
            Ok(Stats {
                pending: row.get(0)?,
                failing: row.get(1)?,
                next_fire_at: row.get(2)?,
            })
        },
    )
}

fn reminder_from_row(row: &Row) -> rusqlite::Result<Reminder> {
    Ok(Reminder {
        id: row.get(0)?,
        user_id: row.get(1)?,
        channel_id: row.get(2)?,
        guild_id: row.get(3)?,
        message: row.get(4)?,
        fire_at: row.get(5)?,
        created_at: row.get(6)?,
        attempts: row.get(7)?,
        images: Vec::new(),
    })
}

fn with_images(conn: &Connection, mut reminders: Vec<Reminder>) -> rusqlite::Result<Vec<Reminder>> {
    let mut stmt = conn.prepare(
        "SELECT filename, data FROM reminder_images WHERE reminder_id = ?1 ORDER BY position",
    )?;
    for reminder in &mut reminders {
        reminder.images = stmt
            .query_map([reminder.id], |row| {
                Ok(Image {
                    filename: row.get(0)?,
                    data: row.get(1)?,
                })
            })?
            .collect::<Result<_, _>>()?;
    }
    Ok(reminders)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::test_connection;

    fn conn() -> Connection {
        test_connection("reminders", MIGRATIONS)
    }

    fn new(user_id: u64, fire_at: i64) -> NewReminder {
        NewReminder {
            user_id,
            channel_id: 10,
            guild_id: Some(20),
            message: format!("at {fire_at}"),
            fire_at,
            created_at: 0,
            images: Vec::new(),
        }
    }

    #[test]
    fn add_and_get_with_images() {
        let mut conn = conn();
        let mut reminder = new(1, 100);
        reminder.images = vec![
            Image {
                filename: "a.png".into(),
                data: vec![1, 2],
            },
            Image {
                filename: "b.png".into(),
                data: vec![3],
            },
        ];
        let id = add(&mut conn, &reminder).unwrap();
        let got = get(&conn, id).unwrap().unwrap();
        assert_eq!(got.images, reminder.images);
        assert_eq!(got.guild_id, Some(20));
        assert_eq!(list_pending(&conn, 1).unwrap()[0].image_count, 2);
    }

    #[test]
    fn due_and_retry() {
        let mut conn = conn();
        let early = add(&mut conn, &new(1, 100)).unwrap();
        let late = add(&mut conn, &new(1, 200)).unwrap();

        assert_eq!(next_try_at(&conn).unwrap(), Some(100));
        assert!(due(&conn, 99).unwrap().is_empty());
        assert_eq!(due(&conn, 150).unwrap().len(), 1);

        // A failed send moves it back and counts the attempt.
        mark_failed(&conn, early, 300).unwrap();
        assert_eq!(next_try_at(&conn).unwrap(), Some(200));
        assert_eq!(stats(&conn).unwrap().failing, 1);

        mark_sent(&conn, late, 200).unwrap();
        let due_now = due(&conn, 300).unwrap();
        assert_eq!(due_now.len(), 1);
        assert_eq!(due_now[0].attempts, 1);
        assert_eq!(due_now[0].fire_at, 100);
    }

    #[test]
    fn delete_only_own_pending() {
        let mut conn = conn();
        let id = add(&mut conn, &new(1, 100)).unwrap();
        assert!(!delete_pending(&conn, id, 2).unwrap());
        assert!(delete_pending(&conn, id, 1).unwrap());
        assert!(get(&conn, id).unwrap().is_none());

        let sent = add(&mut conn, &new(1, 100)).unwrap();
        mark_sent(&conn, sent, 100).unwrap();
        assert!(!delete_pending(&conn, sent, 1).unwrap());
    }

    #[test]
    fn purge_removes_images_too() {
        let mut conn = conn();
        let mut reminder = new(1, 100);
        reminder.images = vec![Image {
            filename: "a.png".into(),
            data: vec![1],
        }];
        let id = add(&mut conn, &reminder).unwrap();
        mark_sent(&conn, id, 100).unwrap();
        assert_eq!(purge_sent(&conn, 50).unwrap(), 0);
        assert_eq!(purge_sent(&conn, 150).unwrap(), 1);
        let images: i64 = conn
            .query_row("SELECT count(*) FROM reminder_images", [], |r| r.get(0))
            .unwrap();
        assert_eq!(images, 0);
    }

    #[test]
    fn list_and_stats() {
        let mut conn = conn();
        add(&mut conn, &new(1, 300)).unwrap();
        add(&mut conn, &new(1, 100)).unwrap();
        add(&mut conn, &new(2, 50)).unwrap();
        let list = list_pending(&conn, 1).unwrap();
        assert_eq!(
            list.iter().map(|s| s.fire_at).collect::<Vec<_>>(),
            [100, 300]
        );
        assert_eq!(
            stats(&conn).unwrap(),
            Stats {
                pending: 3,
                failing: 0,
                next_fire_at: Some(50)
            }
        );
    }
}
