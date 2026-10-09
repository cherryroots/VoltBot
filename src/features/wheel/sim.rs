//! Simulated games, to compare the payout rules. Run with
//! `cargo test --release wheel::sim -- --ignored --nocapture`.

use super::ledger::{Bet, Round, Rules, Season, check_bet, ledger};

const PLAYERS: u64 = 12;
const GAMES: usize = 5000;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Style {
    /// Everything on one random option.
    AllIn,
    /// The 10% minimum on one random option.
    Safe,
    /// 60% split over as many options as allowed.
    Spread,
    /// 30% on the option with the most money on it.
    Favourite,
    /// 20% on the option with the least money on it.
    LongShot,
    /// 10–50% on one random option.
    Casual,
}

const STYLES: [Style; 6] = [
    Style::AllIn,
    Style::Safe,
    Style::Spread,
    Style::Favourite,
    Style::LongShot,
    Style::Casual,
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, p: f64) -> bool {
        (self.next() % 10_000) as f64 / 10_000.0 < p
    }
}

fn style(player: u64) -> Style {
    STYLES[(player as usize - 1) % STYLES.len()]
}

/// Plays one game with a uniformly random winner each round.
fn play(rules: Rules, rng: &mut Rng) -> Season {
    let options: Vec<u64> = (1..=PLAYERS).collect();
    let mut season = Season {
        id: 1,
        number: 1,
        rules,
        options: options.clone(),
        rounds: Vec::new(),
    };
    let mut left = options.clone();
    for number in 1..PLAYERS as i64 {
        season.rounds.push(Round {
            id: number,
            number,
            ..Round::default()
        });
        let index = season.rounds.len() - 1;
        let mut order: Vec<u64> = options.clone();
        for i in (1..order.len()).rev() {
            order.swap(i, rng.below(i + 1));
        }
        for &player in &order {
            // Most people show up most weeks.
            if rng.chance(0.85) {
                season.rounds[index].claims.push(player);
            } else {
                continue;
            }
            let numbers = ledger(&season);
            let current = numbers.last().unwrap();
            let usable = current.standing(player).map_or(0, |s| s.usable());
            let money = current.money(player);
            let total = |o: u64| current.total_on(o);
            let random = |rng: &mut Rng| left[rng.below(left.len())];
            let picks: Vec<(u64, i64)> = match style(player) {
                Style::AllIn => vec![(random(rng), usable)],
                Style::Safe => vec![(random(rng), (money * 10 + 99) / 100)],
                Style::Spread => {
                    let n = current.max_bets();
                    let mut chosen: Vec<u64> = Vec::new();
                    while chosen.len() < n {
                        let o = random(rng);
                        if !chosen.contains(&o) {
                            chosen.push(o);
                        }
                    }
                    let each = (money * 60 / 100 / n as i64).max(1);
                    chosen.into_iter().map(|o| (o, each)).collect()
                }
                Style::Favourite => {
                    let best = *left.iter().max_by_key(|&&o| (total(o), o)).unwrap();
                    let pick = if total(best) == 0 { random(rng) } else { best };
                    vec![(pick, money * 30 / 100)]
                }
                Style::LongShot => {
                    let least = left.iter().map(|&o| total(o)).min().unwrap();
                    let cands: Vec<u64> = left
                        .iter()
                        .copied()
                        .filter(|&o| total(o) == least)
                        .collect();
                    vec![(cands[rng.below(cands.len())], money * 20 / 100)]
                }
                Style::Casual => {
                    let pct = 10 + rng.below(41) as i64;
                    vec![(random(rng), (money * pct + 99) / 100)]
                }
            };
            for (on, amount) in picks {
                if amount <= 0 {
                    continue;
                }
                if let Ok(amount) = check_bet(&season, player, on, &amount.to_string()) {
                    season.rounds[index]
                        .bets
                        .retain(|b| !(b.by == player && b.on == on));
                    season.rounds[index].bets.push(Bet {
                        by: player,
                        on,
                        amount,
                    });
                }
            }
        }
        // Spin the wheel.
        let winner = left[rng.below(left.len())];
        season.rounds[index].winner = Some(winner);
        left.retain(|&o| o != winner);
    }
    season
}

