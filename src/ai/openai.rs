//! OpenAI's Responses API, called directly over HTTP.
//!
//! One request per answer: `POST /v1/responses` with `"stream": true`. The reply is a stream
//! of server-sent events (see [`super::sse`]); this file turns the few that matter into
//! [`ChatEvent`]s. OpenAI stores every response (`"store": true`), so a follow-up only sends
//! what's new plus `previous_response_id`.
//!
//! Attached files are uploaded first (`POST /v1/files`). Files the code interpreter takes
//! go into its sandbox; PDFs, Office files and spreadsheets are also given to the model to
//! read (`input_file`). Small text files are already in the message text.
//!
//! API reference: <https://platform.openai.com/docs/api-reference/responses>,
//! file inputs: <https://platform.openai.com/docs/guides/file-inputs>

use std::time::Duration;

use anyhow::{Context as _, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::warn;

use super::fallback::ApiError;
use super::sse::{SseEvent, SseParser};
use super::{
    Activity, ChatEvent, ChatProvider, ChatRequest, Done, GeneratedFile, Input, ModelFile, Part,
    Role, ToolCall, Turn,
};

/// File types the model reads itself when sent as `input_file`: PDFs (text and page images),
/// Word, PowerPoint and Excel (text only; spreadsheets the first 1000 rows). CSV and other
/// text files are left out: the message text already has them when they're small.
const READABLE_FILES: &[&str] = &[
    "pdf", "doc", "docx", "odt", "rtf", "ppt", "pptx", "xls", "xlsx",
];
/// File types the code interpreter takes, from OpenAI's list.
const SANDBOX_FILES: &[&str] = &[
    "c", "cs", "cpp", "java", "py", "js", "ts", "rb", "sh", "php", "css", "html", "md", "txt",
    "tex", "csv", "json", "xml", "xlsx", "doc", "docx", "pdf", "pptx", "jpeg", "jpg", "gif", "png",
    "tar", "zip", "pkl",
];
/// OpenAI reads at most this many bytes of `input_file`s in one request.
const MAX_READABLE_BYTES: usize = 50 * 1024 * 1024;
/// Uploaded files are deleted by OpenAI after 30 days (the longest it allows). Replies
/// continue from stored responses that point at the file, so it has to outlive the chat.
const FILE_LIFETIME_SECS: u64 = 30 * 24 * 60 * 60;
/// Waits before retrying when OpenAI is busy or rate limited.
const RETRY_WAITS: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(6)];

/// The file's extension in lowercase, like `"pdf"`. Empty when it has none.
fn extension(file: &ModelFile) -> String {
    file.name
        .rsplit_once('.')
        .map_or(String::new(), |(_, ext)| ext.to_ascii_lowercase())
}

fn is_readable(file: &ModelFile) -> bool {
    READABLE_FILES.contains(&extension(file).as_str())
}

fn fits_sandbox(file: &ModelFile) -> bool {
    SANDBOX_FILES.contains(&extension(file).as_str())
}

/// `[ai.openai]` in config.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenAiConfig {
    pub model: String,
    /// "none", "low", "medium", "high" or "xhigh".
    pub reasoning_effort: String,
    /// How long answers are: "low", "medium" or "high".
    pub verbosity: String,
    /// Leave out for the account's default.
    pub service_tier: Option<String>,
}

impl Default for OpenAiConfig {
    /// voltgpt's settings.
    fn default() -> Self {
        OpenAiConfig {
            model: "gpt-6.1-sol".to_string(),
            reasoning_effort: "medium".to_string(),
            verbosity: "low".to_string(),
            service_tier: Some("fast".to_string()),
        }
    }
}

pub struct OpenAi {
    http: reqwest::Client,
    token: String,
    /// Ends in `/v1`.
    base_url: String,
    config: OpenAiConfig,
}

