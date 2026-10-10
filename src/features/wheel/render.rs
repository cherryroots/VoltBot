//! The round as a picture. [`round_svg`] lays the round out as SVG text (pure, tested like
//! any other string), and [`round_png`] turns that into a PNG with the shared drawing
//! helpers in `util::svg`, which also explain the fonts.

use super::ledger::{BET_CAP, OutcomeKind, Rules, Standing, TAX_THRESHOLD, outcomes};
use super::ui::View;
use crate::util::svg::{
    ACCENT, AMBER, BG, DIM, GREEN, LINE, MUTED, RED, ROW, Style, Svg, TEXT, columns, markup,
    text_width, to_png,
};

/// Width of the picture in SVG units.
const WIDTH: f32 = 600.0;
/// Names longer than this are cut short.
const MAX_NAME: usize = 14;

/// The round as a PNG. Call it off the async threads (see [`to_png`]).
pub fn round_png(view: &View) -> anyhow::Result<Vec<u8>> {
    to_png(|| round_svg(view))
}

/// The round as SVG: standings and bets while it's open, the payouts once it has a winner.
pub fn round_svg(view: &View) -> String {
    let mut svg = Svg::new(WIDTH);
    if view.season.rounds[view.index].winner.is_some() {
        resolved(&mut svg, view);
    } else {
        open(&mut svg, view);
    }
    svg.finish()
}

