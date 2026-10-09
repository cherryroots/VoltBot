//! The chat history: every message the bot answered and every answer it gave.
//!
//! `chat_turns` holds one row per turn. Turns point at the turn they answer or reply to
//! (`parent_id`), so a conversation is the chain from a turn up to its root, and a
//! regenerated answer is a second child of the same question. Rows are never edited.
//!
//! `chat_messages` maps Discord message IDs to turns. A long answer spans several Discord
//! messages, and replying to any of them continues the same conversation.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::ai::Role;
use crate::util::media::MediaKind;

pub const MIGRATIONS: &[&str] = &[
    // 1
    "CREATE TABLE chat_turns (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        parent_id INTEGER REFERENCES chat_turns (id),
        role TEXT NOT NULL,               -- 'user' or 'assistant'
        author_id INTEGER NOT NULL,       -- who wrote it; the bot for answers
        channel_id INTEGER NOT NULL,
        content_json TEXT NOT NULL,       -- provider-neutral, see StoredPart
        native_json TEXT,                 -- the provider's raw output, for answers
        provider TEXT,                    -- who wrote an answer: 'openai', ...
        model TEXT,
        continuation_id TEXT,             -- e.g. OpenAI's response ID
        created_at INTEGER NOT NULL
    );
    CREATE INDEX chat_turns_parent ON chat_turns (parent_id);
    CREATE TABLE chat_messages (
        message_id INTEGER PRIMARY KEY,
        turn_id INTEGER NOT NULL REFERENCES chat_turns (id)
    );
    CREATE INDEX chat_messages_turn ON chat_messages (turn_id);",
];

/// One piece of a stored turn. Media is kept as a link and downloaded again when an old
/// conversation has to be sent in full.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StoredPart {
    Text {
        text: String,
    },
    Media {
        url: String,
        kind: StoredKind,
        mime: String,
    },
}

/// [`MediaKind`] as stored.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoredKind {
    Image,
    Gif,
    Video,
}

impl From<MediaKind> for StoredKind {
    fn from(kind: MediaKind) -> Self {
        match kind {
            MediaKind::Image => StoredKind::Image,
            MediaKind::Gif => StoredKind::Gif,
            MediaKind::Video => StoredKind::Video,
        }
    }
}

impl From<StoredKind> for MediaKind {
    fn from(kind: StoredKind) -> Self {
        match kind {
            StoredKind::Image => MediaKind::Image,
            StoredKind::Gif => MediaKind::Gif,
            StoredKind::Video => MediaKind::Video,
        }
    }
}

/// Which provider wrote an answer, and how to continue from it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Written {
    pub provider: String,
    pub model: String,
    pub continuation_id: Option<String>,
    pub native_json: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewTurn {
    pub parent_id: Option<i64>,
    pub role: Role,
    pub author_id: u64,
    pub channel_id: u64,
    pub parts: Vec<StoredPart>,
    /// Set for answers.
    pub written: Option<Written>,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Turn {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub role: Role,
    pub author_id: u64,
    pub parts: Vec<StoredPart>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub continuation_id: Option<String>,
}

/// Adds a turn and links its Discord messages to it. Linking a message that already
/// belongs to a turn moves it (a regenerated answer reuses the old answer's messages).
pub fn add_turn(conn: &mut Connection, turn: &NewTurn, messages: &[u64]) -> anyhow::Result<i64> {
    let tx = conn.transaction()?;
    let role = match turn.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let written = turn.written.clone().unwrap_or_default();
    let optional = |s: String| (!s.is_empty()).then_some(s);
    tx.execute(
        "INSERT INTO chat_turns (parent_id, role, author_id, channel_id, content_json,
                                 native_json, provider, model, continuation_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            turn.parent_id,
            role,
            turn.author_id,
            turn.channel_id,
            serde_json::to_string(&turn.parts)?,
            written.native_json,
            optional(written.provider),
            optional(written.model),
            written.continuation_id,
            turn.created_at,
        ],
    )?;
    let id = tx.last_insert_rowid();
    for message in messages {
        tx.execute(
            "INSERT OR REPLACE INTO chat_messages (message_id, turn_id) VALUES (?1, ?2)",
            params![message, id],
        )?;
    }
    tx.commit()?;
    Ok(id)
}

/// The turn a Discord message belongs to.
pub fn turn_for_message(conn: &Connection, message_id: u64) -> anyhow::Result<Option<Turn>> {
    let turn_id: Option<i64> = conn
        .query_row(
            "SELECT turn_id FROM chat_messages WHERE message_id = ?1",
            [message_id],
            |row| row.get(0),
        )
        .optional()?;
    match turn_id {
        Some(id) => get_turn(conn, id),
        None => Ok(None),
    }
}