impl OpenAi {
    /// `base` is `OPENAI_BASE` from `.env`, for OpenAI-compatible endpoints. `/v1` is added
    /// when it's missing, like voltgpt did.
    pub fn new(
        http: reqwest::Client,
        token: String,
        base: Option<&str>,
        config: OpenAiConfig,
    ) -> OpenAi {
        let mut base_url = base
            .map(|b| b.trim().trim_end_matches('/').to_string())
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| "https://api.openai.com".to_string());
        if !base_url.ends_with("/v1") {
            base_url.push_str("/v1");
        }
        OpenAi {
            http,
            token,
            base_url,
            config,
        }
    }

    /// The JSON body of a request. `uploads` has the file ID of each attached file in the
    /// order they appear (see [`OpenAi::upload_files`]).
    fn request_body(&self, request: &ChatRequest, uploads: &[Option<String>]) -> Value {
        let turns = match &request.input {
            Input::Full(turns) => turns,
            Input::After { new, .. } => new,
        };
        let input = input_items(turns, uploads);
        let previous = match &request.input {
            Input::Full(_) => None,
            Input::After { continuation, .. } => Some(continuation),
        };

        // Files the code interpreter takes are copied into its sandbox.
        let files = turns.iter().flat_map(|t| &t.parts).filter_map(|p| match p {
            Part::File(file) => Some(file),
            _ => None,
        });
        let file_ids: Vec<&String> = files
            .zip(uploads)
            .filter(|(file, _)| fits_sandbox(file))
            .filter_map(|(_, id)| id.as_ref())
            .collect();
        let mut container = json!({"type": "auto"});
        if !file_ids.is_empty() {
            container["file_ids"] = json!(file_ids);
        }
        let mut tools = vec![
            json!({"type": "web_search"}),
            json!({"type": "code_interpreter", "container": container}),
        ];
        tools.extend(request.tools.iter().map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
                "strict": false,
            })
        }));

        let mut body = json!({
            "model": self.config.model,
            "instructions": request.system,
            "input": input,
            "tools": tools,
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "reasoning": {"effort": self.config.reasoning_effort},
            "text": {"verbosity": self.config.verbosity},
            "truncation": "auto",
            "store": true,
            "stream": true,
            "prompt_cache_key": request.cache_key,
        });
        if let Some(previous) = previous {
            body["previous_response_id"] = json!(previous);
        }
        if let Some(tier) = &self.config.service_tier {
            body["service_tier"] = json!(tier);
        }
        body
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
    }

    /// Posts a Responses request, retrying a couple of times when OpenAI is busy.
    async fn responses(&self, body: &Value) -> anyhow::Result<reqwest::Response> {
        let mut waits = RETRY_WAITS.iter();
        loop {
            let response = self
                .post("/responses")
                .json(body)
                .send()
                .await
                .context("couldn't reach OpenAI")?;
            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }
            let busy = matches!(status.as_u16(), 429 | 500 | 502 | 503);
            if busy && let Some(wait) = waits.next() {
                warn!("OpenAI answered {status}, trying again");
                tokio::time::sleep(*wait).await;
                continue;
            }
            let text = response.text().await.unwrap_or_default();
            return Err(ApiError {
                provider: "openai",
                status,
                message: error_message(&text),
            }
            .into());
        }
    }

    /// Uploads a file with the Files API and returns its ID.
    async fn upload(&self, file: &ModelFile) -> anyhow::Result<String> {
        use reqwest::multipart::{Form, Part as FormPart};

        let data = FormPart::bytes(file.data.clone())
            .file_name(file.name.clone())
            .mime_str(&file.mime)?;
        let form = Form::new()
            .text("purpose", "user_data")
            .text("expires_after[anchor]", "created_at")
            .text("expires_after[seconds]", FILE_LIFETIME_SECS.to_string())
            .part("file", data);
        let response = self
            .post("/files")
            .multipart(form)
            .send()
            .await
            .context("couldn't reach OpenAI")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("OpenAI answered {status}: {}", error_message(&body));
        }
        let body: Value = response.json().await?;
        body["id"]
            .as_str()
            .map(str::to_string)
            .context("OpenAI's upload answer has no file ID")
    }

    /// Uploads every file in the request that the model or the code interpreter can use,
    /// one at a time, in order. Other files, and files that fail to upload, are `None`.
    async fn upload_files(&self, request: &ChatRequest) -> Vec<Option<String>> {
        let turns = match &request.input {
            Input::Full(turns) => turns,
            Input::After { new, .. } => new,
        };
        let mut uploads = Vec::new();
        for part in turns.iter().flat_map(|t| &t.parts) {
            let Part::File(file) = part else { continue };
            if !is_readable(file) && !fits_sandbox(file) {
                uploads.push(None);
                continue;
            }
            match self.upload(file).await {
                Ok(id) => uploads.push(Some(id)),
                Err(err) => {
                    warn!("couldn't upload {} to OpenAI: {err:#}", file.name);
                    uploads.push(None);
                }
            }
        }
        uploads
    }
}

