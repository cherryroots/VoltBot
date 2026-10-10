//! The status as a picture, drawn like the movie wheel's rounds (see `util::svg`).
//!
//! [`online_svg`] lays out a [`Dashboard`]: a header, four tiles with the last day's graph
//! (latency, memory, database, errors), the AI provider, Claude's spend this month with a
//! graph of the month so far, one card per feature with its `stats()`, and the last error.
//! Graphs come from the samples in `history.rs`; a graph needs two samples, so a new
//! install shows plain numbers for its first 15 minutes.
//!
//! Text from features and from the AI status can hold Discord timestamps (`<t:…:R>`). The
//! embed shows those as they are; here [`plain`] writes them out ("in 3h 12m").

use chrono::{DateTime, Datelike, Months, NaiveDate, TimeDelta, Utc};

use super::history::{self, History};
use crate::core::logging::ErrorStats;
use crate::core::{Panel, Stat};
use crate::util::shorten;
use crate::util::svg::{
    ACCENT, AMBER, DIM, GREEN, LINE, MUTED, RED, ROW, Style, Svg, TEXT, esc, text_width,
};

/// Width of the picture in SVG units.
const WIDTH: f32 = 640.0;
/// Left and right edge of the cards.
const LEFT: f32 = 16.0;
const RIGHT: f32 = WIDTH - 16.0;
/// Space between cards.
const GAP: f32 = 8.0;

/// Everything the status shows, gathered by `status.rs`. The fallback embed shows the same.
pub struct Dashboard {
    pub now: DateTime<Utc>,
    pub bot_name: String,
    /// "0.1.0 · abc1234"
    pub version: String,
    pub uptime: TimeDelta,
    pub servers: usize,
    pub latency_ms: Option<f64>,
    pub memory_mb: Option<f64>,
    pub database_mb: f64,
    pub errors: ErrorStats,
    /// `None` when chat has no provider (no API key).
    pub ai: Option<AiView>,
    /// `None` when Claude isn't set up; an error when this month couldn't be read.
    pub spend: Option<Result<SpendView, String>>,
    pub features: Vec<FeatureView>,
    /// Boxes of longer text from the features, those with the same title merged.
    pub panels: Vec<Panel>,
    pub history: History,
}

pub struct AiView {
    /// "Claude"
    pub provider: String,
    pub model: String,
    /// Why chat isn't on the main provider, like "Claude is out of credit; trying again
    /// <t:…:R>".
    pub fallback: Option<String>,
}

pub struct SpendView {
    pub usd: f64,
    /// 0 when there's no budget.
    pub budget: f64,
    /// Anthropic's bill (with an Admin API key) rather than the estimate.
    pub billed: bool,
    /// "Admin key: on", or why it isn't used.
    pub admin_key: String,
    pub admin_key_failing: bool,
    /// "Last read <t:…:R>: $12.00 billed"
    pub last_read: Option<String>,
    /// Share of input read from the cache, when `show_cache_hits` is on.
    pub cache_hits: Option<f64>,
    /// USD per job, most expensive first, when `show_job_costs` is on.
    pub jobs: Option<Vec<(String, f64)>>,
}

pub struct FeatureView {
    pub name: &'static str,
    /// The feature's stats, or why they couldn't be loaded.
    pub stats: Result<Vec<Stat>, String>,
}

// ---------------------------------------------------------------------------------------
// The two pictures
// ---------------------------------------------------------------------------------------

pub fn online_svg(d: &Dashboard) -> String {
    let mut svg = Svg::new(WIDTH);
    let sub = format!(
        "{}  ·  updated {} UTC",
        plural(d.servers, "server"),
        d.now.format("%H:%M")
    );
    let right = [
        d.version.clone(),
        format!("up {}", human_duration(d.uptime)),
    ];
    header(&mut svg, &d.bot_name, "ONLINE", GREEN, &right, &sub);
    tiles(&mut svg, d);
    ai(&mut svg, d);
    for panel in &d.panels {
        text_panel(&mut svg, d, panel);
    }
    if let Some(spend) = &d.spend {
        match spend {
            Ok(spend) => claude_spend(&mut svg, d, spend),
            Err(err) => {
                section(&mut svg, "Claude spend this month");
                svg.note(&format!("Couldn't read it: {}", shorten(err, 80)));
            }
        }
    }
    features(&mut svg, d);
    last_error(&mut svg, d);

    svg.y += 22.0;
    svg.text(
        24.0,
        svg.y,
        "Graphs: system over the last day, features over 7 days, spend this month",
        Style::new(11.0, DIM),
    );
    svg.y += 14.0;
    svg.finish()
}

/// The last picture before the bot stops.
pub fn offline_svg(bot_name: &str, version: &str, now: DateTime<Utc>, uptime: TimeDelta) -> String {
    let mut svg = Svg::new(WIDTH);
    let sub = format!(
        "Stopped {} UTC  ·  was up for {}",
        now.format("%-d %b %H:%M"),
        human_duration(uptime)
    );
    header(
        &mut svg,
        bot_name,
        "OFFLINE",
        RED,
        &[version.to_string()],
        &sub,
    );
    svg.y += 4.0;
    svg.finish()
}

// ---------------------------------------------------------------------------------------
// Parts
// ---------------------------------------------------------------------------------------

