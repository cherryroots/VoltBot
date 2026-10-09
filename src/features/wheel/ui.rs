//! What the wheel looks like in Discord: the status embed and its buttons, the menus, the
//! bet modal, and the button IDs. Everything here takes plain values and builds messages;
//! nothing talks to Discord or the database.

use std::collections::HashMap;

use serenity::all::{
    ButtonStyle, CreateActionRow, CreateButton, CreateEmbed, CreateEmbedFooter, CreateInputText,
    CreateModal, CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption, InputTextStyle,
};

use super::ledger::{CLAIM, Outcome, OutcomeKind, RoundLedger, Season, can_undo, outcomes};
use crate::util::shorten;

/// Display names by user ID, looked up before rendering.
pub type Names = HashMap<u64, String>;

/// What a select menu is for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PickKind {
    /// Pick an option to bet on; the amount modal follows.
    Place,
    /// Pick one of your bets to remove.
    Remove,
    /// Pick the winner (admins).
    Winner,
}

impl PickKind {
    fn as_str(self) -> &'static str {
        match self {
            PickKind::Place => "place",
            PickKind::Remove => "remove",
            PickKind::Winner => "winner",
        }
    }

    fn parse(text: &str) -> Option<PickKind> {
        match text {
            "place" => Some(PickKind::Place),
            "remove" => Some(PickKind::Remove),
            "winner" => Some(PickKind::Winner),
            _ => None,
        }
    }
}

/// Every button, menu and modal this feature makes. The ID text is built and parsed only
/// here. `round` is a round's database ID; `message` is the status message to update.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Show the server's current round on this status message.
    Current,
    Claim {
        round: i64,
    },
    /// Opens the menu of options to bet on.
    Bet {
        round: i64,
    },
    /// Opens the menu of your bets to remove.
    Unbet {
        round: i64,
    },
    /// Opens the menu to pick the winner (admins).
    Winner {
        round: i64,
    },
    /// Clears the winner of a resolved round (admins).
    Undo {
        round: i64,
    },
    /// A menu from [`Action::Bet`], [`Action::Unbet`] or [`Action::Winner`]. The picked user
    /// is the selected value.
    Pick {
        kind: PickKind,
        round: i64,
        message: u64,
    },
    /// The bet amount modal.
    Amount {
        round: i64,
        on: u64,
        message: u64,
    },
    /// The confirm button of `/reset_wheel`.
    Reset {
        keep_options: bool,
    },
}

impl Action {
    pub fn custom_id(&self) -> String {
        match self {
            Action::Current => "wheel:current".to_string(),
            Action::Claim { round } => format!("wheel:claim:{round}"),
            Action::Bet { round } => format!("wheel:bet:{round}"),
            Action::Unbet { round } => format!("wheel:unbet:{round}"),
            Action::Winner { round } => format!("wheel:winner:{round}"),
            Action::Undo { round } => format!("wheel:undo:{round}"),
            Action::Pick {
                kind,
                round,
                message,
            } => format!("wheel:pick:{}:{round}:{message}", kind.as_str()),
            Action::Amount { round, on, message } => {
                format!("wheel:amount:{round}:{on}:{message}")
            }
            Action::Reset { keep_options } => format!("wheel:reset:{}", u8::from(*keep_options)),
        }
    }

    /// Parses what the dispatcher passes on: the ID without the `wheel:` prefix.
    pub fn parse(action: &str) -> Option<Action> {
        let parts: Vec<&str> = action.split(':').collect();
        Some(match parts.as_slice() {
            ["current"] => Action::Current,
            ["claim", round] => Action::Claim {
                round: round.parse().ok()?,
            },
            ["bet", round] => Action::Bet {
                round: round.parse().ok()?,
            },
            ["unbet", round] => Action::Unbet {
                round: round.parse().ok()?,
            },
            ["winner", round] => Action::Winner {
                round: round.parse().ok()?,
            },
            ["undo", round] => Action::Undo {
                round: round.parse().ok()?,
            },
            ["pick", kind, round, message] => Action::Pick {
                kind: PickKind::parse(kind)?,
                round: round.parse().ok()?,
                message: message.parse().ok()?,
            },
            ["amount", round, on, message] => Action::Amount {
                round: round.parse().ok()?,
                on: on.parse().ok()?,
                message: message.parse().ok()?,
            },
            ["reset", "0"] => Action::Reset {
                keep_options: false,
            },
            ["reset", "1"] => Action::Reset { keep_options: true },
            _ => return None,
        })
    }
}