/// How one attached file is sent: the file to read, and a note saying where it is.
/// `readable_bytes` counts the `input_file` bytes so far, to stay under OpenAI's limit.
fn file_content(file: &ModelFile, upload: Option<&str>, readable_bytes: &mut usize) -> Vec<Value> {
    let name = &file.name;
    let Some(file_id) = upload else {
        let text = if is_readable(file) || fits_sandbox(file) {
            format!("[{name} is attached, but it couldn't be uploaded]")
        } else {
            format!("[{name} is attached, but you can't open this kind of file here]")
        };
        return vec![json!({"type": "input_text", "text": text})];
    };
    let mut content = Vec::new();
    if is_readable(file) && *readable_bytes + file.data.len() <= MAX_READABLE_BYTES {
        *readable_bytes += file.data.len();
        content.push(json!({"type": "input_file", "file_id": file_id}));
    }
    if fits_sandbox(file) {
        content.push(json!({
            "type": "input_text",
            "text": format!("[{name} is in the code interpreter's /mnt/data folder]"),
        }));
    }
    content
}

/// Turns become the API's input items. Tool results are items of their own. `uploads` is
/// what [`OpenAi::upload_files`] returned for the same turns.
fn input_items(turns: &[Turn], uploads: &[Option<String>]) -> Vec<Value> {
    let mut uploads = uploads.iter();
    let mut readable_bytes = 0;
    let mut items = Vec::new();
    for turn in turns {
        let mut content = Vec::new();
        for part in &turn.parts {
            match (part, turn.role) {
                (Part::Text(text), Role::User) => {
                    content.push(json!({"type": "input_text", "text": text}));
                }
                (Part::Text(text), Role::Assistant) => {
                    content.push(json!({"type": "output_text", "text": text}));
                }
                (Part::Image(image), _) => content.push(json!({
                    "type": "input_image",
                    "image_url": image.data_url(),
                    "detail": "auto",
                })),
                (Part::File(file), _) => {
                    let upload = uploads.next().and_then(|id| id.as_deref());
                    content.extend(file_content(file, upload, &mut readable_bytes));
                }
                (Part::ToolResult { call_id, output }, _) => items.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output,
                })),
                // OpenAI remembers its own answers; the turn's text is enough.
                (Part::Native { .. }, _) => {}
            }
        }
        if !content.is_empty() {
            let role = match turn.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            items.push(json!({"type": "message", "role": role, "content": content}));
        }
    }
    items
}

/// Turns one streamed event into a [`ChatEvent`], or `None` for the many we don't need.
fn parse_event(event: &SseEvent) -> anyhow::Result<Option<ChatEvent>> {
    let data: Value = serde_json::from_str(&event.data).context("OpenAI sent invalid JSON")?;
    let kind = data["type"].as_str().unwrap_or_default();
    let text = |key: &str| data[key].as_str().unwrap_or_default().to_string();
    Ok(match kind {
        "response.output_text.delta" | "response.refusal.delta" => {
            Some(ChatEvent::TextDelta(text("delta")))
        }
        "response.output_item.added" => match data["item"]["type"].as_str() {
            Some("reasoning") => Some(ChatEvent::Activity(Activity::Thinking)),
            Some("web_search_call") => Some(ChatEvent::Activity(Activity::SearchingWeb)),
            Some("code_interpreter_call") => Some(ChatEvent::Activity(Activity::RunningCode)),
            Some("message") => Some(ChatEvent::Activity(Activity::Writing)),
            _ => None,
        },
        // "incomplete" means it stopped early (for example at the output limit); what it
        // wrote so far is still the answer.
        "response.completed" | "response.incomplete" => {
            Some(ChatEvent::Done(parse_done(&data["response"])))
        }
        "response.failed" => return Err(stream_error(&data["response"]["error"]).into()),
        "error" => return Err(stream_error(&data).into()),
        _ => None,
    })
}

