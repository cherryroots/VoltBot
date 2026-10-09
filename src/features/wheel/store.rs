//! The wheel tables and every query on them. Plain synchronous `rusqlite`; the async code
//! runs these through `ctx.db.call(...)`, and the tests run them on an in-memory database.
//!
//! Balances are never stored. [`load_season`] reads a season into the plain structs of
//! [`super::ledger`], which computes the money.

use rusqlite::{Connection, OptionalExtension, params};

use super::ledger::{Bet, Round, Season};

pub const MIGRATIONS: &[&str] = &[
    // 1
    "CREATE TABLE wheel_seasons (
        id INTEGER PRIMARY KEY,
        guild_id INTEGER NOT NULL,
        started_at INTEGER NOT NULL,      -- unix seconds
        ended_at INTEGER                  -- NULL while the season is being played
    );
    -- One game per server: at most one season without an end.
    CREATE UNIQUE INDEX wheel_one_active_season ON wheel_seasons (guild_id)
        WHERE ended_at IS NULL;
    CREATE TABLE wheel_options (
        season_id INTEGER NOT NULL REFERENCES wheel_seasons (id) ON DELETE CASCADE,
        user_id INTEGER NOT NULL,
        PRIMARY KEY (season_id, user_id)
    );
    CREATE TABLE wheel_rounds (
        id INTEGER PRIMARY KEY,
        season_id INTEGER NOT NULL REFERENCES wheel_seasons (id) ON DELETE CASCADE,
        number INTEGER NOT NULL,          -- 1, 2, 3, ... within the season
        winner_id INTEGER,
        resolved_at INTEGER,
        UNIQUE (season_id, number)
    );
    CREATE TABLE wheel_claims (
        round_id INTEGER NOT NULL REFERENCES wheel_rounds (id) ON DELETE CASCADE,
        user_id INTEGER NOT NULL,
        PRIMARY KEY (round_id, user_id)
    );
    CREATE TABLE wheel_bets (
        round_id INTEGER NOT NULL REFERENCES wheel_rounds (id) ON DELETE CASCADE,
        by_id INTEGER NOT NULL,
        on_id INTEGER NOT NULL,
        amount INTEGER NOT NULL CHECK (amount > 0),
        PRIMARY KEY (round_id, by_id, on_id)
    );",
];

/// A season's row, without its rounds.
#[derive(Debug, Clone, PartialEq)]
pub struct SeasonInfo {
    pub id: i64,
    pub guild_id: u64,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

/// The server's season that is being played, if any.
pub fn active_season(conn: &Connection, guild: u64) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM wheel_seasons WHERE guild_id = ?1 AND ended_at IS NULL",
        [guild],
        |row| row.get(0),
    )
    .optional()
}

/// The server's active season. Starts one with round 1 if there is none. Run it in a
/// transaction, so a season is never left without its round.
pub fn ensure_season(conn: &Connection, guild: u64, now: i64) -> rusqlite::Result<i64> {
    match active_season(conn, guild)? {
        Some(id) => Ok(id),
        None => start_season(conn, guild, &[], now),
    }
}

/// Starts a season with these options and round 1. The server must have no active season.
/// Run it in a transaction, like [`ensure_season`].
pub fn start_season(
    conn: &Connection,
    guild: u64,
    options: &[u64],
    now: i64,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO wheel_seasons (guild_id, started_at) VALUES (?1, ?2)",
        params![guild, now],
    )?;
    let season = conn.last_insert_rowid();
    for option in options {
        add_option(conn, season, *option)?;
    }
    add_round(conn, season, 1)?;
    Ok(season)
}

/// Ends a season. Its rounds stay, so it can still be viewed.
pub fn end_season(conn: &Connection, season: i64, now: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE wheel_seasons SET ended_at = ?2 WHERE id = ?1",
        params![season, now],
    )?;
    Ok(())
}

/// The active season of every server, as (server, season).
pub fn active_seasons(conn: &Connection) -> rusqlite::Result<Vec<(u64, i64)>> {
    let mut stmt = conn.prepare("SELECT guild_id, id FROM wheel_seasons WHERE ended_at IS NULL")?;
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect()
}

/// Every season of the server, oldest first.
pub fn seasons(conn: &Connection, guild: u64) -> rusqlite::Result<Vec<SeasonInfo>> {
    let mut stmt = conn.prepare(
        "SELECT id, guild_id, started_at, ended_at FROM wheel_seasons
         WHERE guild_id = ?1 ORDER BY id",
    )?;
    stmt.query_map([guild], |row| {
        Ok(SeasonInfo {
            id: row.get(0)?,
            guild_id: row.get(1)?,
            started_at: row.get(2)?,
            ended_at: row.get(3)?,
        })
    })?
    .collect()
}