/// "Vivy  ONLINE" with up to two lines on the right and a summary line under it.
fn header(
    svg: &mut Svg,
    name: &str,
    state: &str,
    color: &'static str,
    right: &[String],
    sub: &str,
) {
    let name = shorten(name, 20);
    svg.text(24.0, 44.0, &name, Style::new(26.0, TEXT).weight(700));
    let x = 24.0 + text_width(&name, 26.0) + 14.0;
    let width = 9.0 + state.len() as f32 * 7.4;
    svg.rect([x, 26.0, width, 20.0], color, 10.0, "fill-opacity=\"0.18\"");
    svg.text(
        x + width / 2.0,
        41.0,
        state,
        Style::new(11.0, color).weight(700).middle().spaced(),
    );
    for (n, line) in right.iter().enumerate() {
        svg.text(
            WIDTH - 24.0,
            30.0 + 16.0 * n as f32,
            line,
            Style::new(12.0, DIM).weight(500).end(),
        );
    }
    svg.text(24.0, 70.0, sub, Style::new(13.0, MUTED));
    svg.y = 86.0;
}

/// Latency, memory, database and errors, each with the last day's graph.
fn tiles(svg: &mut Svg, d: &Dashboard) {
    let width = (RIGHT - LEFT - 3.0 * GAP) / 4.0;
    let height = 84.0;
    let y = svg.y;
    let day_ago = d.now.timestamp() - 86_400;
    let tile = |svg: &mut Svg, n: usize, label: &str| {
        let x = LEFT + n as f32 * (width + GAP);
        svg.rect([x, y, width, height], ROW, 8.0, "");
        svg.text(
            x + 12.0,
            y + 20.0,
            label,
            Style::new(10.0, DIM).weight(700).spaced(),
        );
        x
    };

    let measured = [
        (
            "LATENCY",
            history::LATENCY,
            d.latency_ms,
            format_ms as fn(f64) -> String,
        ),
        ("MEMORY", history::MEMORY, d.memory_mb, format_mb),
        (
            "DATABASE",
            history::DATABASE,
            Some(d.database_mb),
            format_mb,
        ),
    ];
    for (n, (label, key, value, format)) in measured.into_iter().enumerate() {
        let x = tile(svg, n, label);
        let shown = value.map_or("–".to_string(), format);
        svg.text(
            x + 12.0,
            y + 46.0,
            &shown,
            Style::new(20.0, TEXT).weight(700),
        );
        let points = series(
            &d.history,
            key,
            day_ago,
            value.map(|v| (d.now.timestamp(), v)),
        );
        sparkline(
            svg,
            [x + 12.0, y + 56.0, width - 24.0, 18.0],
            &points,
            (day_ago, d.now.timestamp()),
            ACCENT,
        );
    }

    // Errors: today's count, and a bar per hour.
    let x = tile(svg, 3, "ERRORS");
    let color = if d.errors.last_hour > 0 { RED } else { TEXT };
    svg.text(
        x + 12.0,
        y + 46.0,
        &d.errors.last_day.to_string(),
        Style::new(20.0, color).weight(700),
    );
    svg.text(
        x + 12.0 + text_width(&d.errors.last_day.to_string(), 20.0) + 6.0,
        y + 46.0,
        &format!("today, {} this hour", d.errors.last_hour),
        Style::new(11.0, DIM),
    );
    bars(
        svg,
        [x + 12.0, y + 56.0, width - 24.0, 18.0],
        &d.errors.per_hour,
        RED,
    );
    svg.y += height;
}

/// The provider and model chat uses, and the fallback if it's on one.
fn ai(svg: &mut Svg, d: &Dashboard) {
    section(svg, "AI");
    let lines = d.ai.as_ref().and_then(|ai| ai.fallback.as_ref()).is_some();
    let height = if lines { 62.0 } else { 40.0 };
    let y = svg.y;
    svg.rect([LEFT, y, RIGHT - LEFT, height], ROW, 8.0, "");
    let Some(ai) = &d.ai else {
        svg.text(32.0, y + 25.0, "Off (no API key)", Style::new(14.0, DIM));
        svg.y += height;
        return;
    };
    svg.text(
        32.0,
        y + 25.0,
        &ai.provider,
        Style::new(15.0, TEXT).weight(700),
    );
    svg.text(
        32.0 + text_width(&ai.provider, 15.0) + 12.0,
        y + 25.0,
        &ai.model,
        Style::new(13.0, MUTED),
    );
    let (state, color) = if ai.fallback.is_some() {
        ("FALLBACK", AMBER)
    } else {
        ("MAIN", GREEN)
    };
    pill(svg, RIGHT - 16.0, y + 11.0, state, color);
    if let Some(fallback) = &ai.fallback {
        svg.text(
            32.0,
            y + 48.0,
            &shorten(&plain(fallback, d.now), 90),
            Style::new(12.0, AMBER).weight(500),
        );
    }
    svg.y += height;
}

