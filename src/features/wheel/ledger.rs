//! The money: who has how much in each round. Pure functions over plain values, no
//! Discord or database, so every rule is tested here.
//!
//! The rules:
//!
//! - Every round a player can claim 100.
//! - When a round is over, a player who bet less than 10% of their money loses 3% of it per
//!   missing percentage point (up to 30%).
//! - A player can bet on at most half of the options left, rounded up.
//! - Payouts depend on the season's [`Rules`]. Classic (voltgpt's): a winning bet pays
//!   `amount × (options − 1)`, where options are the wheel options left in that round.
//!   Pool: every bet and tax of the round goes into a pot, which the bets on the winner
//!   share by stake. Either way a losing bet loses its amount.
//! - Integer division truncates, as in Go.
//!
//! Balances are never stored. [`ledger`] folds over the rounds once and returns everything
//! the status picture, the bet checks and the chat tool need.

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

/// How a season pays out winning bets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rules {
    /// voltgpt's: a winning bet pays `amount × (options − 1)`, however many people picked
    /// the same option. Seasons from before pool betting keep these.
    #[default]
    Classic,
    /// Every bet and tax of the round goes into a pot, and the bets on the winner share it
    /// by stake: favourites pay little, long shots a lot. A pot nobody won carries over to
    /// the next round.
    Pool,
}

impl Rules {
    /// The name stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Rules::Classic => "classic",
            Rules::Pool => "pool",
        }
    }

    pub fn parse(text: &str) -> Option<Rules> {
        match text {
            "classic" => Some(Rules::Classic),
            "pool" => Some(Rules::Pool),
            _ => None,
        }
    }
}

/// A season: the wheel options and the rounds so far, oldest first.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Season {
    pub id: i64,
    /// Which season of the server this is, counting from 1.
    pub number: i64,
    pub rules: Rules,
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

    /// The smallest total bet that avoids the tax: [`TAX_THRESHOLD`]% of the money, rounded
    /// up.
    pub fn safe_bet(&self) -> i64 {
        (self.money * TAX_THRESHOLD + 99) / 100
    }
}

/// One round's numbers.
#[derive(Debug, Clone, PartialEq)]
pub struct RoundLedger {
    pub number: i64,
    pub rules: Rules,
    /// The options that hadn't won before this round, in option order.
    pub options_left: Vec<u64>,
    /// Every player of the season, in the order they first played.
    pub standings: Vec<Standing>,
    /// The total bet on each option this round.
    pub totals: HashMap<u64, i64>,
    /// Pool rules: what earlier rounds left in the pot because nobody won it.
    pub carried: i64,
    /// Pool rules: the pot if the round ended now: `carried`, this round's bets and the
    /// taxes. 0 under classic rules.
    pub pot: i64,
}

impl RoundLedger {
    /// The total bet on `option` this round.
    pub fn total_on(&self, option: u64) -> i64 {
        self.totals.get(&option).copied().unwrap_or(0)
    }

    /// What a bet wins (positive) or loses (negative) if `winner` wins.
    pub fn result(&self, bet: &Bet, winner: u64) -> i64 {
        if bet.on != winner {
            return -bet.amount;
        }
        match self.rules {
            Rules::Classic => bet.amount * (self.options_left.len() as i64 - 1).max(0),
            Rules::Pool => self.pot * bet.amount / self.total_on(winner) - bet.amount,
        }
    }

    /// Pool rules: what a bet on `option` gets back per 1 bet if the round ended now, its
    /// stake included. `None` under classic rules or when nobody bet on it yet.
    pub fn odds(&self, option: u64) -> Option<f64> {
        let total = self.total_on(option);
        (self.rules == Rules::Pool && total > 0).then(|| self.pot as f64 / total as f64)
    }

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
    // Pool rules: what is left in the pot for the next round.
    let mut carried_pot = 0;
    let mut result = Vec::new();
    for (index, round) in season.rounds.iter().enumerate() {
        let mut totals: HashMap<u64, i64> = HashMap::new();
        for bet in &round.bets {
            *totals.entry(bet.on).or_default() += bet.amount;
        }
        let mut numbers = RoundLedger {
            number: round.number,
            rules: season.rules,
            options_left: options_left(season, index),
            standings: Vec::new(),
            totals,
            carried: 0,
            pot: 0,
        };

        // Money, bets and tax first: under pool rules the payouts depend on everyone's tax.
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
            numbers.standings.push(Standing {
                user,
                money,
                bet,
                bet_percent,
                tax,
                payout: 0,
            });
        }
        if season.rules == Rules::Pool {
            let bets: i64 = numbers.standings.iter().map(|s| s.bet).sum();
            let taxes: i64 = numbers.standings.iter().map(|s| s.tax).sum();
            numbers.carried = carried_pot;
            numbers.pot = carried_pot + bets + taxes;
        }

