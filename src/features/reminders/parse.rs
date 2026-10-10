//! The reminder time grammar, written with the `winnow` parser-combinator crate.
//!
//! Everything here is pure: text goes in, a time comes out. Nothing touches Discord or the
//! database, so the tests at the bottom run without either.
//!
//! The grammar is built from small parsers that each understand one thing (`duration`,
//! `clock`, `day`, `zone`) and are combined into `when`. Each small parser is a plain
//! function `fn(&mut &str) -> ModalResult<T>`: it reads from the front of the input, moves
//! the input forward past what it read, and returns what it found.

use chrono::{
    DateTime, Datelike, Days, Months, NaiveDate, NaiveTime, TimeDelta, TimeZone, Utc, Weekday,
};
use chrono_tz::Tz;
use winnow::ModalResult;
use winnow::ascii::{Caseless, dec_uint, space0, space1};
use winnow::combinator::{alt, cut_err, not, opt, preceded, repeat, terminated};
use winnow::error::{ContextError, ErrMode, StrContext, StrContextValue};
use winnow::prelude::*;
use winnow::token::{literal, one_of, take_while};

/// A time expression as the user wrote it. [`resolve`] turns it into an actual instant.
#[derive(Debug, Clone, PartialEq)]
pub enum When {
    /// "in 2h30m", "in 1 week and 2 days"
    In(Vec<(u32, Unit)>),
    /// "at 16:30 CET", "tomorrow at 3pm", "next friday", "on 2026-12-24 at noon"
    At {
        day: Option<Day>,
        time: Option<NaiveTime>,
        zone: Option<Tz>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Unit {
    Year,
    Month,
    Week,
    Day,
    Hour,
    Minute,
    Second,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Day {
    Today,
    Tomorrow,
    /// `next: true` for "next friday", which never means today.
    Weekday {
        day: Weekday,
        next: bool,
    },
    Date(NaiveDate),
}

/// A reminder message split into its time and its text.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub when: When,
    pub message: String,
}

/// Splits "in 2h check the oven" or "check the oven in 2h" into a time and a message.
///
/// The time is looked for at the start first, then at the end. On failure the error is a
/// sentence that can be shown to the user as is.
pub fn parse_reminder(text: &str) -> Result<Parsed, String> {
    let text = text.trim();

    // 1. The time comes first: "in 2h check the oven".
    let mut input = text;
    let leading_error = match when.parse_next(&mut input) {
        Ok(when) => return Ok(day_then_time(when, input)),
        Err(err) => (text.len() - input.len(), err),
    };

    // 2. The time comes last: "check the oven in 2h".
    if let Some((start, when)) = time_at_end(text) {
        return Ok(Parsed {
            when,
            message: clean_message(&text[..start]),
        });
    }

    // 3. Neither worked. If the start looked like a time ("in ...", "at ..."), say where it
    //    went wrong; otherwise there was no time at all.
    let (offset, err) = leading_error;
    match err {
        ErrMode::Cut(err) => {
            let word = text[offset..]
                .split_whitespace()
                .next()
                .unwrap_or("the end");
            Err(format!(
                "I got stuck at \"{word}\": I expected {}.",
                expected(&err)
            ))
        }
        _ => Err("I couldn't find a time in that.".to_string()),
    }
}

/// The time was at the start and `rest` is the message after it. For "tomorrow call mom at
/// 5pm" the start only gave a day, so the time at the end of `rest` is used with it.
fn day_then_time(when: When, rest: &str) -> Parsed {
    if let When::At {
        day: Some(day),
        time: None,
        ..
    } = when
        && let Some((
            start,
            When::At {
                day: None,
                time,
                zone,
            },
        )) = time_at_end(rest)
    {
        return Parsed {
            when: When::At {
                day: Some(day),
                time,
                zone,
            },
            message: clean_message(&rest[..start]),
        };
    }
    Parsed {
        when,
        message: clean_message(rest),
    }
}

/// Finds a time at the end of `text`: tries every word as the start of the time, from the
/// left, and takes the first one that reads all the way to the end. Returns where the time
/// starts and the time.
fn time_at_end(text: &str) -> Option<(usize, When)> {
    let word_starts = text
        .char_indices()
        .filter(|(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8());
    for start in word_starts {
        let mut tail = &text[start..];
        if let Ok(when) = when.parse_next(&mut tail)
            && tail.trim_end_matches(['.', '!', '?', ' ']).is_empty()
        {
            return Some((start, when));
        }
    }
    None
}

/// Reads a time on its own, like "in 2h" or "tomorrow at 9am". Used by the chat tool, where
/// the model passes the time and the message separately.
pub fn parse_when(text: &str) -> Result<When, String> {
    let mut input = text.trim();
    match when.parse_next(&mut input) {
        Ok(when) if input.trim_end_matches(['.', '!', '?', ' ']).is_empty() => Ok(when),
        Ok(_) => Err(format!(
            "I didn't understand \"{}\" after the time.",
            input.trim()
        )),
        Err(ErrMode::Cut(err)) => Err(format!("I expected {}.", expected(&err))),
        Err(_) => Err("That isn't a time I understand.".to_string()),
    }
}

/// Turns a parsed time into an instant. `now` is the current time and `home` is the
/// user's timezone, used when the text doesn't name one.
pub fn resolve(when: &When, now: DateTime<Utc>, home: Tz) -> Result<DateTime<Utc>, String> {
    let too_far = || "That's too far in the future.".to_string();
    match when {
        When::In(parts) => {
            let mut t = now;
            for &(n, unit) in parts {
                t = add(t, n, unit, home).ok_or_else(too_far)?;
            }
            Ok(t)
        }
        When::At { day, time, zone } => {
            let zone = zone.unwrap_or(home);
            let today = now.with_timezone(&zone).date_naive();
            let time = time.unwrap_or(NINE_AM);
            let date = match day {
                // "at 15:00": today, or tomorrow if 15:00 has already passed.
                None => {
                    let t = to_utc(zone, today, time)?;
                    if t > now {
                        return Ok(t);
                    }
                    today.succ_opt().ok_or_else(too_far)?
                }
                Some(Day::Today) => today,
                Some(Day::Tomorrow) => today.succ_opt().ok_or_else(too_far)?,
                Some(Day::Weekday { day, next }) => {
                    let ahead = (7 + day.num_days_from_monday()
                        - today.weekday().num_days_from_monday())
                        % 7;
                    let date = today + Days::new(ahead.into());
                    // "friday" on a Friday means today if the time is still ahead.
                    // "next friday" on a Friday means a week from now.
                    if ahead == 0 && (*next || to_utc(zone, date, time)? <= now) {
                        date + Days::new(7)
                    } else {
                        date
                    }
                }
                Some(Day::Date(date)) => *date,
            };
            let t = to_utc(zone, date, time)?;
            if t <= now {
                return Err(format!("<t:{}:f> has already passed.", t.timestamp()));
            }
            Ok(t)
        }
    }
}

/// Looks up a timezone by IANA name ("Europe/Oslo") or a common abbreviation ("CET").
///
/// Abbreviations map to a real place, so summer time is handled: "CET" in July is UTC+2.
/// Single words that aren't abbreviations are rejected on purpose, because tz names like
/// "Turkey" or "Japan" are also ordinary words in a reminder.
pub fn lookup_zone(name: &str) -> Option<Tz> {
    let zone = match name.to_ascii_uppercase().as_str() {
        "UTC" | "GMT" | "Z" => Tz::UTC,
        "EST" | "EDT" | "ET" => Tz::America__New_York,
        "CST" | "CDT" | "CT" => Tz::America__Chicago,
        "MST" | "MDT" | "MT" => Tz::America__Denver,
        "PST" | "PDT" | "PT" => Tz::America__Los_Angeles,
        "CET" | "CEST" => Tz::Europe__Paris,
        "BST" => Tz::Europe__London,
        "JST" => Tz::Asia__Tokyo,
        "AEST" | "AEDT" => Tz::Australia__Sydney,
        _ if name.contains('/') => return Tz::from_str_insensitive(name).ok(),
        _ => return None,
    };
    Some(zone)
}

// ---------------------------------------------------------------------------------------
// The grammar
// ---------------------------------------------------------------------------------------

const NINE_AM: NaiveTime = NaiveTime::from_hms_opt(9, 0, 0).unwrap();

/// The whole time expression.
fn when(input: &mut &str) -> ModalResult<When> {
    alt((in_duration, at_time, on_day)).parse_next(input)
}

/// "in 2h30m", "in 1 week and 2 days"
fn in_duration(input: &mut &str) -> ModalResult<When> {
    (word("in"), space1).parse_next(input)?;
    // After "in" there must be a duration. `cut_err` stops `alt` from trying the other
    // forms, so the error points at the word that isn't a duration.
    let parts = cut_err(duration)
        .context(describe("a duration like 2h30m or 3 days"))
        .parse_next(input)?;
    Ok(When::In(parts))
}

/// "at 16:30", "at 3pm CET", "at 2026-03-01 15:04 EST", "at 9:00 tomorrow"
fn at_time(input: &mut &str) -> ModalResult<When> {
    (word("at"), space1).parse_next(input)?;
    let (date, time) = cut_err(alt((
        (date, space1, opt((word("at"), space1)), clock).map(|(d, _, _, t)| (Some(d), t)),
        clock.map(|t| (None, t)),
    )))
    .context(describe("a time like 16:30, 3pm or noon"))
    .parse_next(input)?;
    let zone = opt(preceded(space1, zone)).parse_next(input)?;
    let day = match date {
        Some(date) => Some(Day::Date(date)),
        None => opt(preceded(space1, day)).parse_next(input)?,
    };
    Ok(When::At {
        day,
        time: Some(time),
        zone,
    })
}

/// "tomorrow", "friday at 3pm", "next friday 9:30 CET", "on 2026-12-24 at noon"
fn on_day(input: &mut &str) -> ModalResult<When> {
    let day = day.parse_next(input)?;
    let time = opt(preceded((space1, opt((word("at"), space1))), clock)).parse_next(input)?;
    let zone = match time {
        Some(_) => opt(preceded(space1, zone)).parse_next(input)?,
        None => None,
    };
    Ok(When::At {
        day: Some(day),
        time,
        zone,
    })
}

/// One or more "<number> <unit>" parts, optionally separated by spaces, commas or "and".
fn duration(input: &mut &str) -> ModalResult<Vec<(u32, Unit)>> {
    let separator = (space0, opt((alt((",", word("and"))), space0)));
    repeat(1.., preceded(separator, duration_part)).parse_next(input)
}

/// "2h", "30 minutes"
fn duration_part(input: &mut &str) -> ModalResult<(u32, Unit)> {
    let n: u32 = dec_uint.parse_next(input)?;
    space0.parse_next(input)?;
    let unit = unit.parse_next(input)?;
    Ok((n, unit))
}

fn unit(input: &mut &str) -> ModalResult<Unit> {
    // Longer spellings come first, so "mins" isn't read as "m" followed by "ins".
    alt((
        any_word(&["years", "year", "yrs", "yr", "y"]).value(Unit::Year),
        any_word(&["months", "month", "mos", "mo"]).value(Unit::Month),
        any_word(&["weeks", "week", "wks", "wk", "w"]).value(Unit::Week),
        any_word(&["days", "day", "d"]).value(Unit::Day),
        any_word(&["hours", "hour", "hrs", "hr", "h"]).value(Unit::Hour),
        any_word(&["minutes", "minute", "mins", "min", "m"]).value(Unit::Minute),
        any_word(&["seconds", "second", "secs", "sec", "s"]).value(Unit::Second),
    ))
    .parse_next(input)
}

/// "16:30", "16:30:15", "3pm", "3:30 pm", "noon", "midnight"
fn clock(input: &mut &str) -> ModalResult<NaiveTime> {
    alt((
        word("noon").value(NaiveTime::from_hms_opt(12, 0, 0).unwrap()),
        word("midnight").value(NaiveTime::MIN),
        numeric_clock,
    ))
    .parse_next(input)
}

fn numeric_clock(input: &mut &str) -> ModalResult<NaiveTime> {
    let hour: u32 = dec_uint.parse_next(input)?;
    let minute = opt(preceded(':', two_digits)).parse_next(input)?;
    let second = match minute {
        Some(_) => opt(preceded(':', two_digits)).parse_next(input)?,
        None => None,
    };
    let pm = opt(preceded(space0, any_word(&["am", "pm"])))
        .map(|m| m.map(|m: &str| m.eq_ignore_ascii_case("pm")))
        .parse_next(input)?;

    // A bare number like "at 5" is too vague: it needs minutes or am/pm.
    if minute.is_none() && pm.is_none() {
        return backtrack();
    }
    let hour = match pm {
        None => hour,
        Some(_) if !(1..=12).contains(&hour) => return backtrack(),
        Some(true) => hour % 12 + 12,
        Some(false) => hour % 12,
    };
    match NaiveTime::from_hms_opt(hour, minute.unwrap_or(0), second.unwrap_or(0)) {
        Some(time) => Ok(time),
        None => backtrack(),
    }
}

/// "today", "tomorrow", "friday", "next friday", "on friday", "2026-12-24", "on 2026-12-24"
fn day(input: &mut &str) -> ModalResult<Day> {
    alt((
        word("today").value(Day::Today),
        word("tomorrow").value(Day::Tomorrow),
        preceded((word("next"), space1), weekday).map(|day| Day::Weekday { day, next: true }),
        preceded(
            (word("on"), space1),
            alt((date.map(Day::Date), plain_weekday)),
        ),
        date.map(Day::Date),
        plain_weekday,
    ))
    .parse_next(input)
}

fn plain_weekday(input: &mut &str) -> ModalResult<Day> {
    weekday
        .map(|day| Day::Weekday { day, next: false })
        .parse_next(input)
}

fn weekday(input: &mut &str) -> ModalResult<Weekday> {
    let name = any_word(&[
        "monday",
        "mon",
        "tuesday",
        "tues",
        "tue",
        "wednesday",
        "wed",
        "thursday",
        "thurs",
        "thur",
        "thu",
        "friday",
        "fri",
        "saturday",
        "sat",
        "sunday",
        "sun",
    ])
    .parse_next(input)?;
    // The first three letters are enough to tell the days apart.
    let day = match name[..3].to_ascii_lowercase().as_str() {
        "mon" => Weekday::Mon,
        "tue" => Weekday::Tue,
        "wed" => Weekday::Wed,
        "thu" => Weekday::Thu,
        "fri" => Weekday::Fri,
        "sat" => Weekday::Sat,
        _ => Weekday::Sun,
    };
    Ok(day)
}

/// "2026-12-24"
fn date(input: &mut &str) -> ModalResult<NaiveDate> {
    let year = take_while(4, |c: char| c.is_ascii_digit())
        .parse_to()
        .parse_next(input)?;
    let month = preceded('-', two_digits).parse_next(input)?;
    let day = preceded('-', two_digits).parse_next(input)?;
    match NaiveDate::from_ymd_opt(year, month, day) {
        Some(date) => Ok(date),
        None => backtrack(),
    }
}

/// "CET", "Europe/Oslo"
fn zone(input: &mut &str) -> ModalResult<Tz> {
    let name = take_while(1.., |c: char| {
        c.is_ascii_alphanumeric() || "/_+-".contains(c)
    })
    .parse_next(input)?;
    match lookup_zone(name) {
        Some(zone) => Ok(zone),
        None => backtrack(),
    }
}

// ---------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------

/// Matches `w` ignoring case, as a whole word: "in" matches "in 2h" but not "inside".
/// Digits may follow, so "h" still matches in "2h30m". An apostrophe counts as part of the
/// word, so "tomorrow's standup" isn't read as "tomorrow".
fn word<'s>(w: &'static str) -> impl Parser<&'s str, &'s str, ErrMode<ContextError>> {
    terminated(
        literal(Caseless(w)),
        not(one_of(|c: char| c.is_alphabetic() || c == '\'' || c == '’')),
    )
}

/// The first of `words` that matches, tried in order.
fn any_word<'s>(
    words: &'static [&'static str],
) -> impl Parser<&'s str, &'s str, ErrMode<ContextError>> {
    move |input: &mut &'s str| {
        for w in words {
            let start = *input;
            if let Ok(found) = word(w).parse_next(input) {
                return Ok(found);
            }
            *input = start;
        }
        backtrack()
    }
}