/// The open round: who has what, the bets, and who still has to play.
fn open(svg: &mut Svg, view: &View) {
    let round = &view.season.rounds[view.index];
    let numbers = &view.ledger[view.index];
    let left = numbers.options_left.len();
    let (state, color) = if view.active && view.is_latest() {
        ("OPEN", GREEN)
    } else {
        ("ENDED", DIM)
    };
    let payouts = match numbers.rules {
        Rules::Classic => format!("winning bets pay ×{}", left.saturating_sub(1)),
        Rules::Pool if numbers.carried > 0 => {
            format!("pot {} ({} carried over)", numbers.pot, numbers.carried)
        }
        Rules::Pool => format!("pot {}", numbers.pot),
    };
    let sub = format!(
        "{left} options left  ·  bet on up to {}  ·  {payouts}",
        numbers.max_bets()
    );
    header(svg, view, state, color, &sub);

    svg.section("Standings");
    let standings = sorted_by_money(view, &numbers.standings);
    if standings.is_empty() {
        svg.note("Nobody has played yet. Press Claim! to get started.");
    } else {
        let (bar, money, bet, percent, note) = (170.0, 330.0, 400.0, 460.0, WIDTH - 24.0);
        svg.y += 6.0;
        for (x, label) in [
            (money, "Money"),
            (bet, "Bet"),
            (percent, "Bet %"),
            (note, "If it ended now"),
        ] {
            svg.text(
                x,
                svg.y + 8.0,
                label,
                Style::new(11.0, DIM).weight(500).end(),
            );
        }
        svg.y += 16.0;
        let richest = standings.iter().map(|s| s.money).max().unwrap_or(0).max(1);
        for (n, s) in standings.iter().enumerate() {
            let y = svg.y;
            svg.stripe(n, 30.0);
            let rank = (n + 1).to_string();
            svg.text(
                36.0,
                y + 20.0,
                &rank,
                Style::new(12.0, DIM).weight(600).middle(),
            );
            svg.text(
                52.0,
                y + 20.0,
                &short(view.name(s.user)),
                Style::new(14.0, TEXT).weight(600),
            );
            svg.rect([bar, y + 11.0, 110.0, 8.0], LINE, 4.0, "");
            if s.money > 0 {
                let width = (110.0 * s.money as f32 / richest as f32).max(8.0);
                svg.rect([bar, y + 11.0, width, 8.0], ACCENT, 4.0, "");
            }
            let row = |x: f32, text: &str, style: Style| (x, text.to_string(), style);
            let bet_text = if s.bet > 0 {
                s.bet.to_string()
            } else {
                "–".to_string()
            };
            let (percent_color, percent_weight) = if s.under_threshold() {
                (AMBER, 700)
            } else {
                (GREEN, 500)
            };
            let (note_text, note_color) = if s.tax > 0 {
                (format!("taxed −{}", s.tax), AMBER)
            } else if s.under_threshold() {
                ("–".to_string(), DIM)
            } else {
                ("safe".to_string(), DIM)
            };
            for (x, text, style) in [
                row(
                    money,
                    &s.money.to_string(),
                    Style::new(14.0, TEXT).weight(600).end(),
                ),
                row(bet, &bet_text, Style::new(14.0, MUTED).end()),
                row(
                    percent,
                    &format!("{}%", s.bet_percent),
                    Style::new(14.0, percent_color).weight(percent_weight).end(),
                ),
                row(
                    note,
                    &note_text,
                    Style::new(13.0, note_color).weight(600).end(),
                ),
            ] {
                svg.text(x, y + 20.0, &text, style);
            }
            svg.y += 30.0;
        }
        svg.y += 18.0;
        svg.text(
            24.0,
            svg.y,
            &match numbers.rules {
                Rules::Classic => format!(
                    "Bet at least {TAX_THRESHOLD}% of your money each round, or lose 3% of it per missing point."
                ),
                Rules::Pool => format!(
                    "Bet {TAX_THRESHOLD}–{BET_CAP}% of your money each round. Under {TAX_THRESHOLD}%, 3% per missing point goes to the pot."
                ),
            },
            Style::new(11.0, DIM),
        );
        svg.y += 16.0;
    }

    svg.section("Bets this round");
    if numbers.options_left.is_empty() {
        svg.note("Nobody is on the wheel yet. Admins add options with /wheel_add.");
    } else {
        svg.y += 4.0;
        let total = |option: u64| numbers.total_on(option);
        let mut options = numbers.options_left.clone();
        options.sort_by(|a, b| {
            total(*b)
                .cmp(&total(*a))
                .then_with(|| view.name(*a).cmp(view.name(*b)))
        });
        for option in options {
            let pieces: Vec<String> = round
                .bets
                .iter()
                .filter(|b| b.on == option)
                .map(|b| {
                    format!(
                        "{} <tspan fill=\"{TEXT}\" font-weight=\"600\">{}</tspan>",
                        markup(&short(view.name(b.by))),
                        b.amount
                    )
                })
                .collect();
            let plain: Vec<String> = round
                .bets
                .iter()
                .filter(|b| b.on == option)
                .map(|b| format!("{} {}", short(view.name(b.by)), b.amount))
                .collect();
            let lines = wrap(&pieces, &plain, WIDTH - 32.0 - 300.0, 13.0);
            let height = 30.0 + 20.0 * (lines.len().max(1) - 1) as f32;
            let y = svg.y;
            if pieces.is_empty() {
                let dashed = format!("stroke=\"{LINE}\" stroke-dasharray=\"3 3\"");
                svg.rect([16.0, y, WIDTH - 32.0, height], BG, 6.0, &dashed);
                svg.text(
                    32.0,
                    y + 20.0,
                    &short(view.name(option)),
                    Style::new(14.0, DIM).weight(600),
                );
                let empty = match numbers.rules {
                    Rules::Classic => "no bets",
                    Rules::Pool => "no bets: a bet here takes the whole pot",
                };
                svg.text(WIDTH - 32.0, y + 20.0, empty, Style::new(13.0, DIM).end());
            } else {
                svg.rect([16.0, y, WIDTH - 32.0, height], ROW, 6.0, "");
                svg.text(
                    32.0,
                    y + 20.0,
                    &short(view.name(option)),
                    Style::new(14.0, TEXT).weight(600),
                );
                svg.text(
                    170.0,
                    y + 20.0,
                    &match numbers.odds(option) {
                        Some(odds) => format!("{} on it  ·  ×{odds:.1}", total(option)),
                        None => format!("{} on it", total(option)),
                    },
                    Style::new(13.0, MUTED).weight(500),
                );
                for (n, line) in lines.iter().enumerate() {
                    svg.raw_text(
                        WIDTH - 32.0,
                        y + 20.0 + 20.0 * n as f32,
                        line,
                        Style::new(13.0, MUTED).end(),
                    );
                }
            }
            svg.y += height + 4.0;
        }
    }

    // Who still has to play, while the round is open.
    if view.active && view.is_latest() {
        let players: Vec<u64> = numbers.standings.iter().map(|s| s.user).collect();
        let mut not_claimed: Vec<&str> = players
            .iter()
            .filter(|p| !round.claims.contains(p))
            .map(|&p| view.name(p))
            .collect();
        let mut no_bet: Vec<&str> = players
            .iter()
            .filter(|p| round.claims.contains(p) && !round.bets.iter().any(|b| b.by == **p))
            .map(|&p| view.name(p))
            .collect();
        not_claimed.sort();
        no_bet.sort();
        let mut groups = Vec::new();
        if !not_claimed.is_empty() {
            groups.push(("Haven't claimed", not_claimed));
        }
        if !no_bet.is_empty() {
            groups.push(("Claimed, no bet yet", no_bet));
        }
        if !groups.is_empty() {
            svg.section("Waiting on");
            svg.y += 4.0;
            let mut lines = Vec::new();
            for (label, names) in groups {
                let names: Vec<String> = names.iter().map(|n| short(n)).collect();
                let pieces: Vec<String> = names.iter().map(|n| markup(n)).collect();
                for (n, line) in wrap(&pieces, &names, WIDTH - 32.0 - 180.0, 13.0)
                    .into_iter()
                    .enumerate()
                {
                    lines.push((if n == 0 { label } else { "" }, line));
                }
            }
            let height = 12.0 + 22.0 * lines.len() as f32;
            svg.rect(
                [16.0, svg.y, WIDTH - 32.0, height],
                AMBER,
                8.0,
                "fill-opacity=\"0.10\"",
            );
            svg.rect([16.0, svg.y, 4.0, height], AMBER, 2.0, "");
            for (n, (label, line)) in lines.iter().enumerate() {
                let y = svg.y + 22.0 + 22.0 * n as f32;
                svg.text(32.0, y, label, Style::new(13.0, AMBER).weight(600));
                svg.raw_text(180.0, y, line, Style::new(13.0, TEXT).weight(500));
            }
            svg.y += height;
        }
    }

    let won: Vec<String> = view.season.rounds[..view.index]
        .iter()
        .filter_map(|r| {
            r.winner
                .map(|w| format!("round {} {}", r.number, short(view.name(w))))
        })
        .collect();
    if !won.is_empty() {
        svg.y += 26.0;
        svg.text(
            24.0,
            svg.y,
            &format!("Already won: {}", won.join(" · ")),
            Style::new(12.0, DIM).weight(500),
        );
    }
    svg.y += 20.0;
}

