//! The money: who has how much in each round. Pure functions over plain values, no
//! Discord or database, so every rule is tested here.
//!
//! The rules, the same as voltgpt's:
//!
//! - Every round a player can claim 100.
//! - When a round is over, a player who bet less than 10% of their money loses 3% of it per
//!   missing percentage point (up to 30%).
//! - A winning bet pays `amount × (options − 1)`, where options are the wheel options left
//!   in that round; a losing bet loses its amount.
//! - Integer division truncates, as in Go.
//!
//! Balances are never stored. [`ledger`] folds over the rounds once and returns everything
//! the status embed, the bet checks and the chat tool need.

use std::collections::HashMap;

/// What a player gets for claiming.
pub const CLAIM: i64 = 100;
/// Players who bet less than this percentage of their money are taxed.
pub const TAX_THRESHOLD: i64 = 10;

/// A bet as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct Bet {
    pub by: u64,
    pub on: u64,
    pub amount: i64,
}

/// A round as stored.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Round {
    pub id: i64,
    /// 1, 2, 3, ... within the season.
    pub number: i64,
    pub winner: Option<u64>,
    pub claims: Vec<u64>,
    pub bets: Vec<Bet>,
}

/// A season: the wheel options and the rounds so far, oldest first.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Season {
    pub id: i64,
    pub options: Vec<u64>,
    pub rounds: Vec<Round>,
}

/// One player in one round.
#[derive(Debug, Clone, PartialEq)]
pub struct Standing {
    pub user: u64,
    /// Money at the start of the round, with this round's claim.
    pub money: i64,
    /// The total of this round's bets.
    pub bet: i64,
    /// `bet` as a percentage of `money`, truncated.
    pub bet_percent: i64,
    /// Lost to the tax when the round ends.
    pub tax: i64,
    /// Won (positive) or lost (negative) on bets when the round ends. 0 without a winner.
    pub payout: i64,
}

impl Standing {
    pub fn under_threshold(&self) -> bool {
        self.bet_percent < TAX_THRESHOLD
    }

    /// Money after the round: tax taken, bets paid out.
    pub fn after(&self) -> i64 {
        self.money - self.tax + self.payout
    }

    /// What's left to bet with in an open round.
    pub fn usable(&self) -> i64 {
        self.money - self.bet
    }
}

/// One round's numbers.
#[derive(Debug, Clone, PartialEq)]
pub struct RoundLedger {
    pub number: i64,
    /// The options that hadn't won before this round, in option order.
    pub options_left: Vec<u64>,
    /// Every player of the season, in the order they first played.
    pub standings: Vec<Standing>,
}

impl RoundLedger {
    pub fn standing(&self, user: u64) -> Option<&Standing> {
        self.standings.iter().find(|s| s.user == user)
    }

    /// Money at the start of the round; 0 for someone who never played.
    pub fn money(&self, user: u64) -> i64 {
        self.standing(user).map_or(0, |s| s.money)
    }

    /// How many options one player may bet on: half of those left, rounded up.
    pub fn max_bets(&self) -> usize {
        self.options_left.len().div_ceil(2)
    }
}

/// Everyone who claimed or bet in the season, in the order they first did.
pub fn players(season: &Season) -> Vec<u64> {
    let mut players = Vec::new();
    for round in &season.rounds {
        for user in round.claims.iter().chain(round.bets.iter().map(|b| &b.by)) {
            if !players.contains(user) {
                players.push(*user);
            }
        }
    }
    players
}

/// Every user the season mentions: options, players and winners, each once.
pub fn users(season: &Season) -> Vec<u64> {
    let mut users = season.options.clone();
    users.extend(players(season));
    users.extend(season.rounds.iter().filter_map(|r| r.winner));
    let mut seen = Vec::new();
    users.retain(|u| {
        let new = !seen.contains(u);
        seen.push(*u);
        new
    });
    users
}

/// Whether the winner of round `index` can be undone: it's the round before the latest one,
/// and nobody has bet in the latest round yet.
pub fn can_undo(season: &Season, index: usize) -> bool {
    let rounds = &season.rounds;
    index + 2 == rounds.len() && rounds[index].winner.is_some() && rounds[index + 1].bets.is_empty()
}

/// The options that hadn't won before round `index`.
fn options_left(season: &Season, index: usize) -> Vec<u64> {
    let won: Vec<u64> = season.rounds[..index]
        .iter()
        .filter_map(|r| r.winner)
        .collect();
    season
        .options
        .iter()
        .copied()
        .filter(|o| !won.contains(o))
        .collect()
}

