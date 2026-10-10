//! Reading Claude's streamed answer.
//!
//! The answer arrives as content blocks (thinking, text, tool calls, results of Claude's
//! built-in tools), each opened by `content_block_start`, filled by `content_block_delta`s
//! and closed by `content_block_stop`. [`Collector`] rebuilds the blocks exactly as Claude
//! wrote them, because they go back into the conversation unchanged, and turns the events
//! that matter into [`ChatEvent`]s on the way.
//!
//! Event reference: <https://platform.claude.com/docs/en/build-with-claude/streaming>

use std::collections::HashMap;

use anyhow::bail;
use serde_json::{Value, json};
use tracing::{debug, info};

use crate::ai::fallback::ApiError;
use crate::ai::{Activity, ChatEvent, ToolCall};

#[derive(Debug, Default)]
pub struct Collector {
    /// The blocks so far.
    pub content: Vec<Value>,
    /// Where this response's blocks start in `content`: after a pause, the continuation's
    /// blocks follow the earlier ones.
    base: usize,
    /// Tool arguments as they stream in, by block index.
    partial: HashMap<usize, String>,
    /// Tool calls whose arguments weren't valid JSON: ID → what arrived.
    invalid: HashMap<String, String>,
    /// Why the response ended: "end_turn", "tool_use", "pause_turn", "max_tokens", "refusal"...
    pub stop_reason: Option<String>,
    stop_details: Value,
    /// The code execution container the response used.
    pub container: Option<String>,
    /// The model that wrote it, which differs from the one asked for after a fallback.
    pub model: Option<String>,
    pub usage: Usage,
}

/// Tokens of one response, for the log and what it cost.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Usage {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub web_searches: u64,
}

/// A finished round.
#[derive(Debug, PartialEq)]
pub struct Finished {
    /// The blocks, ready to go back into the conversation.
    pub content: Vec<Value>,
    pub tool_calls: Vec<ToolCall>,
    /// Files the code saved, by Files API ID.
    pub file_ids: Vec<String>,
}