/// Reads the finished response: its ID, the tools it wants run, and the files it made.
fn parse_done(response: &Value) -> Done {
    let output = response["output"].as_array().cloned().unwrap_or_default();
    let mut done = Done {
        continuation: response["id"].as_str().map(str::to_string),
        native: Value::Array(output.clone()),
        ..Done::default()
    };
    for item in &output {
        match item["type"].as_str() {
            Some("function_call") => {
                let arguments = item["arguments"].as_str().unwrap_or("{}");
                done.tool_calls.push(ToolCall {
                    id: item["call_id"].as_str().unwrap_or_default().to_string(),
                    name: item["name"].as_str().unwrap_or_default().to_string(),
                    // Bad JSON from the model reaches the tool as a string, which then
                    // reports a clear error back to the model.
                    args: serde_json::from_str(arguments)
                        .unwrap_or_else(|_| Value::String(arguments.to_string())),
                });
            }
            Some("message") => {
                let annotations = item["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|c| c["annotations"].as_array().cloned().unwrap_or_default());
                for annotation in annotations {
                    if annotation["type"] != "container_file_citation" {
                        continue;
                    }
                    let (Some(container), Some(file)) = (
                        annotation["container_id"].as_str(),
                        annotation["file_id"].as_str(),
                    ) else {
                        continue;
                    };
                    let file = GeneratedFile {
                        location: format!("{container}/{file}"),
                        filename: annotation["filename"].as_str().unwrap_or(file).to_string(),
                    };
                    if !done.files.contains(&file) {
                        done.files.push(file);
                    }
                }
            }
            _ => {}
        }
    }
    done
}

/// An error that came in the stream, as an [`ApiError`] with the status the same error
/// has as an HTTP answer, so the fallback reads it the same way.
fn stream_error(error: &Value) -> ApiError {
    let status = match error["code"].as_str() {
        Some("server_error") => 500,
        Some("rate_limit_exceeded" | "insufficient_quota") => 429,
        _ => 400,
    };
    ApiError {
        provider: "openai",
        status: reqwest::StatusCode::from_u16(status).unwrap_or_default(),
        message: error["message"]
            .as_str()
            .unwrap_or("no reason given")
            .to_string(),
    }
}

/// The `message` of an OpenAI error body, or the whole body.
fn error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| crate::util::shorten(body, 300))
}

