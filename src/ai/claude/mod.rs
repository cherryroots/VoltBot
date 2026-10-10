//! Claude, through Anthropic's Messages API, called directly over HTTP.
//!
//! One request per round: `POST /v1/messages` with `"stream": true`. Claude keeps nothing
//! between requests, so each one carries the whole conversation (see `messages.rs`), and
//! the streamed answer is read back block by block (see `collect.rs`).
//!
//! What Claude gets besides the bot's own tools:
//! - web search and web fetch, run on Anthropic's side;
//! - code execution in a container, also on Anthropic's side, kept per channel so files
//!   stay around between answers, with Anthropic's skills for making office files;
//!
//! Pictures and attached files go through the Files API: uploaded once, then sent by ID.
//! The IDs are kept in SQLite (see `uploads.rs`), so they stay the same after a restart.
//! Attached files are also copied into the code execution container, and files the code
//! saves come back as Files API IDs, which [`Claude::download_file`] fetches.
//!
//! What each response cost is added to [`Spend`] (see `price.rs`).
//!
//! API reference: <https://platform.claude.com/docs/en/api/messages>

pub mod billing;
mod collect;
mod messages;
mod price;
mod uploads;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, bail};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use image::imageops::FilterType;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use self::collect::Collector;
pub use self::uploads::MIGRATIONS as UPLOAD_MIGRATIONS;
use self::uploads::Uploads;
use super::fallback::{self, ApiError};
use super::spend::{Spend, Used};
use super::sse::SseParser;
use super::{ChatEvent, ChatProvider, ChatRequest, Done, GeneratedFile, Input, Part, Turn};
use crate::util::media::ModelImage;

pub(crate) const API: &str = "https://api.anthropic.com/v1";
pub(crate) const VERSION: &str = "2023-06-01";
/// Lets a request say what to do with earlier thinking when the conversation changed
/// (`block_binding`), and reports what was left out.
const THINKING_BETA: &str = "thinking-binding-controls-2026-08-01";
/// `fallbacks: "default"`: when the model declines, the API retries on another one.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// Uploads are deleted by Anthropic after this long, so storage doesn't pile up. (The
/// Files API takes 1 hour to 90 days.)
const UPLOAD_LIFETIME: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Pictures are made smaller to fit this many pixels a side. Claude rejects bigger ones
/// once a request holds more than 20 pictures, which a long conversation can.
const MAX_IMAGE_SIDE: u32 = 2000;
/// ... and this many bytes (Claude takes up to 10 MB).
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
/// How often one answer may pause (a long run of built-in tools) and be continued.
const MAX_PAUSES: usize = 5;
/// Waits before retrying when Anthropic is busy or rate limited.
const RETRY_WAITS: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(6)];

/// `[ai.claude]` in config.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ClaudeConfig {
    pub model: String,
    /// How much it thinks: "low", "medium", "high", "xhigh" or "max".
    pub effort: String,
    /// The most tokens one round may write, thinking included.
    pub max_tokens: u32,
    /// Let it run code, which also lets it open attached files.
    pub code_execution: bool,
    /// When the model declines, let the API retry on another Claude model.
    pub fallbacks: bool,
    /// USD a month; the log channel gets a warning at 80% and 100%. 0 turns that off.
    pub monthly_budget: f64,
    /// With `ANTHROPIC_ADMIN_KEY` in `.env`, read Anthropic's bill hourly and use it in
    /// place of the estimate.
    pub billed_spend: bool,
    /// Show on the status message how much input came from the prompt cache this month.
    pub show_cache_hits: bool,
    /// Show on the status message what each job (chat, chime-ins, ...) cost this month.
    pub show_job_costs: bool,
    /// Anthropic's skills loaded into the code execution container, for making
    /// spreadsheets ("xlsx"), documents ("docx"), slides ("pptx") and PDFs ("pdf").
    pub skills: Vec<String>,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        ClaudeConfig {
            model: "claude-opus-5-5".to_string(),
            effort: "medium".to_string(),
            max_tokens: 32_000,
            code_execution: true,
            fallbacks: true,
            monthly_budget: 100.0,
            billed_spend: true,
            show_cache_hits: true,
            show_job_costs: true,
            skills: ["xlsx", "docx", "pptx", "pdf"].map(String::from).to_vec(),
        }
    }
}