/// One round of a season, ready to show.
pub struct View<'a> {
    pub season: &'a Season,
    /// [`super::ledger::ledger`] of the season.
    pub ledger: &'a [RoundLedger],
    /// Which round: an index into `season.rounds`.
    pub index: usize,
    /// Whether the season is being played. Past seasons get no buttons.
    pub active: bool,
    pub names: &'a Names,
}

impl View<'_> {
    fn is_latest(&self) -> bool {
        self.index + 1 == self.season.rounds.len()
    }

    /// Whether to offer "Undo Winner". See [`can_undo`].
    pub fn undoable(&self) -> bool {
        self.active && can_undo(self.season, self.index)
    }

    fn name(&self, user: u64) -> &str {
        name(self.names, user)
    }
}

pub fn name(names: &Names, user: u64) -> &str {
    names.get(&user).map_or("Unknown", String::as_str)
}

/// The status embed of one round, like voltgpt's.
pub fn status_embed(view: &View) -> CreateEmbed {
    let round = &view.season.rounds[view.index];
    let numbers = &view.ledger[view.index];
    let resolved = round.winner.is_some();

    let mut title = format!("Round {}", round.number);
    if !view.active {
        title.push_str(" (past season)");
    }
    let state = match round.winner {
        Some(winner) => format!("State: Resolved\nWinner: ||<@{winner}>||"),
        None => "State: Open\nWinner: _Not set_".to_string(),
    };

    // Richest first; ties by name.
    let mut standings: Vec<_> = numbers.standings.iter().collect();
    standings.sort_by(|a, b| {
        b.money
            .cmp(&a.money)
            .then_with(|| view.name(a.user).cmp(view.name(b.user)))
    });
    let players = lines(standings.iter().map(|s| view.name(s.user).to_string()));
    let money = lines(standings.iter().map(|s| s.money.to_string()));
    let percents = lines(standings.iter().map(|s| {
        if s.under_threshold() {
            format!("**{}%**", s.bet_percent)
        } else {
            format!("{}%", s.bet_percent)
        }
    }));

    let mut claimers: Vec<&str> = round.claims.iter().map(|&u| view.name(u)).collect();
    claimers.sort();
    let claims = lines(claimers.chunks(4).map(|chunk| chunk.join(", ")));

    let by = lines(round.bets.iter().map(|b| view.name(b.by).to_string()));
    let on = lines(round.bets.iter().map(|b| view.name(b.on).to_string()));
    let amounts = lines(round.bets.iter().map(|b| b.amount.to_string()));

    let mut embed = CreateEmbed::new()
        .title(title)
        .color(if resolved { 0xff0000 } else { 0x00ff00 })
        .field("✨ Round Status ✨", state, false)
        .field("Players", or(players, "_No players yet_"), true)
        .field("Money", or(money, "_No balances yet_"), true)
        .field("Bet%", or(percents, "_No bets yet_"), true)
        .field(
            format!("Claims ({CLAIM})"),
            or(claims, "_No claims yet_"),
            false,
        )
        .field("✨ Round bets ✨", "\u{200b}", false)
        .field("By", or(by, "_No bets yet_"), true)
        .field("On", or(on, "_No bets yet_"), true)
        .field("Amount", or(amounts, "_No bets yet_"), true);

    let outcomes = sorted_outcomes(view, outcomes(round, numbers));
    if resolved {
        let label = |o: &Outcome| match o.kind {
            OutcomeKind::Won => "Won",
            OutcomeKind::Lost => "Lost",
            OutcomeKind::Taxed => "Taxed",
        };
        let what = lines(
            outcomes
                .iter()
                .map(|o| format!("{}: {}", label(o), view.name(o.user))),
        );
        let payout = lines(outcomes.iter().map(|o| signed(o.amount)));
        let delta = lines(
            outcomes
                .iter()
                .map(|o| format!("{} → {}", o.before, o.after)),
        );
        embed = embed
            .field("Outcome", or(what, "_No outcomes yet_"), true)
            .field("Payout", or(payout, "_No outcomes yet_"), true)
            .field("Delta", or(delta, "_No outcomes yet_"), true);
    } else {
        embed = embed.field("Outcome", "_No outcomes yet_", true).field(
            "Amount",
            "_No outcomes yet_",
            true,
        );
    }

    let mut footer = vec![
        format!("{} claims", round.claims.len()),
        format!("{} bets", round.bets.len()),
    ];
    if resolved {
        let count = |kind| outcomes.iter().filter(|o| o.kind == kind).count();
        footer.push(format!("{} wins", count(OutcomeKind::Won)));
        footer.push(format!("{} losses", count(OutcomeKind::Lost)));
    }
    let taxed = numbers.standings.iter().filter(|s| s.tax > 0).count();
    footer.push(format!("{taxed} taxed"));
    embed.footer(CreateEmbedFooter::new(footer.join(" • ")))
}

