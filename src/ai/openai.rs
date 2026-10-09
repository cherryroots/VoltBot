//! OpenAI's Responses API, called directly over HTTP.
//!
//! One request per answer: `POST /v1/responses` with `"stream": true`. The reply is a stream
//! of server-sent events (see [`super::sse`]); this file turns the few that matter into
//! [`ChatEvent`]s. OpenAI stores every response (`"store": true`), so a follow-up only sends
//! what's new plus `previous_response_id`.
//!
//! API reference: <https://platform.openai.com/docs/api-reference/responses>

use anyhow::{Context as _, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::sse::{SseEvent, SseParser};
use super::{
    Activity, ChatEvent, ChatProvider, ChatRequest, Done, GeneratedFile, Input, Part, Role,
    ToolCall, Turn,
};

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

    /// The JSON body of a request.
    fn request_body(&self, request: &ChatRequest) -> Value {
        let (input, previous) = match &request.input {
            Input::Full(turns) => (input_items(turns), None),
            Input::After { continuation, new } => (input_items(new), Some(continuation)),
        };

        let mut tools = vec![
            json!({"type": "web_search"}),
            json!({"type": "code_interpreter", "container": {"type": "auto"}}),
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
}

/// Turns become the API's input items. Tool results are items of their own.
fn input_items(turns: &[Turn]) -> Vec<Value> {
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
                (Part::ToolResult { call_id, output }, _) => items.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output,
                })),
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
        "response.failed" => {
            let message = data["response"]["error"]["message"]
                .as_str()
                .unwrap_or("no reason given");
            bail!("OpenAI failed the response: {message}");
        }
        "error" => bail!("OpenAI error: {}", text("message")),
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
        let body = self.request_body(&request);
        let mut response = self
            .post("/responses")
            .json(&body)
            .send()
            .await
            .context("couldn't reach OpenAI")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("OpenAI answered {status}: {}", error_message(&body));
        }

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
        };
        let body = client().request_body(&request);
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