fn two_digits(input: &mut &str) -> ModalResult<u32> {
    take_while(2, |c: char| c.is_ascii_digit())
        .parse_to()
        .parse_next(input)
}

/// "No match here, try something else."
fn backtrack<T>() -> ModalResult<T> {
    Err(ErrMode::Backtrack(ContextError::new()))
}

fn describe(what: &'static str) -> StrContext {
    StrContext::Expected(StrContextValue::Description(what))
}

/// The description attached with `.context(describe(...))`, for error messages.
fn expected(err: &ContextError) -> &'static str {
    err.context()
        .find_map(|c| match c {
            StrContext::Expected(StrContextValue::Description(d)) => Some(*d),
            _ => None,
        })
        .unwrap_or("a time")
}

/// Strips the glue around a message: "in 2h: to check the oven" becomes "check the oven",
/// and "in 2h and call mom" becomes "call mom".
fn clean_message(s: &str) -> String {
    let s = s.trim().trim_start_matches([':', '-', ',']).trim_start();
    let s = strip_word(s, "and ");
    let s = strip_word(s, "to ");
    s.trim()
        .trim_end_matches([':', '-', ','])
        .trim_end()
        .to_string()
}

/// `s` without `word` at its start, ignoring case.
fn strip_word<'s>(s: &'s str, word: &str) -> &'s str {
    match s.get(..word.len()) {
        Some(start) if start.eq_ignore_ascii_case(word) => &s[word.len()..],
        _ => s,
    }
}