/// Biggest gain first; ties by name.
fn sorted_outcomes(view: &View, mut list: Vec<Outcome>) -> Vec<Outcome> {
    list.sort_by(|a, b| {
        b.amount
            .cmp(&a.amount)
            .then_with(|| view.name(a.user).cmp(view.name(b.user)))
    });
    list
}

/// The buttons under a status message.
pub fn status_buttons(view: &View) -> Vec<CreateActionRow> {
    if !view.active {
        return Vec::new();
    }
    let round = view.season.rounds[view.index].id;
    let button = |action: Action, label: &str, emoji: char, style| {
        CreateButton::new(action.custom_id())
            .label(label)
            .emoji(emoji)
            .style(style)
    };
    let buttons = if view.is_latest() {
        vec![
            button(
                Action::Claim { round },
                "Claim!",
                '📈',
                ButtonStyle::Primary,
            ),
            button(
                Action::Bet { round },
                "Place Bet!",
                '💸',
                ButtonStyle::Secondary,
            ),
            button(
                Action::Unbet { round },
                "Remove Bet!",
                '💰',
                ButtonStyle::Secondary,
            ),
            button(
                Action::Winner { round },
                "Set Winner!",
                '✨',
                ButtonStyle::Success,
            ),
        ]
    } else {
        let mut buttons = vec![button(
            Action::Current,
            "View Current Round",
            '🎯',
            ButtonStyle::Primary,
        )];
        if view.undoable() {
            buttons.push(button(
                Action::Undo { round },
                "Undo Winner",
                '⏪',
                ButtonStyle::Danger,
            ));
        }
        buttons
    };
    vec![CreateActionRow::Buttons(buttons)]
}

/// The menu after Place Bet, Remove Bet or Set Winner. `users` are the choices; Discord
/// shows at most 25.
pub fn pick_menu(
    kind: PickKind,
    round: i64,
    message: u64,
    users: &[u64],
    names: &Names,
) -> (String, Vec<CreateActionRow>) {
    let (content, placeholder) = match kind {
        PickKind::Place => ("Place a Bet", "Select an option to bet on"),
        PickKind::Remove => ("Remove a Bet", "Select a bet to remove"),
        PickKind::Winner => ("Pick a Winner", "Select the winner"),
    };
    let mut sorted: Vec<u64> = users.to_vec();
    sorted.sort_by(|a, b| name(names, *a).cmp(name(names, *b)));
    let options = sorted
        .iter()
        .take(25)
        .map(|&user| CreateSelectMenuOption::new(shorten(name(names, user), 100), user.to_string()))
        .collect();
    let action = Action::Pick {
        kind,
        round,
        message,
    };
    let menu = CreateSelectMenu::new(action.custom_id(), CreateSelectMenuKind::String { options })
        .placeholder(placeholder);
    (content.to_string(), vec![CreateActionRow::SelectMenu(menu)])
}

