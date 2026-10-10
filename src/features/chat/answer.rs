//! Producing one answer: stream it from the model into Discord, run the tools it asks for,
//! and attach the files it makes.
//!
//! While the answer streams, the reply is edited once a second. Its last line is a small
//! status (`-# 💭 Thinking…`, `-# 🔧 Reading recent messages…`) that disappears when the
//! answer is done, or turns into `-# ⏹️ Stopped` or `-# ⚠️ Something went wrong: …`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::anyhow;
use serenity::all::{CreateAttachment, MessageId};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::ai::{
    Activity, ChatEvent, ChatProvider, ChatRequest, Done, GeneratedFile, Input, NativeRound,
    ToolCall, ToolDef, ToolOutput, next_input,
};
use crate::core::errors::is_user_error;
use crate::core::{Asker, BotCtx, Feature};
use crate::util::reply::LiveReply;
use crate::util::shorten;
use crate::util::split::{DISCORD_LIMIT, split_message};

use super::store::Written;

/// The fixed instructions. Static, so every request starts the same and hits the cache.
pub(super) const SYSTEM_PROMPT: &str = include_str!("prompt.md");
/// Room kept in the last message for the status line.
const STATUS_ROOM: usize = 100;
/// Most rounds of tool calls in one answer, so a confused model can't loop forever.
const MAX_ROUNDS: usize = 8;
/// Discord allows 10 files per message.
const MAX_FILES: usize = 10;
/// Shown when the model answered with tools or files only.
const EMPTY_ANSWER: &str = "🤷";

pub struct Job {
    pub asker: Asker,
    pub reply: LiveReply,
    pub input: Input,
    pub cancel: CancellationToken,
    /// Kept up to date with the reply's messages, so ❌ on any of them finds this answer.
    pub message_ids: Arc<Mutex<Vec<MessageId>>>,
}

pub enum End {
    Finished,
    Stopped,
    Failed(anyhow::Error),
}

pub struct Outcome {
    pub text: String,
    /// How to continue from this answer. `None` when it didn't finish.
    pub written: Option<Written>,
    pub message_ids: Vec<MessageId>,
    pub end: End,
}

/// The text of the reply so far and the status line under it.
struct Screen {
    reply: LiveReply,
    text: String,
    status: Option<String>,
    /// What Discord shows now, to skip edits that change nothing.
    shown: Vec<String>,
    message_ids: Arc<Mutex<Vec<MessageId>>>,
}

impl Screen {
    async fn show(&mut self, ctx: &BotCtx) -> anyhow::Result<()> {
        let parts = render(&self.text, self.status.as_deref());
        if parts == self.shown {
            return Ok(());
        }
        self.reply.show(&ctx.http, &parts).await?;
        self.shown = parts;
        *self.message_ids.lock().unwrap() = self.reply.message_ids();
        Ok(())
    }
}

/// The messages to show: the text split to fit, with the status as the last line.
fn render(text: &str, status: Option<&str>) -> Vec<String> {
    let mut parts = split_message(text, DISCORD_LIMIT - STATUS_ROOM);
    if let Some(status) = status {
        // Cut to the room kept for it, so the last message stays under Discord's limit.
        let line = format!("-# {}", shorten(status, STATUS_ROOM - 5));
        match parts.last_mut() {
            Some(last) => {
                last.push('\n');
                last.push_str(&line);
            }
            None => parts.push(line),
        }
    }
    parts
}

fn activity_status(activity: &Activity) -> String {
    match activity {
        Activity::Thinking => "💭 Thinking…",
        Activity::SearchingWeb => "🔎 Searching the web…",
        Activity::RunningCode => "🐍 Running code…",
        Activity::Writing => "✍️ Writing…",
    }
    .to_string()
}

/// "read_recent_messages" → "🔧 Read recent messages…"
fn tool_status(name: &str) -> String {
    let words = name.replace('_', " ");
    let mut chars = words.chars();
    let label: String = match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    };
    format!("🔧 {label}…")
}