/// What a picture or file is, for finding its upload again: a hash of the bytes and their
/// length.
///
/// Saved keys outlive the program (see `uploads.rs`), so the hash must never change. Rust's
/// own `DefaultHasher` may change between releases, so this uses FNV-1a (128-bit), a simple
/// hash with fixed constants: <http://www.isthe.com/chongo/tech/comp/fnv/>.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentKey(u128, usize);

impl ContentKey {
    pub fn of(data: &[u8]) -> ContentKey {
        const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
        const PRIME: u128 = 0x0000000001000000000000000000013b;
        let mut hash = OFFSET;
        for &byte in data {
            hash ^= u128::from(byte);
            hash = hash.wrapping_mul(PRIME);
        }
        ContentKey(hash, data.len())
    }

    /// The key as text, for the database: "<hash in hex>-<length>".
    fn to_text(self) -> String {
        format!("{:032x}-{}", self.0, self.1)
    }
}

pub struct Claude {
    http: reqwest::Client,
    key: String,
    config: ClaudeConfig,
    /// Files uploaded so far, saved in the database.
    uploads: Uploads,
    /// The code execution container each cache key (a channel) used last.
    containers: Arc<Mutex<HashMap<String, String>>>,
    /// Where what each response cost is added up; `None` in tests.
    spend: Option<Spend>,
}

/// What's needed to send requests from the task that reads the stream.
#[derive(Clone)]
struct Sender {
    http: reqwest::Client,
    key: String,
    betas: String,
}

impl Sender {
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{API}{path}"))
            .header("x-api-key", &self.key)
            .header("anthropic-version", VERSION)
    }

    /// Posts a Messages request, retrying a couple of times when Anthropic is busy.
    async fn messages(&self, body: &Value) -> anyhow::Result<reqwest::Response> {
        let mut waits = RETRY_WAITS.iter();
        loop {
            let request = self
                .request(reqwest::Method::POST, "/messages")
                .header("anthropic-beta", &self.betas)
                .json(body);
            let response = fallback::send(request)
                .await
                .context("couldn't reach Claude")?;
            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }
            // 429: rate limited, 529: overloaded.
            let busy = matches!(status.as_u16(), 429 | 500 | 502 | 503 | 529);
            if busy && let Some(wait) = waits.next() {
                warn!("Claude answered {status}, trying again");
                tokio::time::sleep(*wait).await;
                continue;
            }
            let text = response.text().await.unwrap_or_default();
            return Err(ApiError {
                provider: "claude",
                status,
                message: error_message(&text),
            }
            .into());
        }
    }

    /// The name of a file Claude's code saved.
    async fn file_name(&self, id: &str) -> anyhow::Result<String> {
        let response = self
            .request(reqwest::Method::GET, &format!("/files/{id}"))
            .send()
            .await?;
        if !response.status().is_success() {
            bail!("looking up file {id} failed with {}", response.status());
        }
        let metadata: Value = response.json().await?;
        Ok(metadata["filename"].as_str().unwrap_or(id).to_string())
    }
}

impl Claude {
    pub fn new(
        http: reqwest::Client,
        key: String,
        config: ClaudeConfig,
        spend: Option<Spend>,
    ) -> Claude {
        // The database comes along with the spend, which keeps it for its own tables.
        let db = spend.as_ref().map(|spend| spend.db().clone());
        Claude {
            http,
            key,
            config,
            uploads: Uploads::new(db),
            containers: Arc::new(Mutex::new(HashMap::new())),
            spend,
        }
    }

    fn sender(&self) -> Sender {
        let mut betas = vec![THINKING_BETA];
        if self.config.fallbacks {
            betas.push(FALLBACK_BETA);
        }
        Sender {
            http: self.http.clone(),
            key: self.key.clone(),
            betas: betas.join(","),
        }
    }