/// The numbers of every round of the season, oldest first.
pub fn ledger(season: &Season) -> Vec<RoundLedger> {
    let players = players(season);
    let mut carried: HashMap<u64, i64> = HashMap::new();
    let mut result = Vec::new();
    for (index, round) in season.rounds.iter().enumerate() {
        let options_left = options_left(season, index);
        let options = options_left.len() as i64;
        let mut standings = Vec::new();
        for &user in &players {
            let claims = round.claims.iter().filter(|&&c| c == user).count() as i64;
            let money = carried.get(&user).copied().unwrap_or(0) + CLAIM * claims;
            let bet: i64 = round
                .bets
                .iter()
                .filter(|b| b.by == user)
                .map(|b| b.amount)
                .sum();
            let bet_percent = if money > 0 { bet * 100 / money } else { 0 };
            let tax = if bet_percent < TAX_THRESHOLD {
                money * 3 * (TAX_THRESHOLD - bet_percent) / 100
            } else {
                0
            };
            let payout = match round.winner {
                Some(winner) => round
                    .bets
                    .iter()
                    .filter(|b| b.by == user)
                    .map(|b| {
                        if b.on == winner {
                            b.amount * (options - 1).max(0)
                        } else {
                            -b.amount
                        }
                    })
                    .sum(),
                None => 0,
            };
            let standing = Standing {
                user,
                money,
                bet,
                bet_percent,
                tax,
                payout,
            };
            // Like voltgpt, every round before the latest counts as over.
            carried.insert(user, standing.after());
            standings.push(standing);
        }
        result.push(RoundLedger {
            number: round.number,
            options_left,
            standings,
        });
    }
    result
}

/// One line of a resolved round's outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub kind: OutcomeKind,
    pub user: u64,
    /// Signed: +60 for a win, -15 for a loss or tax.
    pub amount: i64,
    pub before: i64,
    pub after: i64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OutcomeKind {
    Won,
    Lost,
    Taxed,
}

/// The outcome of a resolved round: each bet won or lost, then each tax, with the balance
/// before and after each line. Empty when the round has no winner.
pub fn outcomes(round: &Round, numbers: &RoundLedger) -> Vec<Outcome> {
    let Some(winner) = round.winner else {
        return Vec::new();
    };
    let options = numbers.options_left.len() as i64;
    let mut balance: HashMap<u64, i64> = numbers
        .standings
        .iter()
        .map(|s| (s.user, s.money))
        .collect();
    let mut lines = Vec::new();
    let mut push = |kind, user, amount| {
        let before = balance.get(&user).copied().unwrap_or(0);
        let after = before + amount;
        balance.insert(user, after);
        lines.push(Outcome {
            kind,
            user,
            amount,
            before,
            after,
        });
    };
    for bet in &round.bets {
        if bet.on == winner {
            push(OutcomeKind::Won, bet.by, bet.amount * (options - 1).max(0));
        } else {
            push(OutcomeKind::Lost, bet.by, -bet.amount);
        }
    }
    for standing in numbers.standings.iter().filter(|s| s.tax > 0) {
        push(OutcomeKind::Taxed, standing.user, -standing.tax);
    }
    lines
}

/// Reads a bet amount: "50", or "25%" of `max` rounded up.
pub fn parse_amount(input: &str, max: i64) -> Option<i64> {
    let input = input.trim();
    match input.strip_suffix('%') {
        Some(percent) => {
            let percent: i64 = percent.trim().parse().ok()?;
            Some((max * percent + 99) / 100)
        }
        None => input.parse().ok(),
    }
}