/// A feature's box of longer text: labels on the left, a line or a few on the right, and
/// its picture (like Vivy's face) in a circle at the top right.
fn text_panel(svg: &mut Svg, d: &Dashboard, panel: &Panel) {
    section(svg, &panel.title);
    let (label_x, text_x) = (32.0, 160.0);
    let picture_size = 64.0;
    let text_right = match panel.picture {
        Some(_) => RIGHT - 16.0 - picture_size - 12.0,
        None => RIGHT - 16.0,
    };
    // Running text is narrower than `text_width`'s estimate, which is made for names.
    let chars = ((text_right - text_x) / (13.0 * 0.5)) as usize;
    let rows: Vec<(&str, Vec<String>)> = panel
        .rows
        .iter()
        .map(|row| {
            (
                row.name.as_str(),
                wrap_words(&plain(&row.value, d.now), chars, 3),
            )
        })
        .collect();
    let lines: usize = rows.iter().map(|(_, lines)| lines.len().max(1)).sum();
    let mut height = 20.0 + 19.0 * lines as f32 + 8.0 * rows.len().saturating_sub(1) as f32;
    if panel.picture.is_some() {
        height = height.max(picture_size + 24.0);
    }
    let y = svg.y;
    svg.rect([LEFT, y, RIGHT - LEFT, height], ROW, 8.0, "");
    svg.rect([LEFT, y, 4.0, height], ACCENT, 2.0, "");
    if let Some(picture) = &panel.picture {
        let r = picture_size / 2.0;
        let (cx, cy) = (RIGHT - 16.0 - r, y + 12.0 + r);
        let id = format!("picture{}", y as i32);
        svg.raw(format!(
            "<clipPath id=\"{id}\"><circle cx=\"{cx}\" cy=\"{cy}\" r=\"{r}\"/></clipPath>\
             <image href=\"{}\" x=\"{}\" y=\"{}\" width=\"{picture_size}\" height=\"{picture_size}\" \
             preserveAspectRatio=\"xMidYMid slice\" clip-path=\"url(#{id})\"/>\
             <circle cx=\"{cx}\" cy=\"{cy}\" r=\"{r}\" fill=\"none\" stroke=\"{ACCENT}\" stroke-width=\"2\"/>",
            esc(picture),
            cx - r,
            cy - r,
        ));
    }
    let mut line_y = y + 24.0;
    for (label, lines) in rows {
        svg.text(
            label_x,
            line_y,
            &shorten(label, 16),
            Style::new(12.0, DIM).weight(600),
        );
        for line in &lines {
            svg.text(text_x, line_y, line, Style::new(13.0, TEXT).weight(500));
            line_y += 19.0;
        }
        if lines.is_empty() {
            line_y += 19.0;
        }
        line_y += 8.0;
    }
    svg.y += height;
}

/// The month's total against the budget, a graph of the month so far, and the details.
fn claude_spend(svg: &mut Svg, d: &Dashboard, spend: &SpendView) {
    section(svg, "Claude spend this month");
    let y = svg.y;
    let height = 238.0;
    svg.rect([LEFT, y, RIGHT - LEFT, height], ROW, 8.0, "");

    // "$12.34 of $100" with a bar.
    let total = format!("${:.2}", spend.usd);
    svg.text(32.0, y + 40.0, &total, Style::new(28.0, TEXT).weight(700));
    let mut x = 32.0 + text_width(&total, 28.0) + 8.0;
    if spend.budget > 0.0 {
        let of = format!("of ${:.0}", spend.budget);
        svg.text(x, y + 40.0, &of, Style::new(14.0, MUTED).weight(500));
        x += text_width(&of, 14.0) + 10.0;
    }
    let (tag, tag_color) = if spend.billed {
        ("BILLED", GREEN)
    } else {
        ("ESTIMATE", DIM)
    };
    let tag_width = 9.0 + tag.len() as f32 * 7.4;
    pill(svg, x + tag_width, y + 26.0, tag, tag_color);

    let (start, end) = month_bounds(d.now);
    let elapsed = (d.now - start).num_seconds() as f64;
    let length = (end - start).num_seconds() as f64;
    // Only guess at the month's end after a day of it.
    let pace = (elapsed >= 86_400.0).then(|| spend.usd * length / elapsed);
    let share = (spend.budget > 0.0).then(|| spend.usd / spend.budget);
    let bar_color = match share {
        Some(s) if s >= 1.0 => RED,
        Some(s) if s >= 0.8 => AMBER,
        _ if pace
            .zip((spend.budget > 0.0).then_some(spend.budget))
            .is_some_and(|(p, b)| p > b) =>
        {
            AMBER
        }
        _ => GREEN,
    };
    if let Some(share) = share {
        let width = RIGHT - LEFT - 32.0;
        svg.rect([32.0, y + 52.0, width, 8.0], LINE, 4.0, "");
        let filled = (width * share.min(1.0) as f32).max(8.0);
        svg.rect([32.0, y + 52.0, filled, 8.0], bar_color, 4.0, "");
    }
    let mut summary = Vec::new();
    if let Some(share) = share {
        summary.push(format!("{:.0}% of the budget", share * 100.0));
    }
    if let Some(pace) = pace {
        summary.push(format!(
            "on pace for ${pace:.2} by {}",
            (end - TimeDelta::days(1)).format("%-d %b")
        ));
    }
    if !summary.is_empty() {
        svg.text(
            32.0,
            y + 80.0,
            &summary.join("  ·  "),
            Style::new(12.0, MUTED).weight(500),
        );
    }

    // The month so far, the budget, and where the pace leads.
    let chart = [32.0, y + 96.0, RIGHT - LEFT - 32.0, 96.0];
    let points = series(
        &d.history,
        history::SPEND,
        start.timestamp(),
        Some((d.now.timestamp(), spend.usd)),
    );
    month_chart(svg, chart, &points, spend, pace, (start, end), d.now);

    // The admin key and the last bill read.
    let mut key = spend.admin_key.clone();
    if let Some(read) = &spend.last_read {
        key = format!("{key}  ·  {}", plain(read, d.now));
    }
    let key_color = if spend.admin_key_failing { AMBER } else { DIM };
    svg.text(
        32.0,
        y + height - 12.0,
        &shorten(&key, 95),
        Style::new(11.0, key_color).weight(500),
    );
    svg.y += height;

    cache_and_jobs(svg, spend);
}