/// A round with a winner: the winner and what everyone won or lost.
fn resolved(svg: &mut Svg, view: &View) {
    let round = &view.season.rounds[view.index];
    let numbers = &view.ledger[view.index];
    let winner = round.winner.expect("resolved rounds have a winner");
    let payouts = match numbers.rules {
        Rules::Classic => format!(
            "winning bets paid ×{}",
            numbers.options_left.len().saturating_sub(1)
        ),
        Rules::Pool if numbers.total_on(winner) == 0 => {
            format!("nobody won the pot of {}, it carries over", numbers.pot)
        }
        Rules::Pool => format!("pot {}", numbers.pot),
    };
    let sub = format!(
        "{} claims  ·  {} bets  ·  {payouts}",
        round.claims.len(),
        round.bets.len()
    );
    header(svg, view, "RESOLVED", RED, &sub);

    svg.y += 4.0;
    svg.rect(
        [16.0, svg.y, WIDTH - 32.0, 56.0],
        GREEN,
        10.0,
        "fill-opacity=\"0.14\"",
    );
    svg.text(
        32.0,
        svg.y + 24.0,
        "WINNER",
        Style::new(11.0, GREEN).weight(700).spaced(),
    );
    svg.text(
        32.0,
        svg.y + 46.0,
        &short(view.name(winner)),
        Style::new(22.0, TEXT).weight(700),
    );
    if view.active && view.index + 2 == view.season.rounds.len() {
        let next = format!("round {} is open", round.number + 1);
        svg.text(
            WIDTH - 32.0,
            svg.y + 35.0,
            &next,
            Style::new(13.0, MUTED).weight(500).end(),
        );
    }
    svg.y += 70.0;

    svg.section("Results");
    let (name, before, bets, tax, after, change) = (40.0, 250.0, 330.0, 400.0, 480.0, WIDTH - 28.0);
    svg.y += 6.0;
    for (x, label) in [
        (before, "Before"),
        (bets, "Bets"),
        (tax, "Tax"),
        (after, "After"),
        (change, "Change"),
    ] {
        svg.text(
            x,
            svg.y + 8.0,
            label,
            Style::new(11.0, DIM).weight(500).end(),
        );
    }
    svg.y += 16.0;

    // Biggest gain first; ties by name.
    let mut rows: Vec<&Standing> = numbers.standings.iter().collect();
    rows.sort_by(|a, b| {
        (b.after() - b.money)
            .cmp(&(a.after() - a.money))
            .then_with(|| view.name(a.user).cmp(view.name(b.user)))
    });
    let picked: Vec<u64> = outcomes(round, numbers)
        .iter()
        .filter(|o| o.kind == OutcomeKind::Won)
        .map(|o| o.user)
        .collect();
    for (n, s) in rows.iter().enumerate() {
        let y = svg.y;
        svg.stripe(n, 30.0);
        let shown = short(view.name(s.user));
        svg.text(name, y + 20.0, &shown, Style::new(14.0, TEXT).weight(600));
        if picked.contains(&s.user) {
            let x = name + text_width(&shown, 14.0) + 10.0;
            svg.text(x, y + 20.0, "★", Style::new(13.0, GREEN).weight(700));
        }
        let delta = s.after() - s.money;
        svg.text(
            before,
            y + 20.0,
            &s.money.to_string(),
            Style::new(14.0, MUTED).weight(500).end(),
        );
        svg.text(
            bets,
            y + 20.0,
            &signed(s.payout),
            Style::new(14.0, sign_color(s.payout, GREEN))
                .weight(600)
                .end(),
        );
        let tax_color = if s.tax > 0 { AMBER } else { DIM };
        svg.text(
            tax,
            y + 20.0,
            &signed(-s.tax),
            Style::new(14.0, tax_color).weight(600).end(),
        );
        svg.text(
            after,
            y + 20.0,
            &s.after().to_string(),
            Style::new(14.0, TEXT).weight(700).end(),
        );
        svg.text(
            change,
            y + 20.0,
            &signed(delta),
            Style::new(14.0, sign_color(delta, GREEN)).weight(700).end(),
        );
        svg.y += 30.0;
    }
    if !picked.is_empty() {
        svg.raw_text(
            24.0,
            svg.y + 24.0,
            &format!("<tspan fill=\"{GREEN}\">★</tspan> bet on the winner"),
            Style::new(12.0, DIM).weight(500),
        );
        svg.y += 16.0;
    }
    svg.y += 24.0;
}

