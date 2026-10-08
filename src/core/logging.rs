//! Logging setup: readable lines (or JSON) on stdout, plus a copy of the important lines in
//! a Discord channel.
//!
//! Log with the `tracing` macros (`info!`, `warn!`, `error!`). The dispatcher wraps every
//! handler in a span with the feature, server, channel and message, so a log line inside a
//! handler knows where it came from without anyone passing that around.
//!
//! Start and stop notices use the target `"lifecycle"`, which is always posted to Discord:
//! `info!(target: "lifecycle", "🟢 Started")`.

use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::io::IsTerminal as _;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use serenity::all::{ChannelId, CreateAllowedMentions, CreateMessage, Http};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt as _};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Layer, filter};

use super::config::LoggingConfig;

/// A log line on its way to the Discord log channel.
#[derive(Debug, Clone)]
pub struct LogLine {
    level: Level,
    lifecycle: bool,
    /// The feature name from the span, or the module that logged it.
    source: String,
    text: String,
    /// A link to the message that triggered the handler, when there is one.
    link: Option<String>,
}

/// Sets up logging. Returns the receiving end of the Discord log queue, which
/// [`spawn_discord_poster`] empties once the bot is connected. Lines logged before then wait
/// in the queue, so startup errors still reach Discord.
pub fn init(config: &LoggingConfig) -> anyhow::Result<mpsc::Receiver<LogLine>> {
    // RUST_LOG picks what goes to stdout, for example `info,voltbot::features::chat=debug`.
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let stdout = if std::env::var("LOG_FORMAT").is_ok_and(|f| f == "json") {
        tracing_subscriber::fmt::layer().json().boxed()
    } else {
        // Colours only in a terminal, not in the systemd journal.
        let ansi = std::io::stdout().is_terminal();
        tracing_subscriber::fmt::layer().with_ansi(ansi).boxed()
    };

    let min_level = Level::from_str(&config.discord_level)
        .map_err(|_| anyhow::anyhow!("unknown logging.discord_level {:?}", config.discord_level))?;
    let (sender, receiver) = mpsc::channel(1000);
    let discord = DiscordLayer { sender }.with_filter(filter::filter_fn(move |meta| {
        // The layer sees our own spans (for their fields), lifecycle lines, and anything
        // at `discord_level` or above.
        (meta.is_span() && meta.target().starts_with("voltbot"))
            || meta.target() == "lifecycle"
            || *meta.level() <= min_level
    }));

    tracing_subscriber::registry()
        .with(stdout.with_filter(env_filter))
        .with(discord)
        .try_init()?;

    // Log panics like errors, with the span they happened in.
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!(" at {}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let payload = info.payload_as_str().unwrap_or("(no message)");
        tracing::error!(target: "panic", "panicked{location}: {payload}");
    }));

    Ok(receiver)
}

// ---------------------------------------------------------------------------------------
// Error counts for the status message
// ---------------------------------------------------------------------------------------

struct ErrorLog {
    times: VecDeque<DateTime<Utc>>,
    last: Option<(DateTime<Utc>, String)>,
}

static ERRORS: Mutex<ErrorLog> = Mutex::new(ErrorLog {
    times: VecDeque::new(),
    last: None,
});

#[derive(Debug, Clone)]
pub struct ErrorStats {
    pub last_hour: usize,
    pub last_day: usize,
    pub last: Option<(DateTime<Utc>, String)>,
}

fn record_error(text: &str) {
    let now = Utc::now();
    let mut log = ERRORS.lock().unwrap_or_else(|e| e.into_inner());
    while log
        .times
        .front()
        .is_some_and(|t| now - *t > TimeDelta::days(1))
    {
        log.times.pop_front();
    }
    log.times.push_back(now);
    log.last = Some((now, text.to_string()));
}

/// Errors logged in the last hour and day, and the most recent one.
pub fn error_stats() -> ErrorStats {
    let now = Utc::now();
    let log = ERRORS.lock().unwrap_or_else(|e| e.into_inner());
    let since = |delta| log.times.iter().filter(|t| now - **t <= delta).count();
    ErrorStats {
        last_hour: since(TimeDelta::hours(1)),
        last_day: since(TimeDelta::days(1)),
        last: log.last.clone(),
    }
}

// ---------------------------------------------------------------------------------------
// The tracing layer
// ---------------------------------------------------------------------------------------

struct DiscordLayer {
    sender: mpsc::Sender<LogLine>,
}

/// The fields of a dispatcher span that a log line needs.
#[derive(Debug, Default, Clone)]
struct SpanFields {
    feature: Option<String>,
    guild: Option<String>,
    channel: Option<String>,
    message: Option<String>,
}

impl SpanFields {
    fn set(&mut self, name: &str, value: String) {
        match name {
            "feature" => self.feature = Some(value),
            "guild" => self.guild = Some(value),
            "channel" => self.channel = Some(value),
            "message" => self.message = Some(value),
            _ => {}
        }
    }

    /// Fills in what's missing from a parent span.
    fn inherit(&mut self, parent: &SpanFields) {
        self.feature = self.feature.take().or_else(|| parent.feature.clone());
        self.guild = self.guild.take().or_else(|| parent.guild.clone());
        self.channel = self.channel.take().or_else(|| parent.channel.clone());
        self.message = self.message.take().or_else(|| parent.message.clone());
    }