struct Stats {
    finals: Vec<Vec<i64>>,
    /// For each game, the round where the eventual leader gained most.
    leader_best_round: Vec<i64>,
    /// Each game's richest-to-median ratio.
    spread: Vec<f64>,
    totals: Vec<i64>,
}

fn run(rules: Rules) -> Stats {
    let mut rng = Rng(0x9E3779B97F4A7C15);
    let mut stats = Stats {
        finals: vec![Vec::new(); PLAYERS as usize],
        leader_best_round: Vec::new(),
        spread: Vec::new(),
        totals: Vec::new(),
    };
    for _ in 0..GAMES {
        let season = play(rules, &mut rng);
        let numbers = ledger(&season);
        let last = numbers.last().unwrap();
        let mut finals: Vec<(u64, i64)> = (1..=PLAYERS)
            .map(|p| (p, last.standing(p).map_or(0, |s| s.after())))
            .collect();
        for &(p, m) in &finals {
            stats.finals[p as usize - 1].push(m);
        }
        stats.totals.push(finals.iter().map(|f| f.1).sum());
        finals.sort_by_key(|f| std::cmp::Reverse(f.1));
        let leader = finals[0].0;
        let best = numbers
            .iter()
            .max_by_key(|n| n.standing(leader).map_or(0, |s| s.after() - s.money))
            .unwrap()
            .number;
        stats.leader_best_round.push(best);
        let median = (finals[5].1 + finals[6].1) as f64 / 2.0;
        stats.spread.push(finals[0].1 as f64 / median.max(1.0));
    }
    stats
}

fn median(mut v: Vec<i64>) -> i64 {
    v.sort();
    v[v.len() / 2]
}

fn report(name: &str, stats: &Stats) {
    println!("\n=== {name} ({GAMES} games, {PLAYERS} players, 11 rounds) ===");
    println!(
        "{:<10} {:>8} {:>8} {:>8} {:>8}",
        "style", "mean", "median", "broke%", "1st%"
    );
    // Who finished first in each game.
    let mut firsts = vec![0usize; PLAYERS as usize];
    for g in 0..GAMES {
        let best = (0..PLAYERS as usize)
            .max_by_key(|&p| (stats.finals[p][g], std::cmp::Reverse(p)))
            .unwrap();
        firsts[best] += 1;
    }
    for (i, s) in STYLES.iter().enumerate() {
        let players: Vec<usize> = (0..PLAYERS as usize)
            .filter(|p| p % STYLES.len() == i)
            .collect();
        let all: Vec<i64> = players
            .iter()
            .flat_map(|&p| stats.finals[p].clone())
            .collect();
        let mean = all.iter().sum::<i64>() as f64 / all.len() as f64;
        let broke = all.iter().filter(|&&m| m <= 20).count() as f64 / all.len() as f64;
        let first: usize = players.iter().map(|&p| firsts[p]).sum();
        println!(
            "{:<10} {:>8.0} {:>8} {:>7.1}% {:>7.1}%",
            format!("{s:?}"),
            mean,
            median(all.clone()),
            broke * 100.0,
            first as f64 / GAMES as f64 * 100.0
        );
    }
    let total = stats.totals.iter().sum::<i64>() as f64 / GAMES as f64;
    println!("money in the game at the end: {total:.0} on average");
    let mut spread = stats.spread.clone();
    spread.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "richest / median player: median ×{:.1}, 90th percentile ×{:.1}",
        spread[GAMES / 2],
        spread[GAMES * 9 / 10]
    );
    let mut buckets = [0; 3];
    for &r in &stats.leader_best_round {
        buckets[match r {
            1..=4 => 0,
            5..=8 => 1,
            _ => 2,
        }] += 1;
    }
    println!(
        "the winner's biggest gain came in rounds 1-4: {:.0}%, 5-8: {:.0}%, 9-11: {:.0}%",
        buckets[0] as f64 / GAMES as f64 * 100.0,
        buckets[1] as f64 / GAMES as f64 * 100.0,
        buckets[2] as f64 / GAMES as f64 * 100.0
    );
}

#[test]
#[ignore]
fn compare_rules() {
    report("classic", &run(Rules::Classic));
    report("pool", &run(Rules::Pool));
}