/// "Round 4  OPEN" with the season on the right and a summary line under it.
fn header(svg: &mut Svg, view: &View, state: &str, color: &'static str, sub: &str) {
    let title = format!("Round {}", view.season.rounds[view.index].number);
    svg.text(24.0, 44.0, &title, Style::new(26.0, TEXT).weight(700));
    let x = 24.0 + text_width(&title, 26.0) + 14.0;
    let width = 9.0 + state.len() as f32 * 7.4;
    svg.rect([x, 26.0, width, 20.0], color, 10.0, "fill-opacity=\"0.18\"");
    svg.text(
        x + width / 2.0,
        41.0,
        state,
        Style::new(11.0, color).weight(700).middle().spaced(),
    );
    let season = format!("Season {}", view.season.number);
    svg.text(
        WIDTH - 24.0,
        30.0,
        &season,
        Style::new(12.0, DIM).weight(500).end(),
    );
    svg.text(
        WIDTH - 24.0,
        46.0,
        "Movie wheel",
        Style::new(12.0, DIM).weight(500).end(),
    );
    svg.text(24.0, 70.0, sub, Style::new(13.0, MUTED));
    svg.y = 90.0;
}

fn sorted_by_money<'a>(view: &View, standings: &'a [Standing]) -> Vec<&'a Standing> {
    let mut list: Vec<&Standing> = standings.iter().collect();
    list.sort_by(|a, b| {
        b.money
            .cmp(&a.money)
            .then_with(|| view.name(a.user).cmp(view.name(b.user)))
    });
    list
}