/// The modal asking how much to bet. `existing` is the player's current bet on this option.
pub fn amount_modal(
    round: i64,
    on: u64,
    message: u64,
    on_name: &str,
    usable: i64,
    existing: i64,
) -> CreateModal {
    // Discord allows 45 characters in a label.
    let label = shorten(&format!("Amount (you can bet {})", usable + existing), 45);
    let mut input = CreateInputText::new(InputTextStyle::Short, label, "amount")
        .placeholder("A number like 50, or a percentage like 25%")
        .required(true);
    if existing > 0 {
        input = input.value(existing.to_string());
    }
    let title = shorten(&format!("Bet on {on_name}"), 45);
    CreateModal::new(Action::Amount { round, on, message }.custom_id(), title)
        .components(vec![CreateActionRow::InputText(input)])
}

/// The `/reset_wheel` confirmation.
pub fn reset_confirmation(keep_options: bool) -> (String, Vec<CreateActionRow>) {
    let kept = if keep_options {
        " with the same options"
    } else {
        " with an empty wheel"
    };
    let text = format!(
        "This ends the current season and starts a new one{kept}. The old season stays viewable with `/wheel_status season:`."
    );
    let button = CreateButton::new(Action::Reset { keep_options }.custom_id())
        .label("Start a new season")
        .style(ButtonStyle::Danger);
    (text, vec![CreateActionRow::Buttons(vec![button])])
}

/// "+60", "-15", "0".
fn signed(amount: i64) -> String {
    if amount > 0 {
        format!("+{amount}")
    } else {
        amount.to_string()
    }
}

fn lines(items: impl Iterator<Item = String>) -> String {
    items.collect::<Vec<_>>().join("\n")
}

