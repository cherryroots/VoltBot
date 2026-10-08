//! Per-user settings shared by all features. For now only the timezone.

use chrono_tz::Tz;
use rusqlite::{Connection, OptionalExtension};
use serenity::all::UserId;

use super::db::Db;

pub fn timezone_sync(conn: &Connection, user: UserId) -> anyhow::Result<Option<Tz>> {
    let name: Option<Option<String>> = conn
        .query_row(
            "SELECT timezone FROM user_settings WHERE user_id = ?1",
            [user.get()],
            |row| row.get(0),
        )
        .optional()?;
    // A name that no longer parses (renamed zone) is treated as unset.
    Ok(name.flatten().and_then(|name| name.parse().ok()))
}

pub fn set_timezone_sync(conn: &Connection, user: UserId, zone: Tz) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO user_settings (user_id, timezone) VALUES (?1, ?2)
         ON CONFLICT (user_id) DO UPDATE SET timezone = excluded.timezone",
        (user.get(), zone.name()),
    )?;
    Ok(())
}

/// The user's timezone, if they set one with `/timezone`.
pub async fn timezone(db: &Db, user: UserId) -> anyhow::Result<Option<Tz>> {
    db.call(move |conn| timezone_sync(conn, user)).await
}

pub async fn set_timezone(db: &Db, user: UserId, zone: Tz) -> anyhow::Result<()> {
    db.call(move |conn| set_timezone_sync(conn, user, zone))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::test_connection;

    #[test]
    fn timezone_round_trip() {
        let conn = test_connection("none", &[]);
        let user = UserId::new(42);
        assert_eq!(timezone_sync(&conn, user).unwrap(), None);
        set_timezone_sync(&conn, user, Tz::Europe__Oslo).unwrap();
        set_timezone_sync(&conn, user, Tz::Asia__Tokyo).unwrap();
        assert_eq!(timezone_sync(&conn, user).unwrap(), Some(Tz::Asia__Tokyo));
    }
}