/// Joins `pieces` with " · " into lines that fit `width`. `plain` is the same text without
/// markup, for measuring.
fn wrap(pieces: &[String], plain: &[String], width: f32, size: f32) -> Vec<String> {
    let mut lines: Vec<(String, String)> = Vec::new();
    for (piece, text) in pieces.iter().zip(plain) {
        match lines.last_mut() {
            Some((line, measured))
                if text_width(&format!("{measured} · {text}"), size) <= width =>
            {
                line.push_str(" · ");
                line.push_str(piece);
                measured.push_str(" · ");
                measured.push_str(text);
            }
            _ => lines.push((piece.clone(), text.clone())),
        }
    }
    lines.into_iter().map(|(line, _)| line).collect()
}

/// The name cut to [`MAX_NAME`] letters' width.
fn short(name: &str) -> String {
    if name.chars().map(columns).sum::<usize>() <= MAX_NAME {
        return name.to_string();
    }
    let mut cut = String::new();
    let mut width = 0;
    for c in name.chars() {
        width += columns(c);
        if width > MAX_NAME - 1 {
            break;
        }
        cut.push(c);
    }
    format!("{cut}…")
}

/// "+60", "−15" (a real minus sign), or "–" for nothing.
fn signed(n: i64) -> String {
    match n {
        0 => "–".to_string(),
        n if n > 0 => format!("+{n}"),
        n => format!("−{}", -n),
    }
}