    /// The JSON body of a request.
    fn request_body(&self, request: &ChatRequest, conversation: Vec<Value>) -> Value {
        let config = &self.config;
        let mut body = json!({
            "model": config.model,
            "max_tokens": config.max_tokens,
            "stream": true,
            "system": messages::system(&request.system),
            "messages": conversation,
            "tools": messages::tools(&request.tools, config.code_execution),
            // Thinking can't be turned off; effort sets how much. When an earlier turn
            // changed, the thinking after it is left out instead of failing the request.
            "thinking": {
                "type": "adaptive",
                "block_binding": {"prefix_mismatch_behavior": "drop_block"},
            },
            "output_config": {"effort": config.effort},
            // Caches the conversation up to its newest message, for the next request.
            "cache_control": {"type": "ephemeral"},
        });
        if config.fallbacks {
            body["fallbacks"] = json!("default");
        }
        if config.code_execution && !config.skills.is_empty() {
            let skills: Vec<Value> = config
                .skills
                .iter()
                .map(|id| json!({"type": "anthropic", "skill_id": id, "version": "latest"}))
                .collect();
            body["container"] = json!({"skills": skills});
        }
        if let Some(id) = self.containers.lock().unwrap().get(&request.cache_key) {
            set_container(&mut body, Some(id));
        }
        body
    }

    /// Uploads the pictures and files of `turns` that aren't uploaded yet, and returns the
    /// IDs of all of them. One that fails to upload is left out and sent another way.
    async fn upload_all(&self, turns: &[Turn]) -> HashMap<ContentKey, String> {
        let mut ids = HashMap::new();
        for part in turns.iter().flat_map(|t| &t.parts) {
            let (name, mime, data) = match part {
                Part::Image(image) => (image_name(image.mime), image.mime, &image.data),
                Part::File(file) => (file.name.clone(), file.mime.as_str(), &file.data),
                _ => continue,
            };
            let key = ContentKey::of(data);
            if ids.contains_key(&key) {
                continue;
            }
            if let Some(id) = self.uploads.get(key, Utc::now()).await {
                ids.insert(key, id);
                continue;
            }
            // Big pictures are made smaller first. The upload is still found by the
            // original's bytes, so this happens once per picture.
            let smaller = match part {
                Part::Image(_) => shrink_image(data.clone()).await,
                _ => None,
            };
            let (mime, data) = match &smaller {
                Some(small) => (small.mime, &small.data),
                None => (mime, data),
            };
            match self.upload(key, &name, mime, data).await {
                Ok(id) => {
                    ids.insert(key, id);
                }
                Err(err) => warn!("couldn't upload {name} to Claude: {err:#}"),
            }
        }
        ids
    }

    async fn upload(
        &self,
        key: ContentKey,
        name: &str,
        mime: &str,
        data: &[u8],
    ) -> anyhow::Result<String> {
        let file = reqwest::multipart::Part::bytes(data.to_vec())
            .file_name(name.to_string())
            .mime_str(mime)?;
        let form = reqwest::multipart::Form::new()
            .part("file", file)
            .text("expires_in_seconds", UPLOAD_LIFETIME.as_secs().to_string());
        let response = self
            .sender()
            .request(reqwest::Method::POST, "/files")
            .multipart(form)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("uploading failed with {status}: {}", error_message(&body));
        }
        let uploaded: Value = response.json().await?;
        let id = uploaded["id"]
            .as_str()
            .context("the upload has no ID")?
            .to_string();
        debug!("uploaded {name} to Claude as {id}");
        self.uploads.put(key, &id, expires_at(&uploaded)).await;
        Ok(id)
    }

    /// Uploads what's new in `turns` and builds the request. Returns it with the upload IDs
    /// it uses.
    async fn prepare(
        &self,
        request: &ChatRequest,
        turns: &[Turn],
    ) -> (Value, HashMap<ContentKey, String>) {
        let uploaded = self.upload_all(turns).await;
        let conversation = messages::to_messages(
            turns,
            &self.config.model,
            &uploaded,
            self.config.code_execution,
        );
        (self.request_body(request, conversation), uploaded)
    }
}

/// When an upload expires, from the Files API's answer (`expires_at`, RFC 3339), or from
/// the lifetime asked for when the answer doesn't say.
fn expires_at(uploaded: &Value) -> DateTime<Utc> {
    uploaded["expires_at"]
        .as_str()
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
        .unwrap_or_else(|| Utc::now() + UPLOAD_LIFETIME)
}