/// Runs the whole answer. Never fails: problems end up in [`Outcome::end`] and on screen.
pub async fn run(ctx: &BotCtx, provider: &dyn ChatProvider, job: Job) -> Outcome {
    let mut screen = Screen {
        reply: job.reply,
        text: String::new(),
        status: Some(activity_status(&Activity::Thinking)),
        shown: Vec::new(),
        message_ids: job.message_ids,
    };
    let result = stream_answer(
        ctx,
        provider,
        &job.asker,
        job.input,
        &job.cancel,
        &mut screen,
    )
    .await;

    let (end, written) = match result {
        Ok(Some((done, rounds, files))) => {
            screen.status = None;
            if let Err(err) = attach_files(ctx, provider, &mut screen, &files).await {
                warn!("couldn't attach the answer's files: {err:#}");
                screen.status = Some("⚠️ Some files couldn't be attached.".into());
            }
            if screen.text.trim().is_empty() && files.is_empty() {
                screen.text = EMPTY_ANSWER.to_string();
            }
            let written = Written {
                provider: provider.name().to_string(),
                model: provider.model().to_string(),
                continuation_id: done.continuation,
                native_json: serde_json::to_string(&rounds).ok(),
            };
            (End::Finished, Some(written))
        }
        Ok(None) => {
            screen.status = Some("⏹️ Stopped".into());
            (End::Stopped, None)
        }
        Err(err) => {
            screen.status = Some(format!(
                "⚠️ Something went wrong: {}",
                shorten(&format!("{err:#}"), 150)
            ));
            (End::Failed(err), None)
        }
    };
    let end = match (screen.show(ctx).await, end) {
        (Err(err), End::Finished) => End::Failed(err.context("showing the answer")),
        (_, end) => end,
    };
    Outcome {
        text: screen.text,
        written,
        message_ids: screen.reply.message_ids(),
        end,
    }
}

type Finished = (Done, Vec<NativeRound>, Vec<GeneratedFile>);

/// Streams rounds of model output until the model stops asking for tools. Returns `None`
/// when ❌ stopped it.
async fn stream_answer(
    ctx: &BotCtx,
    provider: &dyn ChatProvider,
    asker: &Asker,
    mut input: Input,
    cancel: &CancellationToken,
    screen: &mut Screen,
) -> anyhow::Result<Option<Finished>> {
    let (tools, owners) = tools_for(ctx, asker);
    let mut rounds = Vec::new();
    let mut files = Vec::new();
    screen.show(ctx).await?;

    for _round in 0..MAX_ROUNDS {
        let request = ChatRequest {
            system: SYSTEM_PROMPT.to_string(),
            input: input.clone(),
            tools: tools.clone(),
            cache_key: format!("discord:{}", asker.channel),
            job: "chat",
        };
        let mut events = tokio::select! {
            events = provider.stream(request) => events?,
            () = cancel.cancelled() => return Ok(None),
        };

        // A new round of text starts a new paragraph.
        let mut first_delta = true;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let done = loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(None),
                _ = tick.tick() => screen.show(ctx).await?,
                event = events.recv() => match event {
                    None => return Err(anyhow!("the model's stream closed early")),
                    Some(Err(err)) => return Err(err),
                    Some(Ok(ChatEvent::TextDelta(delta))) => {
                        if first_delta && !screen.text.is_empty() && !screen.text.ends_with('\n') {
                            screen.text.push_str("\n\n");
                        }
                        first_delta = false;
                        screen.text.push_str(&delta);
                        screen.status = Some(activity_status(&Activity::Writing));
                    }
                    Some(Ok(ChatEvent::Activity(activity))) => {
                        screen.status = Some(activity_status(&activity));
                    }
                    Some(Ok(ChatEvent::Done(done))) => break done,
                },
            }
        };
        files.extend(done.files.iter().cloned());
        if done.tool_calls.is_empty() {
            rounds.push(NativeRound {
                output: done.native.clone(),
                results: Vec::new(),
            });
            return Ok(Some((done, rounds, files)));
        }

        // Run the tools, then continue the answer with their results.
        let mut results = Vec::new();
        for call in &done.tool_calls {
            screen.status = Some(tool_status(&call.name));
            screen.show(ctx).await?;
            let output = tokio::select! {
                output = run_tool(ctx, asker, &owners, call) => output,
                () = cancel.cancelled() => return Ok(None),
            };
            results.push(ToolOutput {
                call_id: call.id.clone(),
                output,
            });
        }
        screen.status = Some(activity_status(&Activity::Thinking));
        input = next_input(provider, input, &done, &results);
        rounds.push(NativeRound {
            output: done.native,
            results,
        });
    }
    Err(anyhow!(
        "the model kept calling tools ({MAX_ROUNDS} rounds)"
    ))
}

/// The tools of every feature enabled where the asker is, and which feature owns each.
pub(super) fn tools_for(
    ctx: &BotCtx,
    asker: &Asker,
) -> (Vec<ToolDef>, HashMap<&'static str, Arc<dyn Feature>>) {
    let mut tools = Vec::new();
    let mut owners = HashMap::new();
    for feature in ctx.features.iter() {
        if !ctx.gate(feature.name()).allows(asker.guild, asker.channel) {
            continue;
        }
        for tool in feature.tools() {
            owners.insert(tool.name, feature.clone());
            tools.push(tool);
        }
    }
    (tools, owners)
}

/// What the features enabled where the asker is add to the question (see
/// [`Feature::chat_context`]). A feature that fails is left out.
pub async fn context_for(ctx: &BotCtx, asker: &Asker, fresh: bool) -> Vec<String> {
    let mut texts = Vec::new();
    for feature in ctx.features.iter() {
        if !ctx.gate(feature.name()).allows(asker.guild, asker.channel) {
            continue;
        }
        match feature.chat_context(ctx, asker, fresh).await {
            Ok(Some(text)) => texts.push(text),
            Ok(None) => {}
            Err(err) => warn!(
                feature = feature.name(),
                "couldn't add chat context: {err:#}"
            ),
        }
    }
    texts
}