/// The cache hit rate as a ring and the cost per job as bars, each when turned on.
fn cache_and_jobs(svg: &mut Svg, spend: &SpendView) {
    let jobs = spend.jobs.as_deref().filter(|jobs| !jobs.is_empty());
    if spend.cache_hits.is_none() && jobs.is_none() {
        return;
    }
    svg.y += GAP;
    let y = svg.y;
    let rows = jobs.map_or(0, |jobs| jobs.len().min(6));
    let height = (40.0 + 22.0 * rows as f32).max(96.0);
    let mut x = LEFT;

    if let Some(hits) = spend.cache_hits {
        let width = if jobs.is_some() { 170.0 } else { RIGHT - LEFT };
        svg.rect([x, y, width, height], ROW, 8.0, "");
        card_title(svg, x, y, "CACHE HITS");
        let (cx, cy, r) = (x + 44.0, y + height / 2.0 + 10.0, 24.0);
        let circumference = 2.0 * std::f32::consts::PI * r;
        let arc = circumference * hits.clamp(0.0, 1.0) as f32;
        svg.raw(format!(
            "<circle cx=\"{cx}\" cy=\"{cy}\" r=\"{r}\" fill=\"none\" stroke=\"{LINE}\" stroke-width=\"7\"/>"
        ));
        svg.raw(format!(
            "<circle cx=\"{cx}\" cy=\"{cy}\" r=\"{r}\" fill=\"none\" stroke=\"{GREEN}\" stroke-width=\"7\" stroke-linecap=\"round\" stroke-dasharray=\"{arc} {circumference}\" transform=\"rotate(-90 {cx} {cy})\"/>"
        ));
        svg.text(
            cx + 40.0,
            cy + 2.0,
            &format!("{:.0}%", hits * 100.0),
            Style::new(20.0, TEXT).weight(700),
        );
        svg.text(cx + 40.0, cy + 18.0, "of input", Style::new(11.0, DIM));
        x += width + GAP;
    }

    if let Some(jobs) = jobs {
        let width = RIGHT - x;
        svg.rect([x, y, width, height], ROW, 8.0, "");
        card_title(svg, x, y, "COST PER JOB");
        let most = jobs.first().map_or(0.0, |(_, usd)| *usd).max(0.000_001);
        let (bar_x, bar_width) = (x + 110.0, width - 110.0 - 76.0);
        for (n, (job, usd)) in jobs.iter().take(rows).enumerate() {
            let row_y = y + 46.0 + 22.0 * n as f32;
            svg.text(
                x + 14.0,
                row_y,
                &shorten(job, 13),
                Style::new(13.0, MUTED).weight(500),
            );
            svg.rect([bar_x, row_y - 9.0, bar_width, 8.0], LINE, 4.0, "");
            let filled = (bar_width * (*usd / most) as f32).max(8.0);
            svg.rect([bar_x, row_y - 9.0, filled, 8.0], ACCENT, 4.0, "");
            svg.text(
                x + width - 14.0,
                row_y,
                &format!("${usd:.2}"),
                Style::new(13.0, TEXT).weight(600).end(),
            );
        }
    }
    svg.y += height;
}

/// One card per feature with its stats, two side by side. Numbers get a graph of the
/// last 7 days and how much they changed in that time.
fn features(svg: &mut Svg, d: &Dashboard) {
    let shown: Vec<&FeatureView> = d
        .features
        .iter()
        .filter(|f| !matches!(&f.stats, Ok(stats) if stats.is_empty()))
        .collect();
    if shown.is_empty() {
        return;
    }
    section(svg, "Features");
    let width = (RIGHT - LEFT - GAP) / 2.0;
    let week_ago = d.now.timestamp() - 7 * 86_400;
    let rows = |f: &FeatureView| match &f.stats {
        Ok(stats) => stats.len(),
        Err(_) => 1,
    };
    for (n, pair) in shown.chunks(2).enumerate() {
        if n > 0 {
            svg.y += GAP;
        }
        let height = 44.0 + 24.0 * pair.iter().map(|f| rows(f)).max().unwrap_or(1) as f32;
        for (column, feature) in pair.iter().enumerate() {
            let x = LEFT + column as f32 * (width + GAP);
            feature_card(svg, d, feature, [x, svg.y, width, height], week_ago);
        }
        svg.y += height;
    }
}

fn feature_card(svg: &mut Svg, d: &Dashboard, f: &FeatureView, card: [f32; 4], week_ago: i64) {
    let [x, y, width, _] = card;
    svg.rect(card, ROW, 8.0, "");
    svg.text(
        x + 14.0,
        y + 26.0,
        &title(f.name),
        Style::new(14.0, TEXT).weight(700),
    );
    let stats = match &f.stats {
        Ok(stats) => stats,
        Err(err) => {
            svg.text(
                x + 14.0,
                y + 54.0,
                &shorten(&format!("Couldn't load: {err}"), 40),
                Style::new(12.0, RED),
            );
            return;
        }
    };
    let mut graphed = false;
    for (n, stat) in stats.iter().enumerate() {
        let row_y = y + 54.0 + 24.0 * n as f32;
        let value = plain(&stat.value, d.now);
        svg.text(
            x + 14.0,
            row_y,
            &shorten(&stat.name, 18),
            Style::new(13.0, MUTED).weight(500),
        );
        let points = match value.parse::<f64>() {
            Ok(number) => series(
                &d.history,
                &history::stat_key(f.name, &stat.name),
                week_ago,
                Some((d.now.timestamp(), number)),
            ),
            Err(_) => Vec::new(),
        };
        if points.len() < 2 {
            // Room for the value is what the name leaves.
            let room = width - 28.0 - text_width(&stat.name, 13.0) - 12.0;
            let max = (room / (13.0 * 0.6)).max(4.0) as usize;
            svg.text(
                x + width - 14.0,
                row_y,
                &shorten(&value, max),
                Style::new(13.0, TEXT).weight(600).end(),
            );
            continue;
        }
        graphed = true;
        svg.text(
            x + width - 14.0,
            row_y,
            &value,
            Style::new(13.0, TEXT).weight(600).end(),
        );
        let spark = [x + width - 14.0 - 52.0 - 80.0, row_y - 12.0, 80.0, 14.0];
        sparkline(svg, spark, &points, (week_ago, d.now.timestamp()), ACCENT);
        let change = points[points.len() - 1].1 - points[0].1;
        let (text, color) = match change {
            c if c > 0.0 => (format!("+{}", number(c)), GREEN),
            c if c < 0.0 => (format!("−{}", number(-c)), AMBER),
            _ => ("±0".to_string(), DIM),
        };
        svg.text(
            spark[0] - 8.0,
            row_y,
            &text,
            Style::new(11.0, color).weight(600).end(),
        );
    }
    if graphed {
        svg.text(
            x + width - 14.0,
            y + 25.0,
            "7 days",
            Style::new(11.0, DIM).end(),
        );
    }
}