/// Reads a whole season: its options and its rounds with their claims and bets, all in the
/// order they were added.
pub fn load_season(conn: &Connection, season: i64) -> rusqlite::Result<Season> {
    let mut stmt =
        conn.prepare("SELECT user_id FROM wheel_options WHERE season_id = ?1 ORDER BY rowid")?;
    let options = stmt
        .query_map([season], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<u64>>>()?;

    let mut stmt = conn.prepare(
        "SELECT id, number, winner_id FROM wheel_rounds WHERE season_id = ?1 ORDER BY number",
    )?;
    let mut rounds = stmt
        .query_map([season], |row| {
            Ok(Round {
                id: row.get(0)?,
                number: row.get(1)?,
                winner: row.get(2)?,
                claims: Vec::new(),
                bets: Vec::new(),
            })
        })?
        .collect::<rusqlite::Result<Vec<Round>>>()?;

    let mut claims =
        conn.prepare("SELECT user_id FROM wheel_claims WHERE round_id = ?1 ORDER BY rowid")?;
    // Updating a bet keeps its row, so a changed bet stays in its place, like in voltgpt.
    let mut bets = conn.prepare(
        "SELECT by_id, on_id, amount FROM wheel_bets WHERE round_id = ?1 ORDER BY rowid",
    )?;
    for round in &mut rounds {
        round.claims = claims
            .query_map([round.id], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        round.bets = bets
            .query_map([round.id], |row| {
                Ok(Bet {
                    by: row.get(0)?,
                    on: row.get(1)?,
                    amount: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
    }
    let number = conn.query_row(
        "SELECT count(*) FROM wheel_seasons s, wheel_seasons me
         WHERE me.id = ?1 AND s.guild_id = me.guild_id AND s.id <= me.id",
        [season],
        |row| row.get(0),
    )?;
    Ok(Season {
        id: season,
        number,
        options,
        rounds,
    })
}

pub fn add_round(conn: &Connection, season: i64, number: i64) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO wheel_rounds (season_id, number) VALUES (?1, ?2)",
        params![season, number],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Puts someone on the wheel. Returns false if they already were.
pub fn add_option(conn: &Connection, season: i64, user: u64) -> rusqlite::Result<bool> {
    let added = conn.execute(
        "INSERT OR IGNORE INTO wheel_options (season_id, user_id) VALUES (?1, ?2)",
        params![season, user],
    )?;
    Ok(added > 0)
}

/// Takes someone off the wheel. Returns false if they weren't on it.
pub fn remove_option(conn: &Connection, season: i64, user: u64) -> rusqlite::Result<bool> {
    let removed = conn.execute(
        "DELETE FROM wheel_options WHERE season_id = ?1 AND user_id = ?2",
        params![season, user],
    )?;
    Ok(removed > 0)
}

/// Claims for a player. Returns false if they already claimed this round.
pub fn claim(conn: &Connection, round: i64, user: u64) -> rusqlite::Result<bool> {
    let added = conn.execute(
        "INSERT OR IGNORE INTO wheel_claims (round_id, user_id) VALUES (?1, ?2)",
        params![round, user],
    )?;
    Ok(added > 0)
}

/// Places a bet, or changes the amount of the same player's bet on the same option.
pub fn place_bet(conn: &Connection, round: i64, bet: &Bet) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO wheel_bets (round_id, by_id, on_id, amount) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (round_id, by_id, on_id) DO UPDATE SET amount = excluded.amount",
        params![round, bet.by, bet.on, bet.amount],
    )?;
    Ok(())
}

/// Returns false if there was no such bet.
pub fn remove_bet(conn: &Connection, round: i64, by: u64, on: u64) -> rusqlite::Result<bool> {
    let removed = conn.execute(
        "DELETE FROM wheel_bets WHERE round_id = ?1 AND by_id = ?2 AND on_id = ?3",
        params![round, by, on],
    )?;
    Ok(removed > 0)
}

/// Sets the winner of a round and starts the next one. Returns the new round's ID.
pub fn set_winner(conn: &Connection, round: i64, winner: u64, now: i64) -> rusqlite::Result<i64> {
    let (season, number): (i64, i64) = conn.query_row(
        "SELECT season_id, number FROM wheel_rounds WHERE id = ?1",
        [round],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    conn.execute(
        "UPDATE wheel_rounds SET winner_id = ?2, resolved_at = ?3 WHERE id = ?1",
        params![round, winner, now],
    )?;
    add_round(conn, season, number + 1)
}

/// Clears the winner of a round and deletes the round that setting it started, with that
/// round's claims and bets.
pub fn undo_winner(conn: &Connection, round: i64) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM wheel_rounds
         WHERE (season_id, number) = (SELECT season_id, number + 1 FROM wheel_rounds WHERE id = ?1)",
        [round],
    )?;
    conn.execute(
        "UPDATE wheel_rounds SET winner_id = NULL, resolved_at = NULL WHERE id = ?1",
        [round],
    )?;
    Ok(())
}

/// For the control panel.
pub struct Stats {
    pub games: i64,
    /// Bets in the open rounds of all active seasons.
    pub open_bets: i64,
}

pub fn stats(conn: &Connection) -> rusqlite::Result<Stats> {
    conn.query_row(
        "SELECT
            (SELECT count(*) FROM wheel_seasons WHERE ended_at IS NULL),
            (SELECT count(*) FROM wheel_bets b
               JOIN wheel_rounds r ON r.id = b.round_id
               JOIN wheel_seasons s ON s.id = r.season_id
              WHERE s.ended_at IS NULL AND r.winner_id IS NULL)",
        [],
        |row| {
            Ok(Stats {
                games: row.get(0)?,
                open_bets: row.get(1)?,
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::test_connection;

    const GUILD: u64 = 10;

    fn db() -> Connection {
        test_connection("wheel", MIGRATIONS)
    }

    #[test]
    fn a_season_starts_with_round_one() {
        let conn = db();
        let season = ensure_season(&conn, GUILD, 100).unwrap();
        assert_eq!(ensure_season(&conn, GUILD, 200).unwrap(), season);
        let loaded = load_season(&conn, season).unwrap();
        assert_eq!(loaded.rounds.len(), 1);
        assert_eq!(loaded.rounds[0].number, 1);
    }

    #[test]
    fn one_active_season_per_server() {
        let conn = db();
        start_season(&conn, GUILD, &[], 100).unwrap();
        assert!(start_season(&conn, GUILD, &[], 100).is_err());
        // Another server has its own game.
        start_season(&conn, GUILD + 1, &[], 100).unwrap();
    }

    #[test]
    fn claims_and_bets() {
        let conn = db();
        let season = ensure_season(&conn, GUILD, 100).unwrap();
        add_option(&conn, season, 7).unwrap();
        add_option(&conn, season, 8).unwrap();
        assert!(!add_option(&conn, season, 7).unwrap());
        let round = load_season(&conn, season).unwrap().rounds[0].id;

        assert!(claim(&conn, round, 1).unwrap());
        assert!(!claim(&conn, round, 1).unwrap());
        place_bet(
            &conn,
            round,
            &Bet {
                by: 1,
                on: 7,
                amount: 20,
            },
        )
        .unwrap();
        place_bet(
            &conn,
            round,
            &Bet {
                by: 1,
                on: 8,
                amount: 5,
            },
        )
        .unwrap();
        // The same bet again changes the amount and keeps its place.
        place_bet(
            &conn,
            round,
            &Bet {
                by: 1,
                on: 7,
                amount: 30,
            },
        )
        .unwrap();

        let loaded = load_season(&conn, season).unwrap();
        assert_eq!(loaded.options, [7, 8]);
        assert_eq!(loaded.rounds[0].claims, [1]);
        assert_eq!(
            loaded.rounds[0].bets,
            [
                Bet {
                    by: 1,
                    on: 7,
                    amount: 30
                },
                Bet {
                    by: 1,
                    on: 8,
                    amount: 5
                }
            ]
        );

        assert!(remove_bet(&conn, round, 1, 8).unwrap());
        assert!(!remove_bet(&conn, round, 1, 8).unwrap());
        assert!(remove_option(&conn, season, 8).unwrap());
        assert_eq!(load_season(&conn, season).unwrap().options, [7]);
    }

    #[test]
    fn winner_and_undo() {
        let conn = db();
        let season = ensure_season(&conn, GUILD, 100).unwrap();
        let round = load_season(&conn, season).unwrap().rounds[0].id;

        let next = set_winner(&conn, round, 7, 200).unwrap();
        claim(&conn, next, 1).unwrap();
        let loaded = load_season(&conn, season).unwrap();
        assert_eq!(loaded.rounds.len(), 2);
        assert_eq!(loaded.rounds[0].winner, Some(7));
        assert_eq!(loaded.rounds[1].number, 2);

        undo_winner(&conn, round).unwrap();
        let loaded = load_season(&conn, season).unwrap();
        assert_eq!(loaded.rounds.len(), 1);
        assert_eq!(loaded.rounds[0].winner, None);
        // The new round's claims went with it.
        let claims: i64 = conn
            .query_row("SELECT count(*) FROM wheel_claims", [], |r| r.get(0))
            .unwrap();
        assert_eq!(claims, 0);
    }

    #[test]
    fn ended_seasons_stay() {
        let conn = db();
        let old = ensure_season(&conn, GUILD, 100).unwrap();
        end_season(&conn, old, 150).unwrap();
        assert_eq!(active_season(&conn, GUILD).unwrap(), None);
        let new = start_season(&conn, GUILD, &[7], 200).unwrap();
        let list = seasons(&conn, GUILD).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].ended_at, Some(150));
        assert_eq!(list[1].id, new);
        // Numbered per server.
        let other = start_season(&conn, GUILD + 1, &[], 300).unwrap();
        assert_eq!(load_season(&conn, old).unwrap().number, 1);
        assert_eq!(load_season(&conn, new).unwrap().number, 2);
        assert_eq!(load_season(&conn, other).unwrap().number, 1);
        assert_eq!(
            active_seasons(&conn).unwrap(),
            [(GUILD, new), (GUILD + 1, other)]
        );
        let stats = stats(&conn).unwrap();
        assert_eq!((stats.games, stats.open_bets), (2, 0));
    }
}