/// Runs one tool call. Whatever happens, the model gets text back: the result or the error.
pub(super) async fn run_tool(
    ctx: &BotCtx,
    asker: &Asker,
    owners: &HashMap<&'static str, Arc<dyn Feature>>,
    call: &ToolCall,
) -> String {
    let Some(feature) = owners.get(call.name.as_str()) else {
        return format!("Error: there is no tool named {}.", call.name);
    };
    match feature.run_tool(ctx, asker, &call.name, &call.args).await {
        Ok(output) => {
            info!(tool = call.name, "ran a chat tool");
            output
        }
        Err(err) => {
            if !is_user_error(&err) {
                warn!(tool = call.name, "chat tool failed: {err:#}");
            }
            format!("Error: {err:#}")
        }
    }
}

/// Downloads the files the model made, attaches them to the last message, and points the
/// answer's `sandbox:` links at them.
async fn attach_files(
    ctx: &BotCtx,
    provider: &dyn ChatProvider,
    screen: &mut Screen,
    files: &[GeneratedFile],
) -> anyhow::Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    // Make sure the final text is on screen first: attaching edits the last message.
    screen.show(ctx).await?;
    let mut attachments = Vec::new();
    for file in files.iter().take(MAX_FILES) {
        let data = provider.download_file(file).await?;
        attachments.push(CreateAttachment::bytes(data, file_name(&file.filename)));
    }
    let message = screen.reply.attach(&ctx.http, attachments).await?;
    let links: Vec<(String, String)> = message
        .attachments
        .iter()
        .map(|a| (a.filename.clone(), a.url.clone()))
        .collect();
    screen.text = link_files(&screen.text, &links);
    if screen.text.trim().is_empty() {
        screen.text = "Files attached.".to_string();
    }
    Ok(())
}

/// The last part of a path, never empty: "/mnt/data/plot.png" → "plot.png".
fn file_name(path: &str) -> String {
    let name = path.replace('\\', "/");
    match name.rsplit('/').next() {
        Some(last) if !last.is_empty() => last.to_string(),
        _ => "file".to_string(),
    }
}

/// Replaces `sandbox:/mnt/data/plot.png` links with the Discord link of the attached file
/// of the same name. Names that match no file, or several, are left alone.
fn link_files(text: &str, files: &[(String, String)]) -> String {
    let mut result = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("sandbox:") {
        result.push_str(&rest[..start]);
        let link = &rest[start..];
        let end = link
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | ']' | '>' | '"'))
            .unwrap_or(link.len());
        let (link, after) = link.split_at(end);
        let name = file_name(link.trim_start_matches("sandbox:"));
        let matches: Vec<&(String, String)> = files.iter().filter(|(f, _)| *f == name).collect();
        match matches.as_slice() {
            [(_, url)] => result.push_str(url),
            _ => result.push_str(link),
        }
        rest = after;
    }
    result.push_str(rest);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_goes_under_the_text() {
        assert_eq!(render("", Some("💭 Thinking…")), ["-# 💭 Thinking…"]);
        assert_eq!(render("Hi", Some("✍️ Writing…")), ["Hi\n-# ✍️ Writing…"]);
        assert_eq!(render("Hi", None), ["Hi"]);

        let long = "word ".repeat(1000);
        let parts = render(&long, Some("✍️ Writing…"));
        assert!(parts.len() > 2);
        assert!(parts.iter().all(|p| p.chars().count() <= DISCORD_LIMIT));
        // Only the last part has the status.
        assert_eq!(parts.iter().filter(|p| p.contains("-# ")).count(), 1);
        assert!(parts.last().unwrap().ends_with("-# ✍️ Writing…"));
    }

    #[test]
    fn tool_labels() {
        assert_eq!(
            tool_status("read_recent_messages"),
            "🔧 Read recent messages…"
        );
    }

    #[test]
    fn sandbox_links_point_at_attachments() {
        let files = vec![
            ("plot.png".to_string(), "https://cdn/plot.png".to_string()),
            ("a.csv".to_string(), "https://cdn/1/a.csv".to_string()),
            ("a.csv".to_string(), "https://cdn/2/a.csv".to_string()),
        ];
        assert_eq!(
            link_files(
                "See [plot](sandbox:/mnt/data/plot.png) and (sandbox:/mnt/data/a.csv).",
                &files
            ),
            "See [plot](https://cdn/plot.png) and (sandbox:/mnt/data/a.csv)."
        );
        assert_eq!(link_files("no links", &files), "no links");
        assert_eq!(file_name("sandbox:/mnt/data/"), "file");
    }
}