    fn link(&self) -> Option<String> {
        let guild = self.guild.as_deref().unwrap_or("@me");
        Some(format!(
            "https://discord.com/channels/{guild}/{}/{}",
            self.channel.as_ref()?,
            self.message.as_ref()?
        ))
    }
}

impl Visit for SpanFields {
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.set(field.name(), value.to_string());
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.set(field.name(), value.to_string());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.set(field.name(), format!("{value:?}"));
    }
}

/// Collects an event's message and its other fields into one line.
#[derive(Default)]
struct EventText {
    message: String,
    fields: String,
}

impl Visit for EventText {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }
}

impl<S> Layer<S> for DiscordLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut fields = SpanFields::default();
        attrs.record(&mut fields);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(fields);
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut text = EventText::default();
        event.record(&mut text);
        let text = text.message + &text.fields;

        if *meta.level() == Level::ERROR {
            record_error(&text);
        }

        // Walk from the innermost span outwards, collecting feature, channel and message.
        let mut fields = SpanFields::default();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope {
                if let Some(parent) = span.extensions().get::<SpanFields>() {
                    fields.inherit(parent);
                }
            }
        }

        // If the queue is full the line is dropped; stdout still has it.
        let _ = self.sender.try_send(LogLine {
            level: *meta.level(),
            lifecycle: meta.target() == "lifecycle",
            source: fields
                .feature
                .clone()
                .unwrap_or_else(|| meta.target().to_string()),
            link: fields.link(),
            text,
        });
    }
}

// ---------------------------------------------------------------------------------------
// Posting to Discord
// ---------------------------------------------------------------------------------------

/// Posts queued log lines to `channel` every few seconds until shutdown.
pub fn spawn_discord_poster(
    http: Arc<Http>,
    channel: ChannelId,
    mut receiver: mpsc::Receiver<LogLine>,
    shutdown: CancellationToken,
    tasks: &TaskTracker,
) {
    tasks.spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            let stopping = tokio::select! {
                _ = tick.tick() => false,
                _ = shutdown.cancelled() => true,
            };
            let mut lines = Vec::new();
            while let Ok(line) = receiver.try_recv() {
                lines.push(line);
            }
            for content in render(&lines) {
                let message = CreateMessage::new()
                    .content(content)
                    .allowed_mentions(CreateAllowedMentions::new());
                // Not `tracing`: a failure here would log a line that fails to post again.
                if let Err(err) = channel.send_message(&http, message).await {
                    eprintln!("could not post to the log channel: {err}");
                }
            }
            if stopping {
                break;
            }
        }
    });
}

/// Groups identical lines ("×12") and packs them into messages under Discord's limit.
fn render(lines: &[LogLine]) -> Vec<String> {
    let mut groups: Vec<(&LogLine, usize)> = Vec::new();
    for line in lines {
        let same = |(seen, _): &&mut (&LogLine, usize)| {
            seen.level == line.level && seen.source == line.source && seen.text == line.text
        };
        match groups.iter_mut().find(same) {
            Some((_, count)) => *count += 1,
            None => groups.push((line, 1)),
        }
    }

    let mut messages = Vec::new();
    let mut current = String::new();
    for (line, count) in groups {
        let mut text = if line.lifecycle {
            line.text.clone()
        } else {
            let icon = match line.level {
                Level::ERROR => "❌",
                Level::WARN => "⚠️",
                _ => "ℹ️",
            };
            format!("{icon} **{}** `{}` {}", line.level, line.source, line.text)
        };
        text = truncate(&text, 1700);
        if count > 1 {
            let _ = write!(text, " **×{count}**");
        }
        if let Some(link) = &line.link {
            let _ = write!(text, " · [message](<{link}>)");
        }

        if !current.is_empty() && current.len() + text.len() + 1 > 2000 {
            messages.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(&text);
    }
    if !current.is_empty() {
        messages.push(current);
    }
    messages
}

/// Cuts `text` to at most `max` bytes without splitting a character.
fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(level: Level, text: &str) -> LogLine {
        LogLine {
            level,
            lifecycle: false,
            source: "reminders".into(),
            text: text.into(),
            link: None,
        }
    }

    #[test]
    fn groups_repeats() {
        let lines = vec![
            line(Level::ERROR, "boom"),
            line(Level::WARN, "hmm"),
            line(Level::ERROR, "boom"),
        ];
        let messages = render(&lines);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0],
            "❌ **ERROR** `reminders` boom **×2**\n⚠️ **WARN** `reminders` hmm"
        );
    }

    #[test]
    fn splits_long_output() {
        let lines: Vec<_> = (0..10)
            .map(|i| line(Level::ERROR, &format!("{i}{}", "x".repeat(600))))
            .collect();
        let messages = render(&lines);
        assert!(messages.len() > 1);
        assert!(messages.iter().all(|m| m.len() <= 2000));
    }

    #[test]
    fn truncates_on_char_boundary() {
        assert_eq!(truncate("ééé", 3), "é…");
    }
}