/// The uploads a failed request names as gone (expired or deleted). When the error says a
/// file is missing without naming which, all of the request's uploads count, so they're all
/// uploaded again. `None` when the error isn't about a missing file.
fn missing_files(
    err: &anyhow::Error,
    uploaded: &HashMap<ContentKey, String>,
) -> Option<Vec<String>> {
    let api = err.downcast_ref::<ApiError>()?;
    let message = api.message.to_lowercase();
    let missing = ["not found", "expired", "does not exist", "no longer"]
        .iter()
        .any(|words| message.contains(words));
    if !api.status.is_client_error() || !message.contains("file") || !missing {
        return None;
    }
    let ids: Vec<String> = uploaded.values().cloned().collect();
    if ids.is_empty() {
        return None;
    }
    let named: Vec<String> = ids
        .iter()
        .filter(|id| api.message.contains(id.as_str()))
        .cloned()
        .collect();
    Some(if named.is_empty() { ids } else { named })
}

/// A copy of a picture that fits [`MAX_IMAGE_SIDE`] and [`MAX_IMAGE_BYTES`], or `None`
/// when it already does (or can't be read, in which case Claude says so).
async fn shrink_image(data: Vec<u8>) -> Option<ModelImage> {
    tokio::task::spawn_blocking(move || {
        let image = image::load_from_memory(&data).ok()?;
        let fits = image.width() <= MAX_IMAGE_SIDE && image.height() <= MAX_IMAGE_SIDE;
        if fits && data.len() <= MAX_IMAGE_BYTES {
            return None;
        }
        // `resize` fills the bounds, so a picture that's only too many bytes would grow;
        // that one is just saved again as JPEG, which is smaller.
        let smaller = if fits {
            image
        } else {
            image.resize(MAX_IMAGE_SIDE, MAX_IMAGE_SIDE, FilterType::Lanczos3)
        };
        let mut out = std::io::Cursor::new(Vec::new());
        smaller
            .to_rgb8()
            .write_to(&mut out, image::ImageFormat::Jpeg)
            .ok()?;
        Some(ModelImage {
            mime: "image/jpeg",
            data: out.into_inner(),
        })
    })
    .await
    .ok()
    .flatten()
}

/// A name for an uploaded picture, from its type.
fn image_name(mime: &str) -> String {
    let extension = mime.rsplit('/').next().unwrap_or("png");
    format!("image.{extension}")
}

/// The `message` of an API error body, or the whole body.
fn error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| crate::util::shorten(body, 300))
}

/// Sets (or with `None`, removes) the ID of the code execution container to use. The
/// container's skills stay.
fn set_container(body: &mut Value, id: Option<&str>) {
    let Some(fields) = body.as_object_mut() else {
        return;
    };
    let container = fields.entry("container").or_insert_with(|| json!({}));
    match id {
        Some(id) => container["id"] = json!(id),
        None => {
            if let Some(container) = container.as_object_mut() {
                container.remove("id");
            }
        }
    }
    if container.as_object().is_some_and(|c| c.is_empty()) {
        fields.remove("container");
    }
}

/// Whether a failed request failed because its code execution container is gone.
fn container_gone(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ApiError>().is_some_and(|e| {
        e.status.is_client_error() && e.message.to_lowercase().contains("container")
    })
}

#[async_trait]
impl ChatProvider for Claude {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn model(&self) -> &str {
        &self.config.model
    }

    fn sends_full_history(&self) -> bool {
        true
    }

    async fn stream(
        &self,
        request: ChatRequest,
    ) -> anyhow::Result<mpsc::Receiver<anyhow::Result<ChatEvent>>> {
        // Claude never hands out a continuation, so the input is always the whole
        // conversation; `After` only shows up if a caller mixed providers.
        let turns = match &request.input {
            Input::Full(turns) => turns,
            Input::After { new, .. } => new,
        };
        let (mut body, uploaded) = self.prepare(&request, turns).await;
        let sender = self.sender();
        let mut result = sender.messages(&body).await;
        // Uploads expire, and can be deleted; upload those files again.
        if let Err(err) = &result
            && let Some(gone) = missing_files(err, &uploaded)
        {
            info!("{} Claude uploads are gone, uploading again", gone.len());
            self.uploads.forget(&gone).await;
            (body, _) = self.prepare(&request, turns).await;
            result = sender.messages(&body).await;
        }
        // Containers expire; start a new one.
        if let Err(err) = &result
            && body["container"].get("id").is_some()
            && container_gone(err)
        {
            info!("the code execution container expired, starting a new one");
            self.containers.lock().unwrap().remove(&request.cache_key);
            set_container(&mut body, None);
            result = sender.messages(&body).await;
        }
        let response = result?;

        let (events, receiver) = mpsc::channel(64);
        let containers = self.containers.clone();
        let cache_key = request.cache_key;
        let spend = self.spend.clone().map(|spend| (spend, request.job));
        tokio::spawn(async move {
            let result = read_answer(&sender, spend.as_ref(), body, response, &events).await;
            let done = match result {
                Ok(Some((done, container))) => {
                    if let Some(container) = container {
                        containers.lock().unwrap().insert(cache_key, container);
                    }
                    Ok(ChatEvent::Done(done))
                }
                // The receiver is gone (the answer was stopped).
                Ok(None) => return,
                Err(err) => Err(err),
            };
            let _ = events.send(done).await;
        });
        Ok(receiver)
    }