/// Checks a bet in the season's latest round, which must be open. `input` is what the player
/// typed. Returns the amount, or why the bet isn't allowed (for the player).
pub fn check_bet(season: &Season, by: u64, on: u64, input: &str) -> Result<i64, String> {
    let round = match season.rounds.last() {
        Some(round) if round.winner.is_none() => round,
        _ => return Err("This round is over.".to_string()),
    };
    let rounds = ledger(season);
    let numbers = rounds.last().expect("one ledger entry per round");
    if !numbers.options_left.contains(&on) {
        return Err("That option isn't on the wheel.".to_string());
    }
    let existing = round
        .bets
        .iter()
        .find(|b| b.by == by && b.on == on)
        .map_or(0, |b| b.amount);
    let usable = numbers.standing(by).map_or(0, |s| s.usable());
    // Changing a bet frees its old amount first.
    let max = usable + existing;
    let amount = parse_amount(input, max)
        .ok_or("Type a number like 50, or a percentage like 25%.".to_string())?;

    let bets = round.bets.iter().filter(|b| b.by == by).count();
    if existing == 0 && bets >= numbers.max_bets() {
        return Err(format!(
            "You can only bet on half of the options ({}).",
            numbers.max_bets()
        ));
    }
    if amount > max {
        return Err(format!(
            "You don't have that much money. You can bet {max}."
        ));
    }
    if amount <= 0 {
        return Err("You can't bet 0 or less.".to_string());
    }
    Ok(amount)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: u64 = 1;
    const BOB: u64 = 2;
    const CHARLIE: u64 = 3;
    const DANA: u64 = 4;

    fn bet(by: u64, on: u64, amount: i64) -> Bet {
        Bet { by, on, amount }
    }

    fn round(number: i64, winner: Option<u64>, claims: &[u64], bets: Vec<Bet>) -> Round {
        Round {
            id: number,
            number,
            winner,
            claims: claims.to_vec(),
            bets,
        }
    }

    fn season(options: &[u64], rounds: Vec<Round>) -> Season {
        Season {
            id: 1,
            options: options.to_vec(),
            rounds,
        }
    }

    #[test]
    fn claims_add_up() {
        let s = season(&[], vec![round(1, None, &[ALICE], vec![])]);
        assert_eq!(ledger(&s)[0].money(ALICE), 100);
    }

    #[test]
    fn winnings_carry_over() {
        // Alice bets 50 of 100 on Bob, Bob wins with 3 options: 50 × 2 = +100.
        let s = season(
            &[ALICE, BOB, CHARLIE],
            vec![
                round(1, Some(BOB), &[ALICE], vec![bet(ALICE, BOB, 50)]),
                round(2, None, &[ALICE], vec![]),
            ],
        );
        assert_eq!(ledger(&s)[1].money(ALICE), 300);
    }

    #[test]
    fn tax_for_betting_too_little() {
        // No bet: 0% → 30% tax of 100.
        let s = season(
            &[ALICE, BOB, CHARLIE],
            vec![
                round(1, Some(BOB), &[ALICE], vec![]),
                round(2, None, &[ALICE], vec![]),
            ],
        );
        let rounds = ledger(&s);
        assert_eq!(rounds[0].standing(ALICE).unwrap().tax, 30);
        assert_eq!(rounds[1].money(ALICE), 170);
    }

    #[test]
    fn no_tax_from_ten_percent() {
        for amount in [10, 20] {
            let s = season(
                &[ALICE, BOB],
                vec![round(1, None, &[ALICE], vec![bet(ALICE, BOB, amount)])],
            );
            assert_eq!(ledger(&s)[0].standing(ALICE).unwrap().tax, 0);
        }
    }

    #[test]
    fn usable_money_leaves_out_open_bets() {
        let s = season(
            &[ALICE, BOB, CHARLIE],
            vec![round(1, None, &[ALICE], vec![bet(ALICE, BOB, 30)])],
        );
        assert_eq!(ledger(&s)[0].standing(ALICE).unwrap().usable(), 70);
    }

    #[test]
    fn winners_leave_the_wheel() {
        let s = season(
            &[ALICE, BOB, CHARLIE],
            vec![
                round(1, Some(BOB), &[], vec![]),
                round(2, None, &[], vec![]),
            ],
        );
        let rounds = ledger(&s);
        assert_eq!(rounds[0].options_left, [ALICE, BOB, CHARLIE]);
        assert_eq!(rounds[1].options_left, [ALICE, CHARLIE]);
        assert_eq!(rounds[0].max_bets(), 2);
        assert_eq!(rounds[1].max_bets(), 1);
    }

    /// voltgpt's TestStatusEmbedSortsPlayersByBankrollAndBoldsThreshold.
    #[test]
    fn same_balances_as_voltgpt() {
        let s = season(
            &[ALICE, BOB, CHARLIE],
            vec![
                round(
                    1,
                    Some(BOB),
                    &[ALICE, BOB, CHARLIE],
                    vec![bet(ALICE, BOB, 20)],
                ),
                round(2, None, &[ALICE], vec![bet(ALICE, BOB, 30)]),
            ],
        );
        let current = &ledger(&s)[1];
        assert_eq!(current.money(ALICE), 240);
        assert_eq!(current.money(BOB), 70);
        assert_eq!(current.money(CHARLIE), 70);
        assert_eq!(current.standing(ALICE).unwrap().bet_percent, 12);
        assert!(current.standing(BOB).unwrap().under_threshold());
    }

    /// voltgpt's TestResolvedOutcomeColumnsOrderByAmount and the delta column.
    #[test]
    fn outcome_lines() {
        let r = round(
            1,
            Some(BOB),
            &[ALICE, BOB, CHARLIE, DANA],
            vec![bet(ALICE, BOB, 20), bet(DANA, BOB, 10), bet(BOB, ALICE, 15)],
        );
        let s = season(&[ALICE, BOB, CHARLIE, DANA], vec![r.clone()]);
        let numbers = &ledger(&s)[0];
        let lines = outcomes(&r, numbers);
        let summary: Vec<(OutcomeKind, u64, i64, i64, i64)> = lines
            .iter()
            .map(|o| (o.kind, o.user, o.amount, o.before, o.after))
            .collect();
        assert_eq!(
            summary,
            [
                (OutcomeKind::Won, ALICE, 60, 100, 160),
                (OutcomeKind::Won, DANA, 30, 100, 130),
                (OutcomeKind::Lost, BOB, -15, 100, 85),
                (OutcomeKind::Taxed, CHARLIE, -30, 100, 70),
            ]
        );
        assert!(outcomes(&round(2, None, &[], vec![]), numbers).is_empty());
    }

    #[test]
    fn amounts() {
        assert_eq!(parse_amount("50", 200), Some(50));
        assert_eq!(parse_amount(" 25% ", 201), Some(51));
        assert_eq!(parse_amount("100%", 70), Some(70));
        assert_eq!(parse_amount("lots", 70), None);
    }

    #[test]
    fn bet_checks() {
        // 4 options: at most 2 bets each. Alice has 100.
        let open = |bets| {
            season(
                &[ALICE, BOB, CHARLIE, DANA],
                vec![round(1, None, &[ALICE], bets)],
            )
        };
        assert_eq!(check_bet(&open(vec![]), ALICE, BOB, "25%"), Ok(25));
        assert_eq!(check_bet(&open(vec![]), ALICE, BOB, "100"), Ok(100));
        assert!(check_bet(&open(vec![]), ALICE, BOB, "101").is_err());
        assert!(check_bet(&open(vec![]), ALICE, BOB, "0").is_err());
        assert!(check_bet(&open(vec![]), ALICE, BOB, "-5").is_err());
        assert!(check_bet(&open(vec![]), ALICE, BOB, "abc").is_err());
        assert!(check_bet(&open(vec![]), ALICE, 99, "10").is_err());
        // Someone who never claimed has nothing to bet.
        assert!(check_bet(&open(vec![]), BOB, ALICE, "1").is_err());

        // Changing a bet can use its old amount.
        let one = open(vec![bet(ALICE, BOB, 60)]);
        assert_eq!(check_bet(&one, ALICE, BOB, "100"), Ok(100));
        assert!(check_bet(&one, ALICE, CHARLIE, "41").is_err());
        assert_eq!(check_bet(&one, ALICE, CHARLIE, "100%"), Ok(40));

        // A third option is too many, but changing one of the two is fine.
        let two = open(vec![bet(ALICE, BOB, 10), bet(ALICE, CHARLIE, 10)]);
        assert!(check_bet(&two, ALICE, DANA, "10").is_err());
        assert_eq!(check_bet(&two, ALICE, BOB, "20"), Ok(20));

        // Winners leave the wheel, and a resolved round takes no bets.
        let later = season(
            &[ALICE, BOB, CHARLIE, DANA],
            vec![
                round(1, Some(BOB), &[ALICE], vec![bet(ALICE, CHARLIE, 10)]),
                round(2, None, &[], vec![]),
            ],
        );
        assert!(check_bet(&later, ALICE, BOB, "10").is_err());
        assert_eq!(check_bet(&later, ALICE, CHARLIE, "10"), Ok(10));
        let mut resolved = later.clone();
        resolved.rounds.pop();
        assert!(check_bet(&resolved, ALICE, CHARLIE, "10").is_err());
    }
}
