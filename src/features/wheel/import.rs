//! Importing the running game from voltgpt's database.
//!
//! voltgpt keeps the whole game as one JSON value in `game_state (id = 1, data)`:
//!
//! ```json
//! {
//!   "rounds": [{"id": 0, "winner": {"user": null},
//!               "claims": [{"user": {"id": "123", ...}}],
//!               "bets": [{"amount": 20, "by": {"user": {...}}, "on": {"user": {...}}}]}],
//!   "bet_options": [{"user": {...}}],
//!   "players": [{"user": {...}}]
//! }
//! ```
//!
//! Users are whole Discord user objects; only their IDs are kept. voltgpt has one game for
//! the whole bot, so it goes to `main_server` from config.toml. It becomes that server's
//! active season, or an ended one if the server already has a game.

use anyhow::Context as _;
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, Transaction};
use serde::Deserialize;
use tracing::warn;

use super::ledger::Bet;
use super::store;
use crate::core::config::Config;
use crate::core::legacy::table_exists;

pub fn import(old: &Connection, new: &Transaction, config: &Config) -> anyhow::Result<usize> {
    // Without it the import fails, so old.db is kept and the next start tries again.
    let guild = config
        .main_server
        .context("set main_server in config.toml to the server voltgpt's wheel belongs to")?;
    import_into(old, new, guild, Utc::now().timestamp())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct OldGame {
    rounds: Vec<OldRound>,
    bet_options: Vec<OldPlayer>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct OldRound {
    winner: OldPlayer,
    claims: Vec<OldPlayer>,
    bets: Vec<OldBet>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct OldPlayer {
    user: Option<OldUser>,
}

#[derive(Deserialize)]
struct OldUser {
    id: String,
}

#[derive(Deserialize)]
struct OldBet {
    amount: i64,
    by: OldPlayer,
    on: OldPlayer,
}

impl OldPlayer {
    /// The user's ID; `None` for an empty player (a round without a winner).
    fn id(&self) -> Option<u64> {
        let text = &self.user.as_ref()?.id;
        match text.parse() {
            Ok(id) => Some(id),
            Err(_) => {
                warn!("skipped voltgpt wheel user {text:?}: not an ID");
                None
            }
        }
    }
}

/// Returns the number of rounds imported.
fn import_into(old: &Connection, new: &Transaction, guild: u64, now: i64) -> anyhow::Result<usize> {
    if !table_exists(old, "game_state")? {
        return Ok(0);
    }
    let data: Option<String> = old
        .query_row("SELECT data FROM game_state WHERE id = 1", [], |row| {
            row.get(0)
        })
        .optional()?;
    let Some(data) = data else {
        return Ok(0);
    };
    let game: OldGame = serde_json::from_str(&data).context("reading game_state")?;

    // A server that already plays keeps its game; the old one is filed as a past season.
    let existing = store::active_season(new, guild)?;
    if let Some(current) = existing {
        store::end_season(new, current, now)?;
    }
    let options: Vec<u64> = game.bet_options.iter().filter_map(OldPlayer::id).collect();
    let season = store::start_season(new, guild, &options, now)?;
    let mut round = store::load_season(new, season)?.rounds[0].id;

    for (index, old_round) in game.rounds.iter().enumerate() {
        for user in old_round.claims.iter().filter_map(OldPlayer::id) {
            store::claim(new, round, user)?;
        }
        for bet in &old_round.bets {
            let (Some(by), Some(on)) = (bet.by.id(), bet.on.id()) else {
                continue;
            };
            if bet.amount <= 0 {
                warn!("skipped a voltgpt wheel bet of {} by {by}", bet.amount);
                continue;
            }
            let amount = bet.amount;
            store::place_bet(new, round, &Bet { by, on, amount })?;
        }
        let is_last = index + 1 == game.rounds.len();
        // Setting a winner starts the next round, as it did in voltgpt.
        match old_round.winner.id() {
            Some(winner) => round = store::set_winner(new, round, winner, now)?,
            None if !is_last => round = store::add_round(new, season, index as i64 + 2)?,
            None => {}
        }
    }

    if let Some(current) = existing {
        // Put the server's own game back in front.
        store::end_season(new, season, now)?;
        new.execute(
            "UPDATE wheel_seasons SET ended_at = NULL WHERE id = ?1",
            [current],
        )?;
    }
    Ok(game.rounds.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::test_connection;
    use crate::features::wheel::ledger::ledger;

    const GUILD: u64 = 5;

    fn old_db(data: &str) -> Connection {
        let old = Connection::open_in_memory().unwrap();
        old.execute_batch("CREATE TABLE game_state (id INTEGER PRIMARY KEY, data TEXT NOT NULL);")
            .unwrap();
        old.execute("INSERT INTO game_state (id, data) VALUES (1, ?1)", [data])
            .unwrap();
        old
    }

    fn user(id: u64) -> String {
        format!(r#"{{"user": {{"id": "{id}", "username": "u{id}", "global_name": null}}}}"#)
    }

    /// The game from voltgpt's TestStatusEmbedSortsPlayersByBankrollAndBoldsThreshold, as
    /// voltgpt saves it.
    fn voltgpt_game() -> String {
        let (a, b, c) = (user(1), user(2), user(3));
        format!(
            r#"{{
                "rounds": [
                    {{"id": 0, "winner": {b}, "claims": [{a}, {b}, {c}],
                      "bets": [{{"amount": 20, "by": {a}, "on": {b}}}]}},
                    {{"id": 1, "winner": {{"user": null}}, "claims": [{a}],
                      "bets": [{{"amount": 30, "by": {a}, "on": {b}}}]}}
                ],
                "bet_options": [{a}, {b}, {c}],
                "players": [{a}, {b}, {c}]
            }}"#
        )
    }

    #[test]
    fn same_balances_as_voltgpt() {
        let old = old_db(&voltgpt_game());
        let mut new = test_connection("wheel", store::MIGRATIONS);
        let tx = new.transaction().unwrap();
        assert_eq!(import_into(&old, &tx, GUILD, 100).unwrap(), 2);
        tx.commit().unwrap();

        let season = store::active_season(&new, GUILD).unwrap().unwrap();
        let season = store::load_season(&new, season).unwrap();
        assert_eq!(season.options, [1, 2, 3]);
        assert_eq!(season.rounds.len(), 2);
        assert_eq!(season.rounds[0].winner, Some(2));
        assert_eq!(season.rounds[1].winner, None);
        // What voltgpt's status embed shows for round 2.
        let current = &ledger(&season)[1];
        assert_eq!(current.money(1), 240);
        assert_eq!(current.money(2), 70);
        assert_eq!(current.money(3), 70);
        assert_eq!(current.standing(1).unwrap().bet_percent, 12);
    }

    #[test]
    fn needs_main_server() {
        let old = old_db(&voltgpt_game());
        let mut new = test_connection("wheel", store::MIGRATIONS);
        let tx = new.transaction().unwrap();
        assert!(import(&old, &tx, &Config::default()).is_err());
        let config = Config::parse("main_server = 5").unwrap();
        assert_eq!(import(&old, &tx, &config).unwrap(), 2);
        assert!(store::active_season(&tx, GUILD).unwrap().is_some());
    }

    #[test]
    fn an_existing_game_stays_active() {
        let old = old_db(&voltgpt_game());
        let mut new = test_connection("wheel", store::MIGRATIONS);
        let mine = store::start_season(&new, GUILD, &[9], 50).unwrap();
        let tx = new.transaction().unwrap();
        import_into(&old, &tx, GUILD, 100).unwrap();
        tx.commit().unwrap();

        assert_eq!(store::active_season(&new, GUILD).unwrap(), Some(mine));
        let seasons = store::seasons(&new, GUILD).unwrap();
        assert_eq!(seasons.len(), 2);
        assert_eq!(seasons[1].ended_at, Some(100));
    }

    #[test]
    fn empty_or_missing_game() {
        let mut new = test_connection("wheel", store::MIGRATIONS);
        let tx = new.transaction().unwrap();
        let none = Connection::open_in_memory().unwrap();
        assert_eq!(import_into(&none, &tx, GUILD, 100).unwrap(), 0);

        // A reset game: no rounds yet, options kept.
        let old = old_db(&format!(
            r#"{{"rounds": [], "bet_options": [{}], "players": []}}"#,
            user(7)
        ));
        assert_eq!(import_into(&old, &tx, GUILD, 100).unwrap(), 0);
        let season = store::active_season(&tx, GUILD).unwrap().unwrap();
        let season = store::load_season(&tx, season).unwrap();
        assert_eq!(season.options, [7]);
        assert_eq!(season.rounds.len(), 1);
    }
}
