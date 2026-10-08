//! Importing reminders from voltgpt's database.
//!
//! voltgpt's table:
//!
//! ```sql
//! reminders (id INTEGER PRIMARY KEY, user_id TEXT, channel_id TEXT, guild_id TEXT,
//!            message TEXT, images TEXT, fire_at INTEGER, created_at INTEGER)
//! ```
//!
//! IDs are strings, `guild_id` is empty in DMs, and `images` is JSON like
//! `[{"filename": "a.png", "data": "<base64>"}]`. Reminders that came due while the bot was
//! down are imported too and go out right after startup.

use anyhow::{Context as _, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, Row, Transaction};
use serde::Deserialize;
use tracing::warn;

use super::store::{self, Image, NewReminder};
use crate::core::legacy::table_exists;

pub fn import(old: &Connection, new: &Transaction) -> anyhow::Result<usize> {
    if !table_exists(old, "reminders")? {
        return Ok(0);
    }
    let mut stmt = old.prepare(
        "SELECT id, user_id, channel_id, guild_id, message, images, fire_at, created_at
         FROM reminders ORDER BY id",
    )?;
    let mut rows = stmt.query([])?;
    let mut imported = 0;
    while let Some(row) = rows.next()? {
        let old_id: i64 = row.get(0)?;
        // One broken row shouldn't block the rest; it's logged and skipped.
        match read_reminder(row) {
            Ok(reminder) => {
                store::insert(new, &reminder)?;
                imported += 1;
            }
            Err(err) => warn!("skipped voltgpt reminder {old_id}: {err:#}"),
        }
    }
    Ok(imported)
}

fn read_reminder(row: &Row) -> anyhow::Result<NewReminder> {
    let images: Option<String> = row.get(5)?;
    Ok(NewReminder {
        user_id: read_id(row, 1)?.context("user_id is empty")?,
        channel_id: read_id(row, 2)?.context("channel_id is empty")?,
        guild_id: read_id(row, 3)?,
        message: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
        fire_at: row.get(6)?,
        created_at: row.get(7)?,
        // voltgpt didn't keep the message that set the reminder.
        source_message_id: None,
        missing_images: 0,
        images: decode_images(images.as_deref()).context("images")?,
    })
}

/// A Discord ID stored as text (voltgpt) or a number. Empty means none.
fn read_id(row: &Row, index: usize) -> anyhow::Result<Option<u64>> {
    match row.get_ref(index)? {
        ValueRef::Null => Ok(None),
        ValueRef::Integer(n) => Ok(Some(u64::try_from(n)?)),
        ValueRef::Text(text) => {
            let text = std::str::from_utf8(text)?.trim();
            if text.is_empty() {
                Ok(None)
            } else {
                Ok(Some(
                    text.parse()
                        .with_context(|| format!("{text:?} is not an ID"))?,
                ))
            }
        }
        other => Err(anyhow!(
            "unexpected {:?} in an ID column",
            other.data_type()
        )),
    }
}

#[derive(Deserialize)]
struct OldImage {
    filename: String,
    data: String,
}

fn decode_images(json: Option<&str>) -> anyhow::Result<Vec<Image>> {
    let Some(json) = json.filter(|json| !json.trim().is_empty()) else {
        return Ok(Vec::new());
    };
    let old: Option<Vec<OldImage>> = serde_json::from_str(json)?;
    old.unwrap_or_default()
        .into_iter()
        .map(|image| {
            Ok(Image {
                data: BASE64.decode(&image.data)?,
                filename: image.filename,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::test_connection;

    /// voltgpt's schema, from its db.go.
    fn old_db() -> Connection {
        let old = Connection::open_in_memory().unwrap();
        old.execute_batch(
            "CREATE TABLE reminders (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id TEXT NOT NULL,
                channel_id TEXT NOT NULL,
                guild_id TEXT NOT NULL,
                message TEXT NOT NULL,
                images TEXT,
                fire_at INTEGER NOT NULL,
                created_at INTEGER NOT NULL DEFAULT (unixepoch())
            );",
        )
        .unwrap();
        old
    }

    #[test]
    fn imports_voltgpt_reminders() {
        let old = old_db();
        let images = format!(
            r#"[{{"filename":"cat.png","data":"{}"}}]"#,
            BASE64.encode([1, 2, 3])
        );
        old.execute(
            "INSERT INTO reminders (user_id, channel_id, guild_id, message, images, fire_at, created_at)
             VALUES ('102087943627243520', '850179179281776670', '122962330165313536', 'feed the cat', ?1, 2000, 1000),
                    ('102087943627243520', '850179179281776670', '', 'in a DM', NULL, 3000, 1000),
                    ('not-a-number', '1', '', 'broken', NULL, 3000, 1000)",
            [images],
        )
        .unwrap();

        let mut new = test_connection("reminders", store::MIGRATIONS);
        let tx = new.transaction().unwrap();
        assert_eq!(import(&old, &tx).unwrap(), 2);
        tx.commit().unwrap();

        let due = store::due(&new, 5000).unwrap();
        assert_eq!(due.len(), 2);
        let cat = &due[0];
        assert_eq!(cat.user_id, 102087943627243520);
        assert_eq!(cat.guild_id, Some(122962330165313536));
        assert_eq!(cat.message, "feed the cat");
        assert_eq!((cat.fire_at, cat.created_at), (2000, 1000));
        assert_eq!(
            cat.images,
            vec![Image {
                filename: "cat.png".into(),
                data: vec![1, 2, 3]
            }]
        );
        assert_eq!(due[1].guild_id, None);
    }

    #[test]
    fn missing_table_imports_nothing() {
        let old = Connection::open_in_memory().unwrap();
        let mut new = test_connection("reminders", store::MIGRATIONS);
        let tx = new.transaction().unwrap();
        assert_eq!(import(&old, &tx).unwrap(), 0);
    }
}