    async fn download_file(&self, file: &GeneratedFile) -> anyhow::Result<Vec<u8>> {
        let response = self
            .sender()
            .request(
                reqwest::Method::GET,
                &format!("/files/{}/content", file.location),
            )
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!(
                "downloading {} failed with {status}: {}",
                file.filename,
                error_message(&body)
            );
        }
        Ok(response.bytes().await?.to_vec())
    }
}

/// Reads the streamed answer into `events`, continuing it when Claude pauses. Returns the
/// finished round and its container, or `None` when nobody is listening anymore.
async fn read_answer(
    sender: &Sender,
    spend: Option<&(Spend, &'static str)>,
    mut body: Value,
    mut response: reqwest::Response,
    events: &mpsc::Sender<anyhow::Result<ChatEvent>>,
) -> anyhow::Result<Option<(Done, Option<String>)>> {
    let mut collector = Collector::default();
    let mut pauses = 0;
    loop {
        let read = read_response(&mut collector, &mut response, events).await;
        // What the response used so far costs even when it was stopped or failed partway.
        add_spend(spend, &collector, &body).await;
        if !read? {
            return Ok(None);
        }

        match collector.stop_reason.as_deref() {
            // A long run of built-in tools paused; send everything back to continue.
            Some("pause_turn") if pauses < MAX_PAUSES => {
                let messages = body["messages"]
                    .as_array_mut()
                    .context("the request has no messages")?;
                // The collector holds every block of the answer so far, so later pauses
                // replace the assistant message the first one added instead of repeating it.
                if pauses > 0 {
                    messages.pop();
                }
                messages.push(json!({"role": "assistant", "content": collector.content}));
                pauses += 1;
                if let Some(id) = &collector.container {
                    set_container(&mut body, Some(id));
                }
                collector.continue_after_pause();
                response = sender.messages(&body).await?;
            }
            Some("pause_turn") => bail!("Claude kept pausing ({MAX_PAUSES} times)"),
            Some("refusal") => {
                let category = collector.refusal_category().unwrap_or("no reason given");
                bail!("Claude declined to answer this ({category})");
            }
            _ => break,
        }
    }

    let container = collector.container.clone();
    if collector.model.as_deref() != body["model"].as_str() {
        info!(
            "another model answered: {}",
            collector.model.as_deref().unwrap_or("?")
        );
    }
    let finished = collector.finish();
    let mut files = Vec::new();
    for id in finished.file_ids {
        let filename = match sender.file_name(&id).await {
            Ok(name) => name,
            Err(err) => {
                warn!("couldn't look up a file Claude made: {err:#}");
                format!("{id}.bin")
            }
        };
        files.push(GeneratedFile {
            location: id,
            filename,
        });
    }
    let done = Done {
        continuation: None,
        tool_calls: finished.tool_calls,
        files,
        native: Value::Array(finished.content),
    };
    Ok(Some((done, container)))
}

/// Reads one streamed response into `collector`, passing its events on. Returns `false`
/// when nobody is listening anymore (the answer was stopped).
async fn read_response(
    collector: &mut Collector,
    response: &mut reqwest::Response,
    events: &mpsc::Sender<anyhow::Result<ChatEvent>>,
) -> anyhow::Result<bool> {
    let mut parser = SseParser::default();
    loop {
        // A stream that sends nothing for a while ends with a timeout error (the HTTP
        // client's read timeout, see main.rs), which the fallback counts as down.
        let chunk = response
            .chunk()
            .await
            .context("reading Claude's answer failed")?;
        let Some(chunk) = chunk else {
            bail!("Claude's stream ended before the answer did");
        };
        for event in parser.push(&chunk) {
            let data: Value =
                serde_json::from_str(&event.data).context("Claude sent invalid JSON")?;
            for chat_event in collector.push(&data)? {
                if events.send(Ok(chat_event)).await.is_err() {
                    return Ok(false);
                }
            }
            if data["type"] == "message_stop" {
                return Ok(true);
            }
        }
    }
}

/// Logs what one response used and adds what it cost to [`Spend`].
async fn add_spend(spend: Option<&(Spend, &'static str)>, collector: &Collector, body: &Value) {
    let usage = collector.usage;
    // Nothing arrived (it failed before it started), so there's nothing to add.
    if usage == collect::Usage::default() {
        return;
    }
    debug!(
        "Claude used {} input tokens ({} read from cache, {} written to it) and wrote {}",
        usage.input, usage.cache_read, usage.cache_write, usage.output
    );
    if let Some((spend, job)) = spend {
        let model = collector.model.as_deref();
        let model = model.or(body["model"].as_str()).unwrap_or_default();
        let used = Used {
            usd: price::cost(&usage, model),
            input: usage.input,
            cache_read: usage.cache_read,
            cache_write: usage.cache_write,
        };
        spend.add("claude", job, used).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{Role, ToolDef};

    fn client(config: ClaudeConfig) -> Claude {
        Claude::new(reqwest::Client::new(), "key".into(), config, None)
    }

    fn request() -> ChatRequest {
        ChatRequest {
            system: "Be nice.".into(),
            input: Input::Full(vec![Turn {
                role: Role::User,
                parts: vec![Part::Text("hi".into())],
            }]),
            tools: vec![ToolDef {
                name: "get_current_time",
                description: "Time.",
                parameters: json!({"type": "object", "properties": {}}),
            }],
            cache_key: "discord:1".into(),
            job: "chat",
        }
    }

    #[test]
    fn request_body() {
        let claude = client(ClaudeConfig::default());
        let body = claude.request_body(&request(), vec![json!({"role": "user"})]);
        assert_eq!(body["model"], "claude-opus-5-5");
        assert_eq!(body["output_config"], json!({"effort": "medium"}));
        assert_eq!(
            body["thinking"]["block_binding"]["prefix_mismatch_behavior"],
            "drop_block"
        );
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(body["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(
            body["system"],
            json!([{"type": "text", "text": "Be nice.", "cache_control": {"type": "ephemeral"}}])
        );
        assert_eq!(body["tools"].as_array().unwrap().len(), 4);
        assert_eq!(body["container"]["skills"].as_array().unwrap().len(), 4);
        assert_eq!(
            body["container"]["skills"][0],
            json!({"type": "anthropic", "skill_id": "xlsx", "version": "latest"})
        );
        assert!(body["container"].get("id").is_none());
        assert_eq!(
            claude.sender().betas,
            "thinking-binding-controls-2026-08-01,server-side-fallback-2026-07-01"
        );

        // The channel's container is reused.
        claude
            .containers
            .lock()
            .unwrap()
            .insert("discord:1".into(), "container_1".into());
        let mut body = claude.request_body(&request(), Vec::new());
        assert_eq!(body["container"]["id"], "container_1");
        // An expired container is dropped; the skills stay.
        set_container(&mut body, None);
        assert!(body["container"].get("id").is_none());
        assert!(body["container"]["skills"].is_array());

        let claude = client(ClaudeConfig {
            fallbacks: false,
            code_execution: false,
            ..ClaudeConfig::default()
        });
        let mut body = claude.request_body(&request(), Vec::new());
        assert!(body.get("fallbacks").is_none());
        assert_eq!(body["tools"].as_array().unwrap().len(), 3);
        // No code execution, no skills, no container.
        assert!(body.get("container").is_none());
        set_container(&mut body, Some("container_2"));
        assert_eq!(body["container"], json!({"id": "container_2"}));
        set_container(&mut body, None);
        assert!(body.get("container").is_none());
        assert_eq!(
            claude.sender().betas,
            "thinking-binding-controls-2026-08-01"
        );
    }

    #[tokio::test]
    async fn big_pictures_are_made_smaller() {
        let encode = |width, height| {
            let mut out = std::io::Cursor::new(Vec::new());
            image::RgbImage::new(width, height)
                .write_to(&mut out, image::ImageFormat::Png)
                .unwrap();
            out.into_inner()
        };
        assert_eq!(shrink_image(encode(100, 50)).await, None);
        let smaller = shrink_image(encode(4000, 1000)).await.unwrap();
        assert_eq!(smaller.mime, "image/jpeg");
        let read = image::load_from_memory(&smaller.data).unwrap();
        assert_eq!((read.width(), read.height()), (2000, 500));
        assert_eq!(shrink_image(b"not a picture".to_vec()).await, None);

        // Too many bytes but small enough sides: saved as JPEG at the same size. Random
        // pixels keep the PNG big.
        let mut seed = 1u32;
        let noise = image::RgbImage::from_fn(1800, 1200, |_, _| {
            // xorshift, a simple random number generator.
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let [r, g, b, _] = seed.to_le_bytes();
            image::Rgb([r, g, b])
        });
        let mut png = std::io::Cursor::new(Vec::new());
        noise.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let png = png.into_inner();
        assert!(png.len() > MAX_IMAGE_BYTES);
        let smaller = shrink_image(png).await.unwrap();
        let read = image::load_from_memory(&smaller.data).unwrap();
        assert_eq!((read.width(), read.height()), (1800, 1200));
    }

    #[test]
    fn content_keys_follow_the_bytes() {
        assert_eq!(ContentKey::of(&[1, 2]), ContentKey::of(&[1, 2]));
        assert_ne!(ContentKey::of(&[1, 2]), ContentKey::of(&[2, 1]));
        assert_eq!(image_name("image/webp"), "image.webp");
    }

    #[test]
    fn missing_uploads_are_recognised() {
        let error = |status: u16, message: &str| {
            anyhow::Error::new(ApiError {
                provider: "claude",
                status: reqwest::StatusCode::from_u16(status).unwrap(),
                message: message.into(),
            })
        };
        let uploaded = HashMap::from([
            (ContentKey::of(&[1]), "file_a".to_string()),
            (ContentKey::of(&[2]), "file_b".to_string()),
        ]);
        // Named: only that one goes.
        let named = error(404, "File `file_b` not found.");
        assert_eq!(
            missing_files(&named, &uploaded),
            Some(vec!["file_b".into()])
        );
        // Not named: all of them go.
        let mut all = missing_files(&error(400, "A file has expired."), &uploaded).unwrap();
        all.sort();
        assert_eq!(all, ["file_a", "file_b"]);
        // Other errors, or nothing uploaded, change nothing.
        assert_eq!(
            missing_files(&error(400, "max_tokens is too large"), &uploaded),
            None
        );
        assert_eq!(
            missing_files(&error(400, "Container container_1 has expired"), &uploaded),
            None
        );
        assert_eq!(
            missing_files(&error(500, "file not found"), &uploaded),
            None
        );
        assert_eq!(missing_files(&named, &HashMap::new()), None);
    }

    #[test]
    fn upload_expiry_comes_from_the_answer() {
        let at = expires_at(&json!({"expires_at": "2026-11-09T12:00:00Z"}));
        assert_eq!(at.to_rfc3339(), "2026-11-09T12:00:00+00:00");
        // Without it: the lifetime asked for.
        let guessed = expires_at(&json!({"expires_at": null}));
        let days = (guessed - Utc::now()).num_days();
        assert!((29..=30).contains(&days), "{days}");
        assert_eq!(
            ContentKey(255, 3).to_text(),
            "000000000000000000000000000000ff-3"
        );
        // FNV-1a must hash the same forever: the keys are saved.
        assert_eq!(
            ContentKey::of(b"").to_text(),
            "6c62272e07bb014262b821756295c58d-0"
        );
        assert_eq!(
            ContentKey::of(b"a").to_text(),
            "d228cb696f1a8caf78912b704e4a8964-1"
        );
    }

    #[test]
    fn expired_containers_are_recognised() {
        let gone = anyhow::Error::new(ApiError {
            provider: "claude",
            status: reqwest::StatusCode::BAD_REQUEST,
            message: "Container container_1 has expired".into(),
        });
        assert!(container_gone(&gone));
        let other = anyhow::Error::new(ApiError {
            provider: "claude",
            status: reqwest::StatusCode::BAD_REQUEST,
            message: "max_tokens is too large".into(),
        });
        assert!(!container_gone(&other));
        assert!(!container_gone(&anyhow::anyhow!("container")));
    }
}