/// The most recent error, in a red box.
fn last_error(svg: &mut Svg, d: &Dashboard) {
    let Some((at, text)) = &d.errors.last else {
        return;
    };
    section(svg, "Last error");
    let lines = wrap_words(&first_lines(text), 88, 4);
    let height = 36.0 + 17.0 * lines.len() as f32;
    let y = svg.y;
    svg.rect(
        [LEFT, y, RIGHT - LEFT, height],
        RED,
        8.0,
        "fill-opacity=\"0.10\"",
    );
    svg.rect([LEFT, y, 4.0, height], RED, 2.0, "");
    svg.text(
        32.0,
        y + 22.0,
        &format!(
            "{}  ·  {} UTC",
            relative(*at, d.now),
            at.format("%-d %b %H:%M")
        ),
        Style::new(12.0, RED).weight(600),
    );
    for (n, line) in lines.iter().enumerate() {
        svg.text(
            32.0,
            y + 42.0 + 17.0 * n as f32,
            line,
            Style::new(12.0, MUTED),
        );
    }
    svg.y += height;
}

// ---------------------------------------------------------------------------------------
// Graphs
// ---------------------------------------------------------------------------------------

/// The samples of `key` since `since`, with the current value added at the end.
fn series(history: &History, key: &str, since: i64, now: Option<(i64, f64)>) -> Vec<(i64, f64)> {
    let mut points: Vec<(i64, f64)> = history
        .get(key)
        .map(|samples| {
            samples
                .iter()
                .filter(|(at, _)| *at >= since)
                .copied()
                .collect()
        })
        .unwrap_or_default();
    points.extend(now);
    points
}

/// A small line graph of `points` over the times `span`, filled underneath. Nothing with
/// fewer than two points.
fn sparkline(svg: &mut Svg, area: [f32; 4], points: &[(i64, f64)], span: (i64, i64), color: &str) {
    if points.len() < 2 {
        return;
    }
    let low = points.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
    let high = points.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
    let coords = scale(points, area, span, (low, high));
    let line = path(&coords);
    let [_, y, _, h] = area;
    let (first, last) = (coords[0].0, coords[coords.len() - 1].0);
    svg.raw(format!(
        "<path d=\"{line} L{last:.1},{:.1} L{first:.1},{:.1} Z\" fill=\"{color}\" fill-opacity=\"0.15\"/>",
        y + h,
        y + h
    ));
    svg.raw(format!(
        "<path d=\"{line}\" fill=\"none\" stroke=\"{color}\" stroke-width=\"1.5\" stroke-linejoin=\"round\" stroke-linecap=\"round\"/>"
    ));
}

/// Bars for `counts`, left to right, scaled to the highest. Empty slots show as a dot.
fn bars(svg: &mut Svg, [x, y, w, h]: [f32; 4], counts: &[usize], color: &str) {
    let most = counts.iter().copied().max().unwrap_or(0).max(1) as f32;
    let slot = w / counts.len() as f32;
    for (n, &count) in counts.iter().enumerate() {
        let bar_x = x + slot * n as f32 + 0.5;
        if count == 0 {
            svg.rect([bar_x, y + h - 2.0, slot - 1.0, 2.0], LINE, 1.0, "");
        } else {
            let bar_h = (h * count as f32 / most).max(3.0);
            svg.rect([bar_x, y + h - bar_h, slot - 1.0, bar_h], color, 1.0, "");
        }
    }
}

/// Spend over the month: the line so far, the pace to the month's end (dotted) and the
/// budget (dashed), with the days along the bottom.
fn month_chart(
    svg: &mut Svg,
    area: [f32; 4],
    points: &[(i64, f64)],
    spend: &SpendView,
    pace: Option<f64>,
    (start, end): (DateTime<Utc>, DateTime<Utc>),
    now: DateTime<Utc>,
) {
    let [x, y, w, h] = area;
    let span = (start.timestamp(), end.timestamp());
    let top = [spend.usd, spend.budget, pace.unwrap_or(0.0)]
        .into_iter()
        .fold(0.01, f64::max)
        * 1.1;
    let range = (0.0, top);

    // The floor and the days of the month.
    svg.raw(format!(
        "<path d=\"M{x},{} H{}\" stroke=\"{LINE}\" stroke-width=\"1\"/>",
        y + h,
        x + w
    ));
    let days = (end - start).num_days();
    for day in [1, 8, 15, 22, days] {
        let at = start + TimeDelta::days(day - 1) + TimeDelta::hours(12);
        let (tx, _) = scale(&[(at.timestamp(), 0.0)], area, span, range)[0];
        svg.text(
            tx,
            y + h + 14.0,
            &format!("{} {}", day, start.format("%b")),
            Style::new(10.0, DIM).middle(),
        );
    }

    if spend.budget > 0.0 {
        let (_, by) = scale(&[(span.0, spend.budget)], area, span, range)[0];
        svg.raw(format!(
            "<path d=\"M{x},{by:.1} H{}\" stroke=\"{AMBER}\" stroke-opacity=\"0.7\" stroke-width=\"1\" stroke-dasharray=\"4 4\"/>",
            x + w
        ));
        svg.text(
            x + w,
            by - 5.0,
            &format!("budget ${:.0}", spend.budget),
            Style::new(10.0, AMBER).weight(600).end(),
        );
    }

    let (nx, ny) = scale(&[(now.timestamp(), spend.usd)], area, span, range)[0];
    if let Some(pace) = pace {
        let (ex, ey) = scale(&[(span.1, pace)], area, span, range)[0];
        svg.raw(format!(
            "<path d=\"M{nx:.1},{ny:.1} L{ex:.1},{ey:.1}\" stroke=\"{ACCENT}\" stroke-opacity=\"0.6\" stroke-width=\"1.5\" stroke-dasharray=\"2 4\" stroke-linecap=\"round\"/>"
        ));
    }
    if points.len() >= 2 {
        let coords = scale(points, area, span, range);
        let line = path(&coords);
        svg.raw(format!(
            "<path d=\"{line} L{nx:.1},{:.1} L{:.1},{:.1} Z\" fill=\"{ACCENT}\" fill-opacity=\"0.18\"/>",
            y + h,
            coords[0].0,
            y + h
        ));
        svg.raw(format!(
            "<path d=\"{line}\" fill=\"none\" stroke=\"{ACCENT}\" stroke-width=\"2\" stroke-linejoin=\"round\"/>"
        ));
    }
    svg.raw(format!(
        "<circle cx=\"{nx:.1}\" cy=\"{ny:.1}\" r=\"3.5\" fill=\"{ACCENT}\"/>"
    ));
}