/// `value`, or `empty` when there's nothing to show. Discord allows 1024 characters.
fn or(value: String, empty: &str) -> String {
    if value.trim().is_empty() {
        empty.to_string()
    } else {
        shorten(&value, 1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::wheel::ledger::{Bet, Round, ledger};

    fn names() -> Names {
        [(1, "Alice"), (2, "Bob"), (3, "Charlie"), (4, "Dana")]
            .into_iter()
            .map(|(id, name)| (id, name.to_string()))
            .collect()
    }

    fn field(embed: &serde_json::Value, name: &str) -> String {
        embed["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == name)
            .unwrap_or_else(|| panic!("no field {name}"))["value"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn render(season: &Season, index: usize) -> serde_json::Value {
        let numbers = ledger(season);
        let names = names();
        let view = View {
            season,
            ledger: &numbers,
            index,
            active: true,
            names: &names,
        };
        serde_json::to_value(status_embed(&view)).unwrap()
    }

    fn bet(by: u64, on: u64, amount: i64) -> Bet {
        Bet { by, on, amount }
    }

    /// voltgpt's TestStatusEmbedSortsPlayersByBankrollAndBoldsThreshold.
    #[test]
    fn players_sorted_by_money_and_threshold_bold() {
        let season = Season {
            id: 1,
            options: vec![1, 2, 3],
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
                    claims: vec![1],
                    bets: vec![bet(1, 2, 30)],
                },
            ],
        };
        let embed = render(&season, 1);
        assert_eq!(embed["title"], "Round 2");
        assert_eq!(field(&embed, "Players"), "Alice\nBob\nCharlie");
        assert_eq!(field(&embed, "Money"), "240\n70\n70");
        assert_eq!(field(&embed, "Bet%"), "12%\n**0%**\n**0%**");
        assert_eq!(field(&embed, "Claims (100)"), "Alice");
        assert_eq!(embed["footer"]["text"], "1 claims • 1 bets • 2 taxed");
    }

    /// voltgpt's TestResolvedOutcomeColumnsOrderByAmount.
    #[test]
    fn resolved_round_lists_outcomes_by_amount() {
        let season = Season {
            id: 1,
            options: vec![1, 2, 3, 4],
            rounds: vec![Round {
                id: 1,
                number: 1,
                winner: Some(2),
                claims: vec![1, 2, 3, 4],
                bets: vec![bet(2, 1, 15), bet(4, 2, 10), bet(1, 2, 20)],
            }],
        };
        let embed = render(&season, 0);
        assert_eq!(
            field(&embed, "Outcome"),
            "Won: Alice\nWon: Dana\nLost: Bob\nTaxed: Charlie"
        );
        assert_eq!(field(&embed, "Payout"), "+60\n+30\n-15\n-30");
        assert_eq!(
            field(&embed, "Delta"),
            "100 → 160\n100 → 130\n100 → 85\n100 → 70"
        );
        assert_eq!(field(&embed, "Claims (100)"), "Alice, Bob, Charlie, Dana");
        assert_eq!(
            embed["footer"]["text"],
            "4 claims • 3 bets • 2 wins • 1 losses • 1 taxed"
        );
        assert!(field(&embed, "✨ Round Status ✨").contains("||<@2>||"));
    }

    #[test]
    fn claims_wrap_after_four() {
        let season = Season {
            id: 1,
            options: vec![],
            rounds: vec![Round {
                id: 1,
                number: 1,
                claims: vec![5, 4, 3, 2, 1],
                ..Round::default()
            }],
        };
        let mut names = names();
        names.insert(5, "Eve".to_string());
        let numbers = ledger(&season);
        let view = View {
            season: &season,
            ledger: &numbers,
            index: 0,
            active: true,
            names: &names,
        };
        let embed = serde_json::to_value(status_embed(&view)).unwrap();
        assert_eq!(
            field(&embed, "Claims (100)"),
            "Alice, Bob, Charlie, Dana\nEve"
        );
    }

    #[test]
    fn undo_only_while_the_next_round_has_no_bets() {
        let mut season = Season {
            id: 1,
            options: vec![1, 2],
            rounds: vec![
                Round {
                    id: 1,
                    number: 1,
                    winner: Some(2),
                    ..Round::default()
                },
                Round {
                    id: 2,
                    number: 2,
                    claims: vec![1],
                    ..Round::default()
                },
            ],
        };
        let labels = |season: &Season, index, active| {
            let names = names();
            let numbers = ledger(season);
            let view = View {
                season,
                ledger: &numbers,
                index,
                active,
                names: &names,
            };
            let rows = serde_json::to_value(status_buttons(&view)).unwrap();
            rows.as_array()
                .unwrap()
                .iter()
                .flat_map(|row| row["components"].as_array().unwrap().clone())
                .map(|b| b["label"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            labels(&season, 0, true),
            ["View Current Round", "Undo Winner"]
        );
        assert_eq!(
            labels(&season, 1, true),
            ["Claim!", "Place Bet!", "Remove Bet!", "Set Winner!"]
        );
        assert!(labels(&season, 0, false).is_empty());
        season.rounds[1].bets.push(bet(1, 1, 10));
        assert_eq!(labels(&season, 0, true), ["View Current Round"]);
    }

    #[test]
    fn action_round_trip() {
        for action in [
            Action::Current,
            Action::Claim { round: 3 },
            Action::Bet { round: 3 },
            Action::Unbet { round: 3 },
            Action::Winner { round: 3 },
            Action::Undo { round: 3 },
            Action::Pick {
                kind: PickKind::Remove,
                round: 3,
                message: 99,
            },
            Action::Amount {
                round: 3,
                on: 7,
                message: 99,
            },
            Action::Reset { keep_options: true },
            Action::Reset {
                keep_options: false,
            },
        ] {
            let id = action.custom_id();
            assert!(id.len() <= 100, "{id}");
            let rest = id.strip_prefix("wheel:").unwrap();
            assert_eq!(Action::parse(rest), Some(action));
        }
        assert_eq!(Action::parse("pick:steal:1:2"), None);
        assert_eq!(Action::parse("claim"), None);
    }
}