/// Adds `n` units to `t`. Years, months, weeks and days move the date on the calendar in
/// `zone` and keep the clock time there, so summer time doesn't shift the hour and "in 1
/// month" on Jan 31 in Tokyo is Feb 28 in Tokyo. Hours, minutes and seconds are exact.
fn add(t: DateTime<Utc>, n: u32, unit: Unit, zone: Tz) -> Option<DateTime<Utc>> {
    let n64 = i64::from(n);
    let local = t.with_timezone(&zone).naive_local();
    let moved = match unit {
        Unit::Year => local.checked_add_months(Months::new(n.checked_mul(12)?)),
        Unit::Month => local.checked_add_months(Months::new(n)),
        Unit::Week => local.checked_add_days(Days::new(u64::from(n) * 7)),
        Unit::Day => local.checked_add_days(Days::new(n.into())),
        Unit::Hour => return t.checked_add_signed(TimeDelta::try_hours(n64)?),
        Unit::Minute => return t.checked_add_signed(TimeDelta::try_minutes(n64)?),
        Unit::Second => return t.checked_add_signed(TimeDelta::try_seconds(n64)?),
    }?;
    to_utc(zone, moved.date(), moved.time()).ok()
}

/// A wall-clock time in `zone` as UTC. When summer time skips that hour, the hour after
/// is used; when the hour happens twice, the first one is used.
fn to_utc(zone: Tz, date: NaiveDate, time: NaiveTime) -> Result<DateTime<Utc>, String> {
    let local = date.and_time(time);
    zone.from_local_datetime(&local)
        .earliest()
        .or_else(|| {
            zone.from_local_datetime(&(local + TimeDelta::hours(1)))
                .earliest()
        })
        .map(|t| t.with_timezone(&Utc))
        .ok_or_else(|| format!("{local} doesn't exist in {zone}."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_on_its_own() {
        assert!(matches!(parse_when("in 2h"), Ok(When::In(_))));
        assert!(matches!(
            parse_when(" tomorrow at 9am "),
            Ok(When::At { .. })
        ));
        assert!(parse_when("in 2h to cook").is_err());
        assert!(parse_when("in soon").is_err());
        assert!(parse_when("whenever").is_err());
    }

    /// Sunday 2026-02-22 12:00 UTC, the same reference time as the Go tests.
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 2, 22, 12, 0, 0).unwrap()
    }

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    /// Parses and resolves with UTC as the user's timezone.
    fn run(text: &str) -> (DateTime<Utc>, String) {
        run_in(text, now(), Tz::UTC)
    }

    fn run_in(text: &str, now: DateTime<Utc>, home: Tz) -> (DateTime<Utc>, String) {
        let parsed = parse_reminder(text).unwrap_or_else(|e| panic!("{text:?}: {e}"));
        let at = resolve(&parsed.when, now, home).unwrap_or_else(|e| panic!("{text:?}: {e}"));
        (at, parsed.message)
    }

    // Ported from voltgpt's parse_test.go.
    #[test]
    fn relative_times() {
        let cases = [
            (
                "in 2h to check the oven",
                TimeDelta::hours(2),
                "check the oven",
            ),
            (
                "in 30m: buy groceries",
                TimeDelta::minutes(30),
                "buy groceries",
            ),
            ("in 2h30m meeting", TimeDelta::minutes(150), "meeting"),
            ("in 1 hour dentist", TimeDelta::hours(1), "dentist"),
            ("in 30 minutes call mom", TimeDelta::minutes(30), "call mom"),
            ("in 7 days workout", TimeDelta::days(7), "workout"),
            ("in 1y update CV", TimeDelta::days(365), "update CV"),
            ("in 2 years tax", TimeDelta::days(730), "tax"),
            // New in VoltBot.
            ("in 1 week 2 days stretch", TimeDelta::days(9), "stretch"),
            (
                "in 1 week and 2 days stretch",
                TimeDelta::days(9),
                "stretch",
            ),
            ("in 1h, 15 mins tea", TimeDelta::minutes(75), "tea"),
            ("in 2h and call mom", TimeDelta::hours(2), "call mom"),
            ("in 2h and to call mom", TimeDelta::hours(2), "call mom"),
            ("IN 2H shout", TimeDelta::hours(2), "shout"),
            ("in 3 months renew", TimeDelta::days(89), "renew"),
        ];
        for (text, delta, message) in cases {
            let (at, msg) = run(text);
            assert_eq!(at - now(), delta, "{text}");
            assert_eq!(msg, message, "{text}");
        }
    }

    // Ported from voltgpt's parse_test.go.
    #[test]
    fn absolute_times() {
        assert_eq!(
            run("at 2026-03-01 15:04 check it"),
            (utc(2026, 3, 1, 15, 4), "check it".into())
        );
        assert_eq!(
            run("at 2026-03-01 15:04 EST check it"),
            (utc(2026, 3, 1, 20, 4), "check it".into())
        );
        // Still ahead today.
        assert_eq!(run("at 15:00 reminder").0, utc(2026, 2, 22, 15, 0));
        // Already passed today, so tomorrow.
        assert_eq!(run("at 10:00 reminder").0, utc(2026, 2, 23, 10, 0));
    }

    #[test]
    fn clock_forms() {
        assert_eq!(run("at 3pm x").0, utc(2026, 2, 22, 15, 0));
        assert_eq!(run("at 3:30 pm x").0, utc(2026, 2, 22, 15, 30));
        assert_eq!(
            run("at 12:30:15 x").0,
            utc(2026, 2, 22, 12, 30) + TimeDelta::seconds(15)
        );
        assert_eq!(run("at noon x").0, utc(2026, 2, 23, 12, 0));
        assert_eq!(run("at midnight x").0, utc(2026, 2, 23, 0, 0));
        assert_eq!(run("at 12am x").0, utc(2026, 2, 23, 0, 0));
        assert_eq!(run("at 12pm x").0, utc(2026, 2, 23, 12, 0));
    }

    #[test]
    fn days() {
        assert_eq!(
            run("tomorrow at 3pm call mom"),
            (utc(2026, 2, 23, 15, 0), "call mom".into())
        );
        assert_eq!(run("tomorrow").0, utc(2026, 2, 23, 9, 0));
        assert_eq!(
            run("friday dentist"),
            (utc(2026, 2, 27, 9, 0), "dentist".into())
        );
        assert_eq!(run("on fri dentist").0, utc(2026, 2, 27, 9, 0));
        assert_eq!(run("at 9:00 friday x").0, utc(2026, 2, 27, 9, 0));
        // Today is Sunday: plain "sunday" is today while the time is ahead...
        assert_eq!(run("sunday at 13:00 x").0, utc(2026, 2, 22, 13, 0));
        // ...and next week once it has passed.
        assert_eq!(run("sunday at 11:00 x").0, utc(2026, 3, 1, 11, 0));
        // "next sunday" is never today.
        assert_eq!(run("next sunday at 13:00 x").0, utc(2026, 3, 1, 13, 0));
        assert_eq!(run("next monday x").0, utc(2026, 2, 23, 9, 0));
        assert_eq!(
            run("on 2026-12-24 at 18:00 CET christmas"),
            (utc(2026, 12, 24, 17, 0), "christmas".into())
        );
        assert_eq!(run("2026-12-24 x").0, utc(2026, 12, 24, 9, 0));
    }

    #[test]
    fn day_first_time_last() {
        assert_eq!(
            run("tomorrow call mom at 5pm"),
            (utc(2026, 2, 23, 17, 0), "call mom".into())
        );
        assert_eq!(
            run("friday dentist at 3:30pm CET."),
            (utc(2026, 2, 27, 14, 30), "dentist".into())
        );
        // A time in the middle of the message stays in the message.
        assert_eq!(
            run("tomorrow at 3pm is the deadline"),
            (utc(2026, 2, 23, 15, 0), "is the deadline".into())
        );
        assert_eq!(
            run("tomorrow meet at 5pm friday"),
            (utc(2026, 2, 23, 9, 0), "meet at 5pm friday".into())
        );
    }

    #[test]
    fn apostrophe_ends_no_word() {
        assert_eq!(
            run("tomorrow's standup at 3pm"),
            (utc(2026, 2, 22, 15, 0), "tomorrow's standup".into())
        );
        assert_eq!(run("friday’s game at 3pm").1, "friday’s game");
    }

    #[test]
    fn calendar_units_follow_the_users_zone() {
        // Jan 31 00:00 in Tokyo is Jan 30 in UTC; a month later is Feb 28 in Tokyo.
        let tokyo_jan_31 = utc(2026, 1, 30, 15, 0);
        assert_eq!(
            run_in("in 1 month x", tokyo_jan_31, Tz::Asia__Tokyo).0,
            utc(2026, 2, 27, 15, 0)
        );
        // New York moves to summer time on Mar 8: "in 1 day" keeps 12:00 on the clock,
        // "in 24h" doesn't.
        let ny_noon = utc(2026, 3, 7, 17, 0);
        assert_eq!(
            run_in("in 1 day x", ny_noon, Tz::America__New_York).0,
            utc(2026, 3, 8, 16, 0)
        );
        assert_eq!(
            run_in("in 1 week x", ny_noon, Tz::America__New_York).0,
            utc(2026, 3, 14, 16, 0)
        );
        assert_eq!(
            run_in("in 24h x", ny_noon, Tz::America__New_York).0,
            utc(2026, 3, 8, 17, 0)
        );
    }

    #[test]
    fn zones() {
        assert_eq!(run("at 16:30 Europe/Oslo x").0, utc(2026, 2, 22, 15, 30));
        assert_eq!(run("at 16:30 europe/oslo x").0, utc(2026, 2, 22, 15, 30));
        // CET follows summer time, unlike Go's fixed offsets.
        let july = utc(2026, 7, 1, 8, 0);
        assert_eq!(
            run_in("at 16:30 CET x", july, Tz::UTC).0,
            utc(2026, 7, 1, 14, 30)
        );
        // Without a zone in the text, the user's own zone is used.
        assert_eq!(
            run_in("at 15:00 x", now(), Tz::America__New_York).0,
            utc(2026, 2, 22, 20, 0)
        );
        // Words that happen to be tz names stay part of the message.
        assert_eq!(run("at 5pm turkey in the oven").1, "turkey in the oven");
    }

    #[test]
    fn time_at_the_end() {
        assert_eq!(
            run("to check the oven in 2h"),
            (now() + TimeDelta::hours(2), "check the oven".into())
        );
        assert_eq!(
            run("call mom tomorrow at 3pm"),
            (utc(2026, 2, 23, 15, 0), "call mom".into())
        );
        assert_eq!(
            run("meet at the station at 5pm."),
            (utc(2026, 2, 22, 17, 0), "meet at the station".into())
        );
    }

    #[test]
    fn errors() {
        let err = parse_reminder("something").unwrap_err();
        assert_eq!(err, "I couldn't find a time in that.");

        let err = parse_reminder("in nothing").unwrap_err();
        assert!(err.contains("\"nothing\""), "{err}");
        assert!(err.contains("duration"), "{err}");

        let err = parse_reminder("at invalid-date here").unwrap_err();
        assert!(err.contains("\"invalid-date\""), "{err}");

        // "5 dogs" is not a duration.
        assert!(parse_reminder("in 5 dogs").is_err());
        // A bare hour is too vague.
        assert!(parse_reminder("at 5 o'clock").is_err());

        let past = parse_reminder("on 2020-01-01 x").unwrap();
        assert!(resolve(&past.when, now(), Tz::UTC).is_err());
    }

    #[test]
    fn zone_lookup() {
        assert_eq!(lookup_zone("cest"), Some(Tz::Europe__Paris));
        assert_eq!(lookup_zone("America/New_York"), Some(Tz::America__New_York));
        assert_eq!(lookup_zone("Turkey"), None);
        assert_eq!(lookup_zone("Mars/Base"), None);
    }
}