pub fn get_turn(conn: &Connection, id: i64) -> anyhow::Result<Option<Turn>> {
    let row = conn
        .query_row(
            "SELECT id, parent_id, role, author_id, content_json, provider, model, continuation_id
             FROM chat_turns WHERE id = ?1",
            [id],
            read_row,
        )
        .optional()?;
    row.map(into_turn).transpose()
}

/// The Discord messages of a turn, in order.
pub fn messages_of(conn: &Connection, turn_id: i64) -> anyhow::Result<Vec<u64>> {
    let mut stmt = conn
        .prepare("SELECT message_id FROM chat_messages WHERE turn_id = ?1 ORDER BY message_id")?;
    let ids = stmt
        .query_map([turn_id], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(ids)
}

/// A turn and the turns above it, oldest first, at most `limit` long.
pub fn chain(conn: &Connection, turn_id: i64, limit: usize) -> anyhow::Result<Vec<Turn>> {
    let mut turns = Vec::new();
    let mut next = Some(turn_id);
    while let Some(id) = next {
        if turns.len() >= limit {
            break;
        }
        let Some(turn) = get_turn(conn, id)? else {
            break;
        };
        next = turn.parent_id;
        turns.push(turn);
    }
    turns.reverse();
    Ok(turns)
}

type RawRow = (
    i64,
    Option<i64>,
    String,
    u64,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn read_row(row: &Row) -> rusqlite::Result<RawRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

fn into_turn(row: RawRow) -> anyhow::Result<Turn> {
    let (id, parent_id, role, author_id, content, provider, model, continuation_id) = row;
    Ok(Turn {
        id,
        parent_id,
        role: if role == "assistant" {
            Role::Assistant
        } else {
            Role::User
        },
        author_id,
        parts: serde_json::from_str(&content)?,
        provider,
        model,
        continuation_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::test_connection;

    fn new_turn(parent_id: Option<i64>, role: Role, text: &str) -> NewTurn {
        NewTurn {
            parent_id,
            role,
            author_id: 1,
            channel_id: 2,
            parts: vec![StoredPart::Text { text: text.into() }],
            written: (role == Role::Assistant).then(|| Written {
                provider: "openai".into(),
                model: "m".into(),
                continuation_id: Some(format!("resp_{text}")),
                native_json: Some("[]".into()),
            }),
            created_at: 0,
        }
    }

    #[test]
    fn chains_and_messages() {
        let mut conn = test_connection("chat", MIGRATIONS);
        let question = add_turn(&mut conn, &new_turn(None, Role::User, "q"), &[10]).unwrap();
        let answer = add_turn(
            &mut conn,
            &new_turn(Some(question), Role::Assistant, "a"),
            &[11, 12],
        )
        .unwrap();
        let follow_up =
            add_turn(&mut conn, &new_turn(Some(answer), Role::User, "f"), &[13]).unwrap();

        // Any part of a long answer finds the answer.
        let found = turn_for_message(&conn, 12).unwrap().unwrap();
        assert_eq!(found.id, answer);
        assert_eq!(found.continuation_id.as_deref(), Some("resp_a"));
        assert_eq!(messages_of(&conn, answer).unwrap(), [11, 12]);

        let chain = chain(&conn, follow_up, 50).unwrap();
        assert_eq!(
            chain.iter().map(|t| t.id).collect::<Vec<_>>(),
            [question, answer, follow_up]
        );
        assert_eq!(chain[0].provider, None);
        assert_eq!(super::chain(&conn, follow_up, 2).unwrap().len(), 2);

        // A regenerated answer takes over the old answer's messages.
        let again = add_turn(
            &mut conn,
            &new_turn(Some(question), Role::Assistant, "b"),
            &[11],
        )
        .unwrap();
        assert_eq!(turn_for_message(&conn, 11).unwrap().unwrap().id, again);
        assert_eq!(turn_for_message(&conn, 12).unwrap().unwrap().id, answer);
    }

    #[test]
    fn parts_round_trip() {
        let mut conn = test_connection("chat", MIGRATIONS);
        let mut turn = new_turn(None, Role::User, "look");
        turn.parts.push(StoredPart::Media {
            url: "https://x/a.png".into(),
            kind: StoredKind::Image,
            mime: "image/png".into(),
        });
        let id = add_turn(&mut conn, &turn, &[1]).unwrap();
        assert_eq!(get_turn(&conn, id).unwrap().unwrap().parts, turn.parts);
    }
}