#[async_trait]
impl ChatProvider for OpenAi {
    fn name(&self) -> &'static str {
        "openai"
    }

    fn model(&self) -> &str {
        &self.config.model
    }

    async fn stream(
        &self,
        request: ChatRequest,
    ) -> anyhow::Result<mpsc::Receiver<anyhow::Result<ChatEvent>>> {
        let uploads = self.upload_files(&request).await;
        let body = self.request_body(&request, &uploads);
        let mut response = self.responses(&body).await?;

        let (sender, receiver) = mpsc::channel(64);
        tokio::spawn(async move {
            let mut parser = SseParser::default();
            loop {
                let chunk = match response.chunk().await {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break,
                    Err(err) => {
                        let _ = sender.send(Err(err.into())).await;
                        return;
                    }
                };
                for event in parser.push(&chunk) {
                    let parsed = parse_event(&event);
                    let finished = matches!(parsed, Ok(Some(ChatEvent::Done(_))) | Err(_));
                    if let Some(parsed) = parsed.transpose()
                        && sender.send(parsed).await.is_err()
                    {
                        // The receiver is gone (the answer was stopped): stop reading.
                        return;
                    }
                    if finished {
                        return;
                    }
                }
            }
            let _ = sender
                .send(Err(anyhow::anyhow!(
                    "OpenAI's stream ended before the answer did"
                )))
                .await;
        });
        Ok(receiver)
    }

    async fn download_file(&self, file: &GeneratedFile) -> anyhow::Result<Vec<u8>> {
        let (container, file_id) = file.location.split_once('/').context("bad file location")?;
        let response = self
            .http
            .get(format!(
                "{}/containers/{container}/files/{file_id}/content",
                self.base_url
            ))
            .bearer_auth(&self.token)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::ToolDef;
    use crate::util::media::ModelImage;

    fn client() -> OpenAi {
        OpenAi::new(
            reqwest::Client::new(),
            "key".into(),
            None,
            OpenAiConfig::default(),
        )
    }

    #[test]
    fn base_url_gets_v1() {
        let base = |b: Option<&str>| {
            OpenAi::new(
                reqwest::Client::new(),
                String::new(),
                b,
                OpenAiConfig::default(),
            )
            .base_url
        };
        assert_eq!(base(None), "https://api.openai.com/v1");
        assert_eq!(base(Some("http://local:8080/")), "http://local:8080/v1");
        assert_eq!(base(Some("http://local/v1")), "http://local/v1");
    }

    #[test]
    fn request_body() {
        let request = ChatRequest {
            system: "Be nice.".into(),
            input: Input::After {
                continuation: "resp_1".into(),
                new: vec![
                    Turn {
                        role: Role::User,
                        parts: vec![
                            Part::Text("look".into()),
                            Part::Image(ModelImage {
                                mime: "image/png",
                                data: vec![1],
                            }),
                        ],
                    },
                    Turn {
                        role: Role::User,
                        parts: vec![Part::ToolResult {
                            call_id: "call_1".into(),
                            output: "12:00".into(),
                        }],
                    },
                ],
            },
            tools: vec![ToolDef {
                name: "get_time",
                description: "Time.",
                parameters: json!({"type": "object", "properties": {}}),
            }],
            cache_key: "discord:1".into(),
            job: "chat",
        };
        let body = client().request_body(&request, &[]);
        assert_eq!(body["previous_response_id"], "resp_1");
        assert_eq!(body["instructions"], "Be nice.");
        assert_eq!(body["service_tier"], "fast");
        assert_eq!(
            body["input"],
            json!([
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "look"},
                    {"type": "input_image", "image_url": "data:image/png;base64,AQ==", "detail": "auto"},
                ]},
                {"type": "function_call_output", "call_id": "call_1", "output": "12:00"},
            ])
        );
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[2]["name"], "get_time");
    }

    #[test]
    fn files_go_to_the_model_and_the_code_interpreter() {
        let file = |name: &str, mime: &str| {
            Part::File(ModelFile {
                name: name.into(),
                mime: mime.into(),
                data: b"data".to_vec(),
            })
        };
        let request = ChatRequest {
            system: String::new(),
            input: Input::Full(vec![Turn {
                role: Role::User,
                parts: vec![
                    Part::Text("what's in these?".into()),
                    file("report.pdf", "application/pdf"),
                    file("slides.odt", "application/vnd.oasis.opendocument.text"),
                    file("data.zip", "application/zip"),
                    file("song.mp3", "audio/mpeg"),
                    file("broken.pdf", "application/pdf"),
                ],
            }]),
            tools: Vec::new(),
            cache_key: String::new(),
            job: "chat",
        };
        let uploads = [
            Some("file-1".to_string()),
            Some("file-2".to_string()),
            Some("file-3".to_string()),
            None,
            None,
        ];
        let body = client().request_body(&request, &uploads);
        let text = |text: &str| json!({"type": "input_text", "text": text});
        assert_eq!(
            body["input"][0]["content"],
            json!([
                text("what's in these?"),
                {"type": "input_file", "file_id": "file-1"},
                text("[report.pdf is in the code interpreter's /mnt/data folder]"),
                {"type": "input_file", "file_id": "file-2"},
                text("[data.zip is in the code interpreter's /mnt/data folder]"),
                text("[song.mp3 is attached, but you can't open this kind of file here]"),
                text("[broken.pdf is attached, but it couldn't be uploaded]"),
            ])
        );
        // The .odt is read, but the code interpreter doesn't take it.
        assert_eq!(
            body["tools"][1]["container"],
            json!({"type": "auto", "file_ids": ["file-1", "file-3"]})
        );
    }

    #[tokio::test]
    async fn uploads_a_file() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            // Read until the multipart body's closing boundary arrives.
            while !String::from_utf8_lossy(&request).trim_end().ends_with("--") {
                let n = socket.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..n]);
            }
            let body = r#"{"id": "file-abc"}"#;
            let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request).to_string()
        });

        let openai = OpenAi::new(
            reqwest::Client::new(),
            "key".into(),
            Some(&base),
            OpenAiConfig::default(),
        );
        let file = ModelFile {
            name: "data.csv".into(),
            mime: "text/csv".into(),
            data: b"a,b\n1,2".to_vec(),
        };
        assert_eq!(openai.upload(&file).await.unwrap(), "file-abc");
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /v1/files "), "{request}");
        assert!(request.contains("name=\"purpose\"\r\n\r\nuser_data"));
        assert!(request.contains("name=\"expires_after[seconds]\"\r\n\r\n2592000"));
        assert!(request.contains("filename=\"data.csv\""));
        assert!(request.contains("a,b\n1,2"));
    }

    fn event(data: Value) -> anyhow::Result<Option<ChatEvent>> {
        parse_event(&SseEvent {
            event: None,
            data: data.to_string(),
        })
    }

    #[test]
    fn parses_stream_events() {
        assert_eq!(
            event(json!({"type": "response.output_text.delta", "delta": "Hi"})).unwrap(),
            Some(ChatEvent::TextDelta("Hi".into()))
        );
        assert_eq!(
            event(
                json!({"type": "response.output_item.added", "item": {"type": "web_search_call"}})
            )
            .unwrap(),
            Some(ChatEvent::Activity(Activity::SearchingWeb))
        );
        assert_eq!(
            event(json!({"type": "response.in_progress"})).unwrap(),
            None
        );
        let err = event(json!({"type": "error", "message": "rate limited"})).unwrap_err();
        assert!(err.to_string().contains("rate limited"));
        let err = event(json!({"type": "response.failed", "response": {"error": {
            "code": "server_error", "message": "The server had an error"}}}))
        .unwrap_err();
        assert_eq!(
            crate::ai::fallback::outage(&err),
            Some(crate::ai::fallback::Outage::Down)
        );
    }

    #[test]
    fn parses_the_finished_response() {
        let response = json!({
            "id": "resp_9",
            "output": [
                {"type": "reasoning", "id": "rs_1"},
                {"type": "function_call", "call_id": "call_1", "name": "get_time", "arguments": "{\"zone\":\"UTC\"}"},
                {"type": "message", "content": [{"type": "output_text", "text": "Here", "annotations": [
                    {"type": "container_file_citation", "container_id": "cntr_1", "file_id": "cfile_1", "filename": "plot.png"},
                    {"type": "container_file_citation", "container_id": "cntr_1", "file_id": "cfile_1", "filename": "plot.png"},
                    {"type": "url_citation", "url": "https://example.com"},
                ]}]},
            ],
        });
        let done = parse_done(&response);
        assert_eq!(done.continuation.as_deref(), Some("resp_9"));
        assert_eq!(
            done.tool_calls,
            [ToolCall {
                id: "call_1".into(),
                name: "get_time".into(),
                args: json!({"zone": "UTC"}),
            }]
        );
        assert_eq!(
            done.files,
            [GeneratedFile {
                location: "cntr_1/cfile_1".into(),
                filename: "plot.png".into(),
            }]
        );
        assert_eq!(done.native.as_array().unwrap().len(), 3);
    }
}
