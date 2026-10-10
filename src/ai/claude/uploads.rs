//! Which pictures and files are already uploaded to Claude's Files API, kept in SQLite.
//!
//! Every request sends the whole conversation, with pictures and files by `file_id`. If the
//! same picture got a new `file_id` after a restart, the request would change partway and
//! the prompt cache would miss from there on. So the IDs are saved in `claude_uploads`
//! (by [`ContentKey`], what the bytes hash to), and the same content keeps its ID until the
//! upload expires.
//!
//! Uploads expire (see `UPLOAD_LIFETIME` in `mod.rs`), and can be deleted in the Console.
//! An ID is reused until a day before it expires; one that turns out to be gone anyway is
//! dropped with [`Uploads::forget`] and the file is uploaded again.
//!
//! In tests (no database) the IDs are only kept in memory.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::{OptionalExtension, params};
use tracing::warn;

use super::ContentKey;
use crate::core::db::Db;

/// The `claude` owner's tables, run at startup with every other owner's (see `main.rs`).
pub const MIGRATIONS: &[&str] = &[
    // 1: Files API uploads by content. `content` is a `ContentKey` as text, `expires_at` is
    // seconds since 1970 (UTC).
    "CREATE TABLE claude_uploads (
        content TEXT PRIMARY KEY,
        file_id TEXT NOT NULL,
        expires_at INTEGER NOT NULL
    );",
];

/// An upload isn't reused in the last day before it expires, so it can't expire halfway
/// through an answer.
const REUSE_MARGIN: TimeDelta = TimeDelta::days(1);

pub struct Uploads {
    db: Option<Db>,
    /// IDs already looked up or uploaded since the start, with when they expire, so most
    /// lookups don't need the database.
    known: Mutex<HashMap<ContentKey, (String, DateTime<Utc>)>>,
}

impl Uploads {
    pub fn new(db: Option<Db>) -> Uploads {
        Uploads {
            db,
            known: Mutex::new(HashMap::new()),
        }
    }

    /// The ID of an earlier upload of the same content that's still good to use at `now`.
    pub async fn get(&self, key: ContentKey, now: DateTime<Utc>) -> Option<String> {
        let fresh = |expires_at: DateTime<Utc>| now + REUSE_MARGIN < expires_at;
        if let Some((id, expires_at)) = self.known.lock().unwrap().get(&key)
            && fresh(*expires_at)
        {
            return Some(id.clone());
        }
        let db = self.db.as_ref()?;
        let content = key.to_text();
        let found = db
            .call(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT file_id, expires_at FROM claude_uploads WHERE content = ?1",
                        [content],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                    )
                    .optional()?)
            })
            .await;
        let (id, expires_at) = match found {
            Ok(Some(row)) => row,
            Ok(None) => return None,
            Err(err) => {
                warn!("couldn't look up a Claude upload: {err:#}");
                return None;
            }
        };
        let expires_at = DateTime::from_timestamp(expires_at, 0)?;
        if !fresh(expires_at) {
            return None;
        }
        self.known
            .lock()
            .unwrap()
            .insert(key, (id.clone(), expires_at));
        Some(id)
    }

    /// Remembers a new upload. A failed save is only logged: the upload still works, it
    /// just won't be found after a restart.
    pub async fn put(&self, key: ContentKey, id: &str, expires_at: DateTime<Utc>) {
        self.known
            .lock()
            .unwrap()
            .insert(key, (id.to_string(), expires_at));
        let Some(db) = self.db.as_ref() else {
            return;
        };
        let (content, id) = (key.to_text(), id.to_string());
        let saved = db
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO claude_uploads (content, file_id, expires_at) VALUES (?1, ?2, ?3)
                     ON CONFLICT (content) DO UPDATE
                     SET file_id = excluded.file_id, expires_at = excluded.expires_at",
                    params![content, id, expires_at.timestamp()],
                )?;
                Ok(())
            })
            .await;
        if let Err(err) = saved {
            warn!("couldn't save a Claude upload: {err:#}");
        }
    }

    /// Drops uploads Claude says are gone, so they're uploaded again.
    pub async fn forget(&self, ids: &[String]) {
        self.known
            .lock()
            .unwrap()
            .retain(|_, (id, _)| !ids.contains(id));
        let Some(db) = self.db.as_ref() else {
            return;
        };
        let ids = ids.to_vec();
        let deleted = db
            .call(move |conn| {
                for id in &ids {
                    conn.execute("DELETE FROM claude_uploads WHERE file_id = ?1", [id])?;
                }
                Ok(())
            })
            .await;
        if let Err(err) = deleted {
            warn!("couldn't drop a Claude upload: {err:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn keeps_ids_across_restarts_until_they_expire() {
        let db = Db::open_in_memory().await.unwrap();
        db.call(|conn| {
            crate::core::db::migrate(conn, "claude", MIGRATIONS)?;
            Ok(())
        })
        .await
        .unwrap();
        let now = Utc::now();
        let key = ContentKey::of(b"a picture");
        let uploads = Uploads::new(Some(db.clone()));
        assert_eq!(uploads.get(key, now).await, None);
        uploads.put(key, "file_1", now + TimeDelta::days(30)).await;
        assert_eq!(uploads.get(key, now).await.as_deref(), Some("file_1"));

        // After a restart it's found in the database.
        let restarted = Uploads::new(Some(db.clone()));
        assert_eq!(restarted.get(key, now).await.as_deref(), Some("file_1"));
        // Not in the last day before it expires.
        let late = now + TimeDelta::days(29) + TimeDelta::hours(1);
        assert_eq!(Uploads::new(Some(db.clone())).get(key, late).await, None);
        assert_eq!(restarted.get(key, late).await, None);

        // A new upload replaces it, and a gone one is forgotten everywhere.
        restarted
            .put(key, "file_2", late + TimeDelta::days(30))
            .await;
        assert_eq!(restarted.get(key, late).await.as_deref(), Some("file_2"));
        restarted.forget(&["file_2".to_string()]).await;
        assert_eq!(restarted.get(key, late).await, None);
        assert_eq!(Uploads::new(Some(db)).get(key, late).await, None);
    }

    #[tokio::test]
    async fn works_without_a_database() {
        let now = Utc::now();
        let key = ContentKey::of(b"a file");
        let uploads = Uploads::new(None);
        uploads.put(key, "file_1", now + TimeDelta::days(30)).await;
        assert_eq!(uploads.get(key, now).await.as_deref(), Some("file_1"));
        uploads.forget(&["file_1".to_string()]).await;
        assert_eq!(uploads.get(key, now).await, None);
    }
}