        if let Some(winner) = round.winner {
            let payouts: Vec<i64> = numbers
                .standings
                .iter()
                .map(|s| {
                    let mine = round.bets.iter().filter(|b| b.by == s.user);
                    mine.map(|b| numbers.result(b, winner)).sum()
                })
                .collect();
            for (standing, payout) in numbers.standings.iter_mut().zip(payouts) {
                standing.payout = payout;
            }
        }
        if season.rules == Rules::Pool {
            carried_pot = match round.winner {
                // Nobody on the winner, or rounding: the rest stays in the pot.
                Some(winner) => {
                    let on_winner = round.bets.iter().filter(|b| b.on == winner);
                    numbers.pot
                        - on_winner
                            .map(|b| numbers.result(b, winner) + b.amount)
                            .sum::<i64>()
                }
                // A past round without a winner (only in voltgpt's games): the bets stay
                // with their owners, the taxes in the pot.
                None => numbers.pot - numbers.standings.iter().map(|s| s.bet).sum::<i64>(),
            };
        }

        // Like voltgpt, every round before the latest counts as over.
        for standing in &numbers.standings {
            carried.insert(standing.user, standing.after());
        }
        result.push(numbers);
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
        let kind = if bet.on == winner {
            OutcomeKind::Won
        } else {
            OutcomeKind::Lost
        };
        push(kind, bet.by, numbers.result(bet, winner));
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
            number: 1,
            rules: Default::default(),
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
    fn safe_bet_rounds_up() {
        let standing = |money| Standing {
            user: ALICE,
            money,
            bet: 0,
            bet_percent: 0,
            tax: 0,
            payout: 0,
        };
        for (money, safe) in [(100, 10), (340, 34), (101, 11), (0, 0)] {
            assert_eq!(standing(money).safe_bet(), safe, "{money}");
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

    fn pool(options: &[u64], rounds: Vec<Round>) -> Season {
        Season {
            rules: Rules::Pool,
            ..season(options, rounds)
        }
    }

    #[test]
    fn pool_split_by_stake() {
        // Bets 45 and Charlie's tax 30 make a pot of 75. Alice and Dana bet 20 and 10 on
        // Bob, so they get 50 and 25 of it back.
        let r = round(
            1,
            Some(BOB),
            &[ALICE, BOB, CHARLIE, DANA],
            vec![bet(ALICE, BOB, 20), bet(DANA, BOB, 10), bet(BOB, ALICE, 15)],
        );
        let s = pool(&[ALICE, BOB, CHARLIE, DANA], vec![r.clone()]);
        let numbers = &ledger(&s)[0];
        assert_eq!(numbers.pot, 75);
        assert_eq!(numbers.odds(BOB), Some(2.5));
        assert_eq!(numbers.odds(CHARLIE), None);
        let after: Vec<i64> = [ALICE, BOB, CHARLIE, DANA]
            .iter()
            .map(|&u| numbers.standing(u).unwrap().after())
            .collect();
        assert_eq!(after, [130, 85, 70, 115]);
        let won: Vec<(u64, i64)> = outcomes(&r, numbers)
            .iter()
            .filter(|o| o.kind == OutcomeKind::Won)
            .map(|o| (o.user, o.amount))
            .collect();
        assert_eq!(won, [(ALICE, 30), (DANA, 15)]);
    }

    #[test]
    fn pool_nobody_won_carries_over() {
        // Nobody bet on Charlie, so round 2 starts with round 1's pot of 75.
        let s = pool(
            &[ALICE, BOB, CHARLIE, DANA],
            vec![
                round(
                    1,
                    Some(CHARLIE),
                    &[ALICE, BOB, CHARLIE, DANA],
                    vec![bet(ALICE, BOB, 20), bet(DANA, BOB, 10), bet(BOB, ALICE, 15)],
                ),
                round(2, Some(BOB), &[ALICE], vec![bet(ALICE, BOB, 18)]),
            ],
        );
        let rounds = ledger(&s);
        assert_eq!(rounds[1].carried, 75);
        assert_eq!(rounds[1].money(ALICE), 180);
        // 75 carried, Alice's 18, and the others' taxes: 25 + 27 + 21.
        assert_eq!(rounds[1].pot, 166);
        assert_eq!(rounds[1].standing(ALICE).unwrap().payout, 148);
        // Money only moves between players and the pot.
        let total = |n: &RoundLedger| n.standings.iter().map(|s| s.after()).sum::<i64>();
        assert_eq!(total(&rounds[0]) + 75, 400);
        assert_eq!(total(&rounds[1]), 400 + 100);
    }

    #[test]
    fn pool_rounding_stays_in_the_pot() {
        // A pot of 61 shared 10 : 20 is 20.33 and 40.67, so 1 is left for round 2.
        let s = pool(
            &[ALICE, BOB, CHARLIE],
            vec![
                round(
                    1,
                    Some(BOB),
                    &[ALICE, BOB, CHARLIE],
                    vec![
                        bet(ALICE, BOB, 10),
                        bet(BOB, BOB, 20),
                        bet(CHARLIE, ALICE, 31),
                    ],
                ),
                round(2, None, &[], vec![]),
            ],
        );
        let rounds = ledger(&s);
        assert_eq!(rounds[0].standing(ALICE).unwrap().payout, 10);
        assert_eq!(rounds[0].standing(BOB).unwrap().payout, 20);
        assert_eq!(rounds[1].carried, 1);
    }

    #[test]
    fn rules_names() {
        for rules in [Rules::Classic, Rules::Pool] {
            assert_eq!(Rules::parse(rules.as_str()), Some(rules));
        }
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