impl Collector {
    /// Reads one event (the JSON of its `data` line).
    pub fn push(&mut self, data: &Value) -> anyhow::Result<Vec<ChatEvent>> {
        let mut events = Vec::new();
        match data["type"].as_str().unwrap_or_default() {
            "message_start" => {
                let message = &data["message"];
                self.model = message["model"].as_str().map(str::to_string);
                self.read_usage(&message["usage"]);
                self.read_container(&message["container"]);
                log_transformations(&message["input_transformations"]);
            }
            "content_block_start" => {
                let index = self.index(data);
                let block = data["content_block"].clone();
                if let Some(activity) = activity(&block) {
                    events.push(ChatEvent::Activity(activity));
                }
                if let Some(text) = block["text"].as_str().filter(|t| !t.is_empty())
                    && block["type"] == "text"
                {
                    events.push(ChatEvent::TextDelta(text.to_string()));
                }
                if matches!(block["type"].as_str(), Some("tool_use" | "server_tool_use")) {
                    self.partial.insert(index, String::new());
                }
                if self.content.len() <= index {
                    self.content.resize(index + 1, Value::Null);
                }
                self.content[index] = block;
            }
            "content_block_delta" => {
                let index = self.index(data);
                let delta = &data["delta"];
                let Some(block) = self.content.get_mut(index) else {
                    bail!("Claude sent a delta for a block it never started");
                };
                let text = |key: &str| delta[key].as_str().unwrap_or_default();
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => {
                        append(block, "text", text("text"));
                        events.push(ChatEvent::TextDelta(text("text").to_string()));
                    }
                    "thinking_delta" => append(block, "thinking", text("thinking")),
                    "signature_delta" => block["signature"] = json!(text("signature")),
                    "citations_delta" => {
                        if !block["citations"].is_array() {
                            block["citations"] = json!([]);
                        }
                        if let Some(list) = block["citations"].as_array_mut() {
                            list.push(delta["citation"].clone());
                        }
                    }
                    "input_json_delta" => {
                        if let Some(partial) = self.partial.get_mut(&index) {
                            partial.push_str(text("partial_json"));
                        }
                    }
                    other => debug!("ignoring a Claude {other} delta"),
                }
            }
            "content_block_stop" => {
                let index = self.index(data);
                if let Some(raw) = self.partial.remove(&index)
                    && let Some(block) = self.content.get_mut(index)
                {
                    // Tool arguments stream unchecked, so they may not be JSON (or may
                    // have been cut off).
                    match parse_input(&raw) {
                        Some(input) => block["input"] = input,
                        None => {
                            block["input"] = json!({});
                            let id = block["id"].as_str().unwrap_or_default().to_string();
                            self.invalid.insert(id, raw);
                        }
                    }
                }
            }
            "message_delta" => {
                let delta = &data["delta"];
                if let Some(reason) = delta["stop_reason"].as_str() {
                    self.stop_reason = Some(reason.to_string());
                }
                self.stop_details = delta["stop_details"].clone();
                self.read_container(&delta["container"]);
                self.read_usage(&data["usage"]);
                log_transformations(&data["input_transformations"]);
            }
            // An error partway through, like being overloaded. It gets the status code the
            // same error has as an HTTP answer, so the fallback reads it the same way.
            "error" => {
                let status = match data["error"]["type"].as_str() {
                    Some("overloaded_error") => 529,
                    Some("api_error") => 500,
                    _ => 400,
                };
                return Err(ApiError {
                    provider: "claude",
                    status: reqwest::StatusCode::from_u16(status).unwrap_or_default(),
                    message: data["error"]["message"]
                        .as_str()
                        .unwrap_or("no reason given")
                        .to_string(),
                }
                .into());
            }
            // "ping", "message_stop" and event types added later.
            _ => {}
        }
        Ok(events)
    }

    /// Prepares for the rest of a paused answer, which arrives as a new response.
    pub fn continue_after_pause(&mut self) {
        self.base = self.content.len();
        self.stop_reason = None;
    }

    /// Why Claude declined, like "cyber", when it did.
    pub fn refusal_category(&self) -> Option<&str> {
        self.stop_details["category"].as_str()
    }

    /// The finished round. Tool calls Claude didn't finish (it ran out of room or
    /// declined partway) aren't run, and leave the conversation, which can't hold a call
    /// without its result.
    pub fn finish(self) -> Finished {
        let complete = !matches!(self.stop_reason.as_deref(), Some("max_tokens" | "refusal"));
        let mut content = without_declined_part(self.content);
        if !complete {
            content.retain(|block| block["type"] != "tool_use");
        }
        let tool_calls = content
            .iter()
            .filter(|block| block["type"] == "tool_use")
            .map(|block| {
                let id = block["id"].as_str().unwrap_or_default().to_string();
                ToolCall {
                    name: block["name"].as_str().unwrap_or_default().to_string(),
                    // Bad JSON reaches the tool as a string, which then tells Claude.
                    args: match self.invalid.get(&id) {
                        Some(raw) => Value::String(raw.clone()),
                        None => block["input"].clone(),
                    },
                    id,
                }
            })
            .collect();
        let file_ids = content.iter().flat_map(saved_files).collect();
        Finished {
            content,
            tool_calls,
            file_ids,
        }
    }

    fn index(&self, data: &Value) -> usize {
        self.base + data["index"].as_u64().unwrap_or_default() as usize
    }

    fn read_usage(&mut self, usage: &Value) {
        let get = |key: &str| usage[key].as_u64();
        // `message_delta` counts are running totals, so the newest one wins.
        if let Some(n) = get("input_tokens") {
            self.usage.input = n;
        }
        if let Some(n) = get("cache_read_input_tokens") {
            self.usage.cache_read = n;
        }
        if let Some(n) = get("cache_creation_input_tokens") {
            self.usage.cache_write = n;
        }
        if let Some(n) = get("output_tokens") {
            self.usage.output = n;
        }
        if let Some(n) = usage["server_tool_use"]["web_search_requests"].as_u64() {
            self.usage.web_searches = n;
        }
    }

    fn read_container(&mut self, container: &Value) {
        if let Some(id) = container["id"].as_str() {
            self.container = Some(id.to_string());
        }
    }
}

fn append(block: &mut Value, key: &str, text: &str) {
    let mut joined = block[key].as_str().unwrap_or_default().to_string();
    joined.push_str(text);
    block[key] = json!(joined);
}

/// Streamed tool arguments as JSON: nothing at all means no arguments.
fn parse_input(raw: &str) -> Option<Value> {
    if raw.trim().is_empty() {
        return Some(json!({}));
    }
    serde_json::from_str::<Value>(raw)
        .ok()
        .filter(Value::is_object)
}

/// What the status line shows when a block starts.
fn activity(block: &Value) -> Option<Activity> {
    match block["type"].as_str()? {
        "thinking" | "redacted_thinking" => Some(Activity::Thinking),
        "text" => Some(Activity::Writing),
        "server_tool_use" => match block["name"].as_str()? {
            "web_search" | "web_fetch" => Some(Activity::SearchingWeb),
            _ => Some(Activity::RunningCode),
        },
        _ => None,
    }
}

