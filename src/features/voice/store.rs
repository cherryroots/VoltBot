//! The voice messages she sent, to count the characters spoken this month against
//! `monthly_characters`.

use rusqlite::{Connection, params};

pub const MIGRATIONS: &[&str] = &[
    // 1
    "CREATE TABLE voice_messages (
        message_id INTEGER PRIMARY KEY,
        guild_id INTEGER,
        channel_id INTEGER NOT NULL,
        -- Who she answered.
        user_id INTEGER NOT NULL,
        -- Characters sent to ElevenLabs, which is what it bills.
        characters INTEGER NOT NULL,
        seconds REAL NOT NULL,
        at INTEGER NOT NULL
    );
    CREATE INDEX voice_messages_at ON voice_messages (at);",
];

/// One voice message she sent.
pub struct Sent {
    pub message_id: u64,
    pub guild_id: Option<u64>,
    pub channel_id: u64,
    pub user_id: u64,
    pub characters: usize,
    pub seconds: f64,
    pub at: i64,
}

pub fn record(conn: &Connection, sent: &Sent) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO voice_messages
         (message_id, guild_id, channel_id, user_id, characters, seconds, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            sent.message_id,
            sent.guild_id,
            sent.channel_id,
            sent.user_id,
            sent.characters as i64,
            sent.seconds,
            sent.at
        ],
    )?;
    Ok(())
}

/// How many voice messages were sent since `since`, and how many characters they had.
pub fn used_since(conn: &Connection, since: i64) -> rusqlite::Result<(u64, u64)> {
    conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(characters), 0) FROM voice_messages WHERE at >= ?1",
        [since],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::test_connection;

    #[test]
    fn counts_characters() {
        let conn = test_connection("voice", MIGRATIONS);
        assert_eq!(used_since(&conn, 0).unwrap(), (0, 0));
        for (id, at) in [(1, 100), (2, 200)] {
            let sent = Sent {
                message_id: id,
                guild_id: None,
                channel_id: 5,
                user_id: 6,
                characters: 40,
                seconds: 2.5,
                at,
            };
            record(&conn, &sent).unwrap();
        }
        assert_eq!(used_since(&conn, 0).unwrap(), (2, 80));
        assert_eq!(used_since(&conn, 150).unwrap(), (1, 40));
    }
}