/// Points in time and value to positions in `area`. A flat line sits in the middle.
fn scale(
    points: &[(i64, f64)],
    [x, y, w, h]: [f32; 4],
    (from, to): (i64, i64),
    (low, high): (f64, f64),
) -> Vec<(f32, f32)> {
    let length = (to - from).max(1) as f32;
    points
        .iter()
        .map(|&(at, value)| {
            let px = x + w * ((at - from) as f32 / length).clamp(0.0, 1.0);
            let share = if high - low < 1e-9 {
                0.5
            } else {
                ((value - low) / (high - low)) as f32
            };
            (px, y + h - h * share.clamp(0.0, 1.0))
        })
        .collect()
}

/// "M1,2 L3,4 ..."
fn path(coords: &[(f32, f32)]) -> String {
    coords
        .iter()
        .enumerate()
        .map(|(n, (x, y))| format!("{}{x:.1},{y:.1}", if n == 0 { "M" } else { " L" }))
        .collect()
}

// ---------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------

/// A section title, with room above it for the cards before.
fn section(svg: &mut Svg, title: &str) {
    svg.y += 12.0;
    svg.section(title);
}

/// A small label at the top of a card.
fn card_title(svg: &mut Svg, x: f32, y: f32, text: &str) {
    svg.text(
        x + 14.0,
        y + 22.0,
        text,
        Style::new(10.0, DIM).weight(700).spaced(),
    );
}

/// A rounded label like "BILLED", right-aligned at `right`.
fn pill(svg: &mut Svg, right: f32, y: f32, text: &str, color: &'static str) {
    let width = 9.0 + text.len() as f32 * 7.4;
    svg.rect(
        [right - width, y, width, 20.0],
        color,
        10.0,
        "fill-opacity=\"0.18\"",
    );
    svg.text(
        right - width / 2.0,
        y + 14.5,
        text,
        Style::new(10.0, color).weight(700).middle().spaced(),
    );
}

/// "reminders" → "Reminders", "control_panel" → "Control panel".
fn title(name: &str) -> String {
    let name = name.replace('_', " ");
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => name,
    }
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

fn format_ms(ms: f64) -> String {
    format!("{ms:.0} ms")
}

fn format_mb(mb: f64) -> String {
    format!("{mb:.1} MB")
}

/// 12 → "12", 2.5 → "2.5", 0.123 → "0.12".
fn number(n: f64) -> String {
    if n.fract().abs() < 1e-9 {
        format!("{n:.0}")
    } else if n >= 10.0 {
        format!("{n:.1}")
    } else {
        format!("{n:.2}")
    }
}

/// The first day of this month and of the next, in UTC.
fn month_bounds(now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    let first = NaiveDate::from_ymd_opt(now.year(), now.month(), 1).expect("valid date");
    let next = first + Months::new(1);
    let midnight = |date: NaiveDate| date.and_hms_opt(0, 0, 0).expect("valid time").and_utc();
    (midnight(first), midnight(next))
}

/// "3d 4h 12m", "4h 12m" or "12m".
pub fn human_duration(duration: TimeDelta) -> String {
    let minutes = duration.num_minutes().max(0);
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours) {
        (0, 0) => format!("{minutes}m"),
        (0, _) => format!("{hours}h {minutes}m"),
        _ => format!("{days}d {hours}h {minutes}m"),
    }
}

/// "in 3h 12m", "12m ago" or "just now".
fn relative(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let delta = at - now;
    if delta.num_seconds().abs() < 60 {
        return "just now".to_string();
    }
    // Two units at most: "3d 4h" rather than "3d 4h 12m".
    let short = |d: TimeDelta| {
        let text = human_duration(d);
        let parts: Vec<&str> = text.split(' ').take(2).collect();
        // "9h 0m" reads better as "9h".
        match parts[..] {
            [first, second] if !second.starts_with('0') => format!("{first} {second}"),
            _ => parts[0].to_string(),
        }
    };
    if delta > TimeDelta::zero() {
        format!("in {}", short(delta))
    } else {
        format!("{} ago", short(-delta))
    }
}