/// The IDs of files code saved, from a code execution result block.
fn saved_files(block: &Value) -> Vec<String> {
    let kind = block["type"].as_str().unwrap_or_default();
    if !matches!(
        kind,
        "bash_code_execution_tool_result" | "code_execution_tool_result"
    ) {
        return Vec::new();
    }
    block["content"]["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|output| output["file_id"].as_str().map(str::to_string))
        .collect()
}

/// When the model declined partway and another model took over (a server-side fallback),
/// a `fallback` block marks the switch. Before the last one, only text and finished
/// built-in tool calls go back into the conversation; the marker itself is dropped.
fn without_declined_part(content: Vec<Value>) -> Vec<Value> {
    let Some(switch) = content.iter().rposition(|b| b["type"] == "fallback") else {
        return content;
    };
    let finished: Vec<Value> = content[..switch]
        .iter()
        .filter(|b| {
            b["type"]
                .as_str()
                .is_some_and(|t| t.ends_with("_tool_result"))
        })
        .map(|b| b["tool_use_id"].clone())
        .collect();
    content
        .into_iter()
        .enumerate()
        .filter(|(i, block)| {
            let kind = block["type"].as_str().unwrap_or_default();
            if kind == "fallback" {
                return false;
            }
            if *i > switch {
                return true;
            }
            kind == "text"
                || kind.ends_with("_tool_result")
                || (kind == "server_tool_use" && finished.contains(&block["id"]))
        })
        .map(|(_, block)| block)
        .collect()
}

/// With the preserved-thinking beta, Claude says when it couldn't use earlier thinking:
/// after the conversation changed, or when another model answered before.
fn log_transformations(list: &Value) {
    for entry in list.as_array().into_iter().flatten() {
        info!(
            "Claude left out earlier thinking at {} ({})",
            entry["path"].as_str().unwrap_or("?"),
            entry["reason"].as_str().unwrap_or("?")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_all(collector: &mut Collector, events: &[Value]) -> Vec<ChatEvent> {
        events
            .iter()
            .flat_map(|e| collector.push(e).unwrap())
            .collect()
    }

    #[test]
    fn rebuilds_the_blocks_and_finds_tool_calls() {
        let mut collector = Collector::default();
        let events = push_all(
            &mut collector,
            &[
                json!({"type": "message_start", "message": {"model": "claude-opus-5-5", "content": [], "usage": {"input_tokens": 10, "cache_read_input_tokens": 900, "cache_creation_input_tokens": 0, "output_tokens": 1}}}),
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": ""}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}}),
                json!({"type": "content_block_stop", "index": 0}),
                json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Let me "}}),
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "check."}}),
                json!({"type": "content_block_stop", "index": 1}),
                json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "t1", "name": "get_current_time", "input": {}}}),
                json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"zone\": "}}),
                json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "\"UTC\"}"}}),
                json!({"type": "content_block_stop", "index": 2}),
                json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 50, "server_tool_use": {"web_search_requests": 1}}}),
                json!({"type": "message_stop"}),
            ],
        );
        assert_eq!(
            events,
            [
                ChatEvent::Activity(Activity::Thinking),
                ChatEvent::Activity(Activity::Writing),
                ChatEvent::TextDelta("Let me ".into()),
                ChatEvent::TextDelta("check.".into()),
            ]
        );
        assert_eq!(
            collector.usage,
            Usage {
                input: 10,
                cache_read: 900,
                cache_write: 0,
                output: 50,
                web_searches: 1,
            }
        );
        let finished = collector.finish();
        assert_eq!(
            finished.content,
            [
                json!({"type": "thinking", "thinking": "", "signature": "sig"}),
                json!({"type": "text", "text": "Let me check."}),
                json!({"type": "tool_use", "id": "t1", "name": "get_current_time", "input": {"zone": "UTC"}}),
            ]
        );
        assert_eq!(
            finished.tool_calls,
            [ToolCall {
                id: "t1".into(),
                name: "get_current_time".into(),
                args: json!({"zone": "UTC"}),
            }]
        );
    }

    #[test]
    fn bad_tool_arguments_reach_the_tool_as_text() {
        let mut collector = Collector::default();
        push_all(
            &mut collector,
            &[
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t1", "name": "memory", "input": {}}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"path\": \"a\"\"}"}}),
                json!({"type": "content_block_stop", "index": 0}),
                json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
            ],
        );
        let finished = collector.finish();
        assert_eq!(finished.content[0]["input"], json!({}));
        assert_eq!(finished.tool_calls[0].args, json!("{\"path\": \"a\"\"}"));
    }

    #[test]
    fn unfinished_tool_calls_are_dropped() {
        let mut collector = Collector::default();
        push_all(
            &mut collector,
            &[
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "Sure"}}),
                json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "t1", "name": "memory", "input": {}}}),
                json!({"type": "content_block_stop", "index": 1}),
                json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"}}),
            ],
        );
        let finished = collector.finish();
        assert_eq!(finished.content, [json!({"type": "text", "text": "Sure"})]);
        assert!(finished.tool_calls.is_empty());
    }

    #[test]
    fn built_in_tools_files_and_container() {
        let mut collector = Collector::default();
        let events = push_all(
            &mut collector,
            &[
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "server_tool_use", "id": "s1", "name": "bash_code_execution", "input": {}}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"command\": \"python plot.py\"}"}}),
                json!({"type": "content_block_stop", "index": 0}),
                json!({"type": "content_block_start", "index": 1, "content_block": {"type": "bash_code_execution_tool_result", "tool_use_id": "s1", "content": {"type": "bash_code_execution_result", "stdout": "", "stderr": "", "return_code": 0, "content": [{"type": "bash_code_execution_output", "file_id": "file_plot"}]}}}),
                json!({"type": "content_block_stop", "index": 1}),
                json!({"type": "content_block_start", "index": 2, "content_block": {"type": "server_tool_use", "id": "s2", "name": "web_search", "input": {}}}),
                json!({"type": "content_block_stop", "index": 2}),
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "container": {"id": "container_1", "expires_at": "x"}}}),
            ],
        );
        assert_eq!(
            events,
            [
                ChatEvent::Activity(Activity::RunningCode),
                ChatEvent::Activity(Activity::SearchingWeb),
            ]
        );
        assert_eq!(collector.container.as_deref(), Some("container_1"));
        let finished = collector.finish();
        assert_eq!(
            finished.content[0]["input"],
            json!({"command": "python plot.py"})
        );
        assert_eq!(finished.file_ids, ["file_plot"]);
        assert!(finished.tool_calls.is_empty());
    }

    #[test]
    fn citations_are_kept() {
        let mut collector = Collector::default();
        push_all(
            &mut collector,
            &[
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "", "citations": null}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "citations_delta", "citation": {"type": "web_search_result_location", "url": "https://a"}}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Sunny"}}),
            ],
        );
        assert_eq!(
            collector.content[0],
            json!({"type": "text", "text": "Sunny", "citations": [{"type": "web_search_result_location", "url": "https://a"}]})
        );
    }

    #[test]
    fn a_paused_answer_continues_after_its_blocks() {
        let mut collector = Collector::default();
        push_all(
            &mut collector,
            &[
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "A"}}),
                json!({"type": "message_delta", "delta": {"stop_reason": "pause_turn"}}),
            ],
        );
        assert_eq!(collector.stop_reason.as_deref(), Some("pause_turn"));
        collector.continue_after_pause();
        push_all(
            &mut collector,
            &[
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "B"}}),
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
            ],
        );
        let finished = collector.finish();
        assert_eq!(
            finished.content,
            [
                json!({"type": "text", "text": "A"}),
                json!({"type": "text", "text": "B"})
            ]
        );
    }

    #[test]
    fn keeps_only_text_from_before_a_fallback() {
        let content = vec![
            json!({"type": "thinking", "thinking": "", "signature": "x"}),
            json!({"type": "text", "text": "Partial"}),
            json!({"type": "server_tool_use", "id": "s1", "name": "web_search"}),
            json!({"type": "web_search_tool_result", "tool_use_id": "s1"}),
            json!({"type": "server_tool_use", "id": "s2", "name": "web_search"}),
            json!({"type": "fallback", "from": {"model": "a"}, "to": {"model": "b"}}),
            json!({"type": "thinking", "thinking": "", "signature": "y"}),
            json!({"type": "text", "text": "Rest"}),
        ];
        let kept = without_declined_part(content.clone());
        let kept: Vec<&str> = kept.iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(
            kept,
            [
                "text",
                "server_tool_use",
                "web_search_tool_result",
                "thinking",
                "text"
            ]
        );
        // No fallback, nothing changes.
        assert_eq!(without_declined_part(content[..5].to_vec()), content[..5]);
    }

    #[test]
    fn errors_in_the_stream() {
        let mut collector = Collector::default();
        let err = collector
            .push(&json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}))
            .unwrap_err();
        assert!(err.to_string().contains("Overloaded"));
        assert_eq!(
            crate::ai::fallback::outage(&err),
            Some(crate::ai::fallback::Outage::Down)
        );
    }
}
