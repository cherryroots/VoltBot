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
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, FormattedFields};
use tracing_subscriber::layer::{Context, SubscriberExt as _};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Layer, filter};

use tracing_log::AsLog as _;

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
        tracing_subscriber::fmt::layer()
            .with_ansi(ansi)
            .event_format(Compact)
            .fmt_fields(CompactFields)
            .boxed()
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

    let subscriber = tracing_subscriber::registry()
        .with(stdout.with_filter(env_filter))
        .with(discord);
    tracing::subscriber::set_global_default(subscriber)?;
    log::set_boxed_logger(Box::new(LogBridge))?;
    log::set_max_level(tracing::level_filters::LevelFilter::current().as_log());

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

/// Passes lines from crates that use `log` instead of `tracing` on to `tracing`. usvg, which
/// draws the wheel picture, is left out: it warns about every character its first font lacks,
/// even when another font draws it. The wheel's renderer picks the fonts itself and warns
/// once about a character no installed font has.
struct LogBridge;

impl log::Log for LogBridge {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::max_level() && !metadata.target().starts_with("usvg")
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            let _ = tracing_log::format_trace(record);
        }
    }

    fn flush(&self) {}
}

// ---------------------------------------------------------------------------------------
// Readable lines on stdout
// ---------------------------------------------------------------------------------------

/// One short line per event:
///
/// ```text
/// 2026-10-09T06:44:13Z  INFO chat::answer message{channel=2 message_id=3}: ran a chat tool tool="x"
/// ```
///
/// Shorter than tracing's own format: the time to the second, our modules without
/// `voltbot::features::`, and the spans without `feature`, `guild` and `user` (see
/// [`CompactFields`]).
struct Compact;

impl<S, N> FormatEvent<S, N> for Compact
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let meta = event.metadata();
        let ansi = writer.has_ansi_escapes();
        let paint = |code: &'static str| if ansi { code } else { "" };
        let (dim, bold, reset) = (paint("\x1b[2m"), paint("\x1b[1m"), paint("\x1b[0m"));
        let colour = paint(match *meta.level() {
            Level::ERROR => "\x1b[31m",
            Level::WARN => "\x1b[33m",
            Level::INFO => "\x1b[32m",
            Level::DEBUG => "\x1b[34m",
            Level::TRACE => "\x1b[35m",
        });

        write!(
            writer,
            "{dim}{}{reset} {colour}{:>5}{reset} {dim}{}{reset}",
            Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
            meta.level(),
            short_target(meta.target()),
        )?;
        if let Some(scope) = ctx.event_scope() {
            for span in scope.from_root() {
                write!(writer, " {bold}{}{reset}", span.name())?;
                if let Some(fields) = span.extensions().get::<FormattedFields<N>>()
                    && !fields.is_empty()
                {
                    write!(writer, "{{{fields}}}")?;
                }
            }
        }
        write!(writer, ": ")?;
        ctx.format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

/// Writes fields as `name=value`, skipping `feature`, `guild` and `user`: the module already
/// names the feature, and the channel and message ID are enough to find the message. The
/// Discord log channel reads the spans itself, so its links still have the server.
struct CompactFields;

impl<'w> FormatFields<'w> for CompactFields {
    fn format_fields<R: RecordFields>(&self, writer: Writer<'w>, fields: R) -> fmt::Result {
        let mut visitor = CompactVisitor {
            writer,
            first: true,
            result: Ok(()),
        };
        fields.record(&mut visitor);
        visitor.result
    }
}

struct CompactVisitor<'w> {
    writer: Writer<'w>,
    first: bool,
    result: fmt::Result,
}

impl Visit for CompactVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if self.result.is_err() || matches!(field.name(), "feature" | "guild" | "user") {
            return;
        }
        let space = if self.first { "" } else { " " };
        self.first = false;
        self.result = match field.name() {
            // The log message itself, without a name.
            "message" => write!(self.writer, "{space}{value:?}"),
            name => write!(self.writer, "{space}{name}={value:?}"),
        };
    }
}

/// Our own modules without the `voltbot::` (and `features::`) in front.
fn short_target(target: &str) -> &str {
    target
        .strip_prefix("voltbot::features::")
        .or_else(|| target.strip_prefix("voltbot::"))
        .unwrap_or(target)
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
            "message_id" => self.message = Some(value),
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

    #[test]
    fn usvg_lines_are_left_out() {
        use log::Log as _;
        log::set_max_level(log::LevelFilter::Info);
        let meta = |target| {
            log::Metadata::builder()
                .target(target)
                .level(log::Level::Warn)
                .build()
        };
        assert!(!LogBridge.enabled(&meta("usvg::text::layout")));
        assert!(LogBridge.enabled(&meta("reqwest::connect")));
    }

    /// A writer that keeps what the formatter wrote, for the test below.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn compact_format() {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .event_format(Compact)
            .fmt_fields(CompactFields)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "message",
                feature = "chat",
                guild = 1u64,
                channel = 2u64,
                user = 4u64
            );
            let _entered = span.enter();
            tracing::info!(target: "voltbot::features::chat::answer", tool = "x", "ran a tool");
        });
        let output = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        let line = output.strip_suffix('\n').unwrap();
        // "2026-10-09T06:44:13Z " in front.
        assert_eq!(
            &line[21..],
            r#" INFO chat::answer message{channel=2}: ran a tool tool="x""#
        );
    }

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