/// Discord markup as plain text: timestamps (`<t:1700000000:R>`) written out, and code
/// backticks left out.
fn plain(text: &str, now: DateTime<Utc>) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("<t:") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 3..];
        let Some(end) = after.find('>') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let (seconds, style) = after[..end].split_once(':').unwrap_or((&after[..end], "f"));
        match seconds
            .parse::<i64>()
            .ok()
            .and_then(|s| DateTime::from_timestamp(s, 0))
        {
            Some(at) if style == "R" => out.push_str(&relative(at, now)),
            Some(at) => out.push_str(&format!("{} UTC", at.format("%-d %b %H:%M"))),
            None => out.push_str(&rest[start..start + 4 + end]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out.replace('`', "")
}

/// The error's first lines as one line of text.
fn first_lines(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Words into lines of at most `width` characters, at most `max` lines (the last ends in
/// "…" when cut).
fn wrap_words(text: &str, width: usize, max: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        let word = shorten(word, width);
        match lines.last_mut() {
            Some(line) if line.chars().count() + 1 + word.chars().count() <= width => {
                line.push(' ');
                line.push_str(&word);
            }
            _ => lines.push(word),
        }
    }
    if lines.len() > max {
        lines.truncate(max);
        let last = &mut lines[max - 1];
        *last = format!("{}…", shorten(last, width - 1).trim_end_matches('…'));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().to_utc()
    }

    /// A week of made-up history: numbers that wander a bit, spend that grows.
    fn sample_history(now: DateTime<Utc>) -> History {
        let mut history = History::new();
        let (start, _) = month_bounds(now);
        let end = now.timestamp() / history::EVERY_SECS * history::EVERY_SECS;
        let mut t = end - 7 * 86_400;
        let mut answers: f64 = 1180.0;
        while t < end {
            let hour = (t / 3600 % 24) as f64;
            let wave = (t as f64 / 5000.0).sin();
            let mut push =
                |key: String, value: f64| history.entry(key).or_default().push((t, value));
            push(
                history::LATENCY.into(),
                42.0 + 8.0 * wave + if hour == 3.0 { 30.0 } else { 0.0 },
            );
            push(
                history::MEMORY.into(),
                38.0 + 4.0 * (t as f64 / 40_000.0).sin(),
            );
            push(
                history::DATABASE.into(),
                11.0 + (t - end) as f64 / 900_000.0,
            );
            if (8.0..23.0).contains(&hour) {
                answers += 1.0;
            }
            push(history::stat_key("chat", "Answers"), answers.floor());
            push(
                history::stat_key("reminders", "Pending"),
                4.0 + (2.0 * (t as f64 / 90_000.0).sin()).round(),
            );
            push(
                history::stat_key("snails", "Links"),
                5200.0 + ((t - end) / 4000) as f64,
            );
            if t >= start.timestamp() {
                let days = (t - start.timestamp()) as f64 / 86_400.0;
                push(history::SPEND.into(), 2.9 * days + 0.4 * wave);
            }
            t += history::EVERY_SECS;
        }
        history
    }

    fn sample(now: DateTime<Utc>) -> Dashboard {
        let mut per_hour = [0; 24];
        per_hour[5] = 2;
        per_hour[17] = 1;
        per_hour[23] = 1;
        Dashboard {
            now,
            bot_name: "Vivy".into(),
            version: "0.1.0 · c61bd5c".into(),
            uptime: TimeDelta::minutes(3 * 1440 + 4 * 60 + 12),
            servers: 3,
            latency_ms: Some(44.0),
            memory_mb: Some(39.4),
            database_mb: 11.2,
            errors: ErrorStats {
                last_hour: 1,
                last_day: 4,
                per_hour,
                last: Some((
                    now - TimeDelta::minutes(12),
                    "chat: couldn't answer <message link>\nCaused by:\n  Claude API error 529: Overloaded. The server is temporarily overloaded, please try again in a moment.".into(),
                )),
            },
            ai: Some(AiView {
                provider: "Claude".into(),
                model: "claude-opus-5-5".into(),
                fallback: None,
            }),
            spend: Some(Ok(SpendView {
                usd: 26.84,
                budget: 100.0,
                billed: true,
                admin_key: "Admin key: on".into(),
                admin_key_failing: false,
                last_read: Some(format!(
                    "Last read <t:{}:R>: $26.10 billed",
                    (now - TimeDelta::minutes(34)).timestamp()
                )),
                cache_hits: Some(0.71),
                jobs: Some(vec![
                    ("chat".into(), 19.42),
                    ("chime".into(), 3.10),
                    ("reflection".into(), 2.65),
                    ("diary".into(), 0.92),
                    ("emoji".into(), 0.75),
                ]),
            })),
            features: vec![
                FeatureView {
                    name: "chat",
                    stats: Ok(vec![Stat::new("Answers", 1301), Stat::new("Follow-ups", 2)]),
                },
                FeatureView {
                    name: "reminders",
                    stats: Ok(vec![
                        Stat::new("Pending", 5),
                        Stat::new("Sent this week", 11),
                        Stat::new("Next", format!("<t:{}:R>", (now + TimeDelta::minutes(134)).timestamp())),
                    ]),
                },
                FeatureView {
                    name: "wheel",
                    stats: Ok(vec![
                        Stat::new("Games", 1),
                        Stat::new("Open bets", 6),
                        Stat::new("Rounds played", 7),
                    ]),
                },
                FeatureView {
                    name: "memory",
                    stats: Ok(vec![
                        Stat::new("Files", 48),
                        Stat::new("Folders", 7),
                        Stat::new("Changes today", 9),
                        Stat::new("Last reflection", format!("<t:{}:R>", (now - TimeDelta::hours(9)).timestamp())),
                    ]),
                },
                FeatureView {
                    name: "snails",
                    stats: Ok(vec![
                        Stat::new("Caught this week", 14),
                        Stat::new("Links", 5302),
                        Stat::new("Pictures", 18113),
                        Stat::new("Backfill", "41/52 channels"),
                    ]),
                },
                FeatureView {
                    name: "broken",
                    stats: Err("database is locked".into()),
                },
            ],
            panels: vec![Panel {
                title: "Vivy's mind".into(),
                rows: vec![
                    Stat::new("Mood", "cozy and a little smug after winning the horror movie argument"),
                    Stat::new("Thinking about", "the wheel finale, Sam's Japan trip, whether Alien beats Aliens"),
                    Stat::new("Wants to know", "what Mira's thesis is about, and if anyone actually finished Dune"),
                    Stat::new(
                        "Next check-in",
                        format!(
                            "<t:{}:R> with Sam (2 planned): ask how the job interview went",
                            (now + TimeDelta::minutes(5 * 60 + 20)).timestamp()
                        ),
                    ),
                ],
                picture: Some(sample_face()),
            }],
            history: sample_history(now),
        }
    }

    /// A stand-in for Vivy's face: a teal square with a lighter middle.
    fn sample_face() -> String {
        use base64::Engine as _;
        let face = image::RgbaImage::from_fn(64, 64, |x, y| {
            let middle = (16..48).contains(&x) && (16..48).contains(&y);
            image::Rgba(if middle {
                [190, 240, 235, 255]
            } else {
                [64, 190, 200, 255]
            })
        });
        let mut png = Vec::new();
        face.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        format!(
            "data:image/png;base64,{}",
            base64::prelude::BASE64_STANDARD.encode(png)
        )
    }

    /// Also writes the pictures to `$STATUS_MOCKUP_DIR` when it's set, to look at them.
    #[test]
    fn draws_the_status() {
        let now = at("2026-10-10T08:40:00Z");
        let full = sample(now);
        let svg = online_svg(&full);
        assert!(svg.contains(">Vivy<") && svg.contains(">ONLINE<"));
        assert!(svg.contains(">$26.84<") && svg.contains(">of $100<"));
        assert!(svg.contains("on pace for $"));
        assert!(svg.contains(">in 2h 14m<"));
        assert!(svg.contains(">VIVY&apos;S MIND<") || svg.contains(">VIVY'S MIND<"));
        assert!(svg.contains(">in 5h 20m with Sam (2 planned): ask how the job"));
        assert!(
            svg.contains(">Couldn&apos;t load: database is locked<")
                || svg.contains("Couldn't load: database is locked")
        );
        assert!(svg.contains(">12m ago  ·  10 Oct 08:28 UTC<"));

        // A new install: no history, no spend, a fallback, no errors.
        let mut fresh = sample(now);
        fresh.history = History::new();
        fresh.spend = None;
        fresh.panels = Vec::new();
        fresh.errors = ErrorStats {
            last_hour: 0,
            last_day: 0,
            per_hour: [0; 24],
            last: None,
        };
        fresh.ai.as_mut().unwrap().fallback =
            Some("Claude is out of credit; trying again <t:1791622800:R>".into());
        let fresh_svg = online_svg(&fresh);
        assert!(fresh_svg.contains(">FALLBACK<"));
        assert!(!fresh_svg.contains("Last error"));

        let offline = offline_svg("Vivy", "0.1.0 · c61bd5c", now, TimeDelta::hours(5));
        for (name, svg) in [("online", svg), ("fresh", fresh_svg), ("offline", offline)] {
            let png = crate::util::svg::to_png(|| svg.clone()).unwrap();
            assert!(png.starts_with(b"\x89PNG"));
            if let Ok(dir) = std::env::var("STATUS_MOCKUP_DIR") {
                std::fs::write(format!("{dir}/status-{name}.png"), png).unwrap();
            }
        }
    }

    #[test]
    fn discord_markup_as_text() {
        let now = at("2026-10-10T08:00:00Z");
        let in_2h = now.timestamp() + 2 * 3600 + 5 * 60;
        assert_eq!(plain(&format!("next <t:{in_2h}:R>"), now), "next in 2h 5m");
        assert_eq!(
            plain(&format!("<t:{}:f>", now.timestamp()), now),
            "10 Oct 08:00 UTC"
        );
        assert_eq!(plain("`model` <t:x:R> <t:", now), "model <t:x:R> <t:");
        assert_eq!(
            relative(
                now - TimeDelta::days(3) - TimeDelta::hours(4) - TimeDelta::minutes(9),
                now
            ),
            "3d 4h ago"
        );
        assert_eq!(relative(now + TimeDelta::seconds(20), now), "just now");
    }

    #[test]
    fn durations() {
        assert_eq!(human_duration(TimeDelta::minutes(12)), "12m");
        assert_eq!(human_duration(TimeDelta::minutes(4 * 60 + 12)), "4h 12m");
        assert_eq!(
            human_duration(TimeDelta::minutes(3 * 1440 + 4 * 60 + 12)),
            "3d 4h 12m"
        );
        assert_eq!(human_duration(TimeDelta::seconds(-5)), "0m");
    }

    #[test]
    fn small_helpers() {
        assert_eq!(
            wrap_words("one two three four", 9, 4),
            ["one two", "three", "four"]
        );
        assert_eq!(
            wrap_words("one two three four", 9, 2),
            ["one two", "three…"]
        );
        let (start, end) = month_bounds(at("2026-12-31T23:00:00Z"));
        assert_eq!(
            (start, end),
            (at("2026-12-01T00:00:00Z"), at("2027-01-01T00:00:00Z"))
        );
        assert_eq!(title("control_panel"), "Control panel");
        assert_eq!(number(12.0), "12");
        assert_eq!(number(2.5), "2.50");
        assert_eq!(path(&[(1.0, 2.0), (3.0, 4.0)]), "M1.0,2.0 L3.0,4.0");
        // A flat line sits in the middle.
        assert_eq!(
            scale(&[(5, 1.0)], [0.0, 0.0, 10.0, 10.0], (0, 10), (1.0, 1.0)),
            [(5.0, 5.0)]
        );
    }
}