fn sign_color(n: i64, positive: &'static str) -> &'static str {
    match n {
        0 => DIM,
        n if n > 0 => positive,
        _ => RED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::wheel::ledger::{Bet, Round, Season, ledger};
    use crate::features::wheel::ui::Names;

    fn names() -> Names {
        [(1, "Alice"), (2, "Bob"), (3, "Charlie"), (4, "Dana <&>")]
            .into_iter()
            .map(|(id, name)| (id, name.to_string()))
            .collect()
    }

    fn bet(by: u64, on: u64, amount: i64) -> Bet {
        Bet { by, on, amount }
    }

    /// Round 1: Alice bets 20 on Bob and wins 60; Charlie is taxed 30. In round 2 Bob hasn't
    /// claimed and Charlie hasn't bet.
    fn season() -> Season {
        Season {
            id: 1,
            number: 2,
            rules: Default::default(),
            options: vec![1, 2, 3, 4],
            rounds: vec![
                Round {
                    id: 1,
                    number: 1,
                    winner: Some(2),
                    claims: vec![1, 2, 3],
                    bets: vec![bet(1, 2, 20)],
                },
                Round {
                    id: 2,
                    number: 2,
                    winner: None,
                    claims: vec![1, 3],
                    bets: vec![bet(1, 4, 30)],
                },
            ],
        }
    }

    fn render(season: &Season, index: usize) -> String {
        let numbers = ledger(season);
        let names = names();
        let view = View {
            season,
            ledger: &numbers,
            index,
            active: true,
            names: &names,
        };
        round_svg(&view)
    }

    #[test]
    fn open_round() {
        let svg = render(&season(), 1);
        assert!(svg.contains(">Round 2<"));
        assert!(svg.contains(">OPEN<"));
        assert!(svg.contains(">Season 2<"));
        assert!(svg.contains("3 options left  ·  bet on up to 2  ·  winning bets pay ×2"));
        // Standings, richest first, with the tax if the round ended now.
        let alice = svg.find(">Alice<").unwrap();
        let bob = svg.find(">Bob<").unwrap();
        assert!(alice < bob);
        assert!(svg.contains(">260<") && svg.contains(">11%<"));
        assert!(svg.contains(">taxed −21<"));
        // Names are escaped.
        assert!(svg.contains("Dana &lt;&amp;&gt;"));
        assert!(svg.contains(">30 on it<"));
        // Bob hasn't claimed; Charlie claimed but hasn't bet.
        assert!(svg.contains(">Haven't claimed<"));
        assert!(svg.contains(">Claimed, no bet yet<"));
        assert!(svg.contains("Already won: round 1 Bob"));
    }

    #[test]
    fn resolved_round() {
        let svg = render(&season(), 0);
        assert!(svg.contains(">RESOLVED<"));
        assert!(svg.contains(">WINNER<"));
        assert!(svg.contains(">round 2 is open<"));
        // Alice bet 20 on Bob with 4 options: +60. Charlie paid 30 tax.
        assert!(svg.contains(">+60<"));
        assert!(svg.contains(">−30<"));
        assert!(svg.contains("bet on the winner"));
        assert!(!svg.contains("Waiting on"));
    }

    #[test]
    fn empty_round() {
        let season = Season {
            id: 1,
            number: 1,
            rules: Default::default(),
            options: vec![],
            rounds: vec![Round {
                id: 1,
                number: 1,
                ..Round::default()
            }],
        };
        let svg = render(&season, 0);
        assert!(svg.contains("Nobody has played yet"));
        assert!(svg.contains("Nobody is on the wheel yet"));
    }

    /// Names resvg can't lay out in a `<tspan>` of their own, and names that need other fonts.
    #[test]
    fn unusual_names() {
        let mut names = names();
        names.insert(1, "Eve 🎬👍🏽 👨‍👩‍👧 🇳🇱 1️⃣".into());
        names.insert(2, "𝗓𝗈𝖾 ✨ ᰁ 𐰁 ａｂ é".into());
        let season = season();
        let numbers = ledger(&season);
        let view = View {
            season: &season,
            ledger: &numbers,
            index: 1,
            active: true,
            names: &names,
        };
        assert!(round_png(&view).unwrap().starts_with(b"\x89PNG"));
    }

    #[test]
    fn wrapping_and_names() {
        let pieces: Vec<String> = ["Alice 10", "Bob 20", "Charlie 30"]
            .map(String::from)
            .into();
        assert_eq!(
            wrap(&pieces, &pieces, 1000.0, 13.0),
            ["Alice 10 · Bob 20 · Charlie 30"]
        );
        assert_eq!(
            wrap(&pieces, &pieces, 150.0, 13.0),
            ["Alice 10 · Bob 20", "Charlie 30"]
        );
        assert_eq!(short("Bartholomew the Third"), "Bartholomew t…");
        assert_eq!(short(&"🎬".repeat(8)), format!("{}…", "🎬".repeat(6)));
        assert_eq!(signed(5), "+5");
        assert_eq!(signed(-5), "−5");
        assert_eq!(signed(0), "–");
    }
}
