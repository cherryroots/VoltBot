//! `get_wheel_status`: the wheel game for chat, read only.

use serde_json::{Value, json};

use super::actions::Game;
use super::ledger::{self, OutcomeKind, Season, outcomes};
use super::ui::{Names, name};
use super::{names, store};
use crate::ai::ToolDef;
use crate::core::{Asker, BotCtx, Result, user_error};

pub fn defs() -> Vec<ToolDef> {
    vec![ToolDef {
        name: "get_wheel_status",
        description: "The movie wheel betting game of this server: a round's state, the options left on the wheel, past winners, every player's money, the claims and bets, and how a resolved round paid out. Read only.",
        parameters: json!({
            "type": "object",
            "properties": {
                "round": {"type": "integer", "description": "Which round. Default: the current one."}
            },
        }),
    }]
}

pub async fn run(ctx: &BotCtx, asker: &Asker, args: &Value) -> Result<String> {
    let Some(guild) = asker.guild else {
        return Err(user_error("The wheel only exists in servers."));
    };
    let game: Option<Game> = ctx
        .db
        .call(move |conn| {
            let Some(id) = store::active_season(conn, guild.get())? else {
                return Ok(None);
            };
            Ok(Some(Game {
                season: store::load_season(conn, id)?,
                active: true,
            }))
        })
        .await?;
    let Some(game) = game else {
        return Ok("No movie wheel game is running in this server.".to_string());
    };

    let count = game.season.rounds.len();
    let index = match args["round"].as_i64() {
        None => count - 1,
        Some(number) => usize::try_from(number - 1)
            .ok()
            .filter(|&n| n < count)
            .ok_or_else(|| user_error(format!("The rounds are 1 to {count}.")))?,
    };
    let names = names::lookup(ctx, guild, &ledger::users(&game.season)).await;
    Ok(describe(&game.season, index, &names, asker.user.get()))
}

/// The round as plain text for the model.
fn describe(season: &Season, index: usize, names: &Names, asker: u64) -> String {
    let numbers = ledger::ledger(season);
    let round = &season.rounds[index];
    let current = &numbers[index];
    let n = |user: u64| name(names, user);
    let list = |users: &[u64]| {
        let names: Vec<&str> = users.iter().map(|&u| n(u)).collect();
        if names.is_empty() {
            "none".to_string()
        } else {
            names.join(", ")
        }
    };

    let mut lines = Vec::new();
    let state = match round.winner {
        Some(winner) => format!("resolved, won by {}", n(winner)),
        None if index + 1 == season.rounds.len() => "open for claims and bets".to_string(),
        None => "over".to_string(),
    };
    lines.push(format!(
        "Movie wheel, round {} of {} ({state}).",
        round.number,
        season.rounds.len()
    ));
    lines.push(format!(
        "Rules: each round a player can claim {}. A player who bets less than {}% of their money loses 3% of it per missing percentage point when the round ends. A winning bet pays amount × (options left − 1); a losing bet loses its amount.",
        ledger::CLAIM,
        ledger::TAX_THRESHOLD
    ));
    lines.push(format!(
        "Options left on the wheel ({}): {}. Each player may bet on up to {} of them.",
        current.options_left.len(),
        list(&current.options_left),
        current.max_bets()
    ));
    let winners: Vec<String> = season
        .rounds
        .iter()
        .filter_map(|r| r.winner.map(|w| format!("round {}: {}", r.number, n(w))))
        .collect();
    if !winners.is_empty() {
        lines.push(format!("Winners so far: {}.", winners.join(", ")));
    }

    let mut standings: Vec<_> = current.standings.iter().collect();
    standings.sort_by_key(|s| std::cmp::Reverse(s.money));
    if standings.is_empty() {
        lines.push("Nobody has played yet.".to_string());
    } else {
        lines.push("Players (money at the start of the round, bet this round):".to_string());
        for s in standings {
            let mut line = format!(
                "- {}: {}, bet {} ({}%)",
                n(s.user),
                s.money,
                s.bet,
                s.bet_percent
            );
            if s.tax > 0 {
                line.push_str(&format!(", taxed {}", s.tax));
            }
            if s.user == asker {
                line.push_str(" (the asker)");
            }
            lines.push(line);
        }
    }

    lines.push(format!("Claimed this round: {}.", list(&round.claims)));
    if round.bets.is_empty() {
        lines.push("No bets this round.".to_string());
    } else {
        lines.push("Bets:".to_string());
        for bet in &round.bets {
            lines.push(format!("- {} on {}: {}", n(bet.by), n(bet.on), bet.amount));
        }
    }
    let results = outcomes(round, current);
    if !results.is_empty() {
        lines.push("Outcome:".to_string());
        for o in results {
            let what = match o.kind {
                OutcomeKind::Won => "won",
                OutcomeKind::Lost => "lost",
                OutcomeKind::Taxed => "taxed",
            };
            lines.push(format!(
                "- {} {what} {} ({} → {})",
                n(o.user),
                o.amount.abs(),
                o.before,
                o.after
            ));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::wheel::ledger::{Bet, Round};

    #[test]
    fn describes_a_round() {
        let names: Names = [(1, "Alice"), (2, "Bob"), (3, "Charlie")]
            .into_iter()
            .map(|(id, name)| (id, name.to_string()))
            .collect();
        let season = Season {
            id: 1,
            options: vec![1, 2, 3],
            rounds: vec![
                Round {
                    id: 1,
                    number: 1,
                    winner: Some(2),
                    claims: vec![1, 2, 3],
                    bets: vec![Bet {
                        by: 1,
                        on: 2,
                        amount: 20,
                    }],
                },
                Round {
                    id: 2,
                    number: 2,
                    winner: None,
                    claims: vec![1],
                    bets: vec![Bet {
                        by: 1,
                        on: 3,
                        amount: 30,
                    }],
                },
            ],
        };
        let text = describe(&season, 1, &names, 1);
        assert!(text.starts_with("Movie wheel, round 2 of 2 (open for claims and bets)."));
        assert!(text.contains("Options left on the wheel (2): Alice, Charlie."));
        assert!(text.contains("Winners so far: round 1: Bob."));
        assert!(text.contains("- Alice: 240, bet 30 (12%) (the asker)"));
        assert!(text.contains("- Bob: 70, bet 0 (0%), taxed 21"));
        assert!(text.contains("- Alice on Charlie: 30"));

        let first = describe(&season, 0, &names, 1);
        assert!(first.contains("(resolved, won by Bob)"));
        assert!(first.contains("- Alice won 40 (100 → 140)"));
        assert!(first.contains("- Bob taxed 30 (100 → 70)"));
    }
}
