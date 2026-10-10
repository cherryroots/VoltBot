//! Talking to AI models.
//!
//! Every provider (OpenAI, Claude) implements [`ChatProvider`]. The rest of the bot
//! only sees the types in this file: a conversation is a list of [`Turn`]s, and an answer
//! arrives as a stream of [`ChatEvent`]s. Discord, message splitting, the tool loop and the
//! bot's own tools live outside, so every provider reuses them.

mod complete;
pub mod openai;
mod sse;

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::util::media::ModelImage;

pub use complete::{ToolRunner, complete};
pub use openai::{OpenAi, OpenAiConfig};

/// Who said something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// One piece of a turn.
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Text(String),
    Image(ModelImage),
    /// A file someone attached that isn't a picture or video: a PDF, a spreadsheet, a zip...
    File(ModelFile),
    /// The answer to a [`ToolCall`], sent back to the model.
    ToolResult {
        call_id: String,
        output: String,
    },
    /// An answer as the provider wrote it, round by round. A provider that has no memory of
    /// its own (Claude) sends this instead of the turn's text when it wrote the answer with
    /// the same model, so its thinking comes back exactly as it was. Others ignore it.
    Native {
        provider: String,
        model: String,
        rounds: Vec<NativeRound>,
    },
}

/// A file for the model, downloaded.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelFile {
    pub name: String,
    /// Like `application/pdf`.
    pub mime: String,
    pub data: Vec<u8>,
}

/// One round of an answer: what the model wrote (the provider's own JSON), and what the
/// bot's tools answered when it asked for them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeRound {
    pub output: serde_json::Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub results: Vec<ToolOutput>,
}

impl NativeRound {
    /// Reads rounds saved as JSON. `None` for anything else, like the plain list of outputs
    /// that answers were saved with before rounds existed.
    pub fn parse_list(json: &str) -> Option<Vec<NativeRound>> {
        let value: serde_json::Value = serde_json::from_str(json).ok()?;
        if !value.as_array()?.iter().all(serde_json::Value::is_object) {
            return None;
        }
        serde_json::from_value(value).ok()
    }
}

/// What one of the bot's tools answered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub call_id: String,
    pub output: String,
}

/// One message in the conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct Turn {
    pub role: Role,
    pub parts: Vec<Part>,
}

/// A function the model may call. `parameters` is a JSON Schema object.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: serde_json::Value,
}

/// The model asking to run one of the bot's tools.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// The arguments as the model wrote them, parsed as JSON.
    pub args: serde_json::Value,
}

/// A file the model made (for example with the code interpreter).
#[derive(Debug, Clone, PartialEq)]
pub struct GeneratedFile {
    /// Provider-specific location, passed back to [`ChatProvider::download_file`].
    pub location: String,
    pub filename: String,
}

/// What the model should continue from.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// The whole conversation, oldest first.
    Full(Vec<Turn>),
    /// Only what's new since an earlier answer, which the provider still remembers. OpenAI
    /// keeps conversations on its side, so a reply only has to send the new message.
    After {
        continuation: String,
        new: Vec<Turn>,
    },
}

pub struct ChatRequest {
    /// Never changes between requests, so the provider can cache it.
    pub system: String,
    pub input: Input,
    /// The bot's own tools, always in the same order (they're part of the cached prefix).
    pub tools: Vec<ToolDef>,
    /// Requests with the same key share a cache, for example one per channel.
    pub cache_key: String,
}

/// Something the model is busy with, shown in the reply's status line.
#[derive(Debug, Clone, PartialEq)]
pub enum Activity {
    Thinking,
    SearchingWeb,
    RunningCode,
    Writing,
}

/// One step of an answer as it streams in.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatEvent {
    TextDelta(String),
    Activity(Activity),
    /// The answer is complete.
    Done(Done),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Done {
    /// Pass this as [`Input::After`] to continue the conversation (OpenAI's response ID).
    pub continuation: Option<String>,
    /// Tools the model wants run before it can finish. Empty means the answer is final.
    pub tool_calls: Vec<ToolCall>,
    pub files: Vec<GeneratedFile>,
    /// The provider's raw output for this answer, kept in the history.
    pub native: serde_json::Value,
}

#[async_trait]
pub trait ChatProvider: Send + Sync {
    /// "openai", "claude", ...
    fn name(&self) -> &'static str;
    fn model(&self) -> &str;

    /// Starts an answer. Events arrive on the returned channel until [`ChatEvent::Done`] or
    /// an error. Dropping the receiver stops the request.
    async fn stream(
        &self,
        request: ChatRequest,
    ) -> anyhow::Result<mpsc::Receiver<anyhow::Result<ChatEvent>>>;

    /// Downloads a file the model made.
    async fn download_file(&self, file: &GeneratedFile) -> anyhow::Result<Vec<u8>>;
}

/// `[ai]` in config.toml: one section per provider.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AiConfig {
    pub openai: OpenAiConfig,
}

/// The AI services the bot has, set up from `.env` and `config.toml` at startup.
#[derive(Clone, Default)]
pub struct Ai {
    /// `None` when no API key is configured; chat then explains that it's off.
    pub chat: Option<Arc<dyn ChatProvider>>,
}

/// The input for the next round, after the model asked for tools in `done` and they
/// answered `results`. OpenAI continues its stored response, so only the results are sent.
/// A provider without a continuation gets the whole conversation again, with its own
/// output of this round added before the results.
pub fn next_input(
    provider: &dyn ChatProvider,
    mut input: Input,
    done: &Done,
    results: &[ToolOutput],
) -> Input {
    let answers = Turn {
        role: Role::User,
        parts: results
            .iter()
            .map(|r| Part::ToolResult {
                call_id: r.call_id.clone(),
                output: r.output.clone(),
            })
            .collect(),
    };
    if let Some(continuation) = &done.continuation {
        return Input::After {
            continuation: continuation.clone(),
            new: vec![answers],
        };
    }
    let turns = match &mut input {
        Input::Full(turns) => turns,
        Input::After { new, .. } => new,
    };
    turns.push(Turn {
        role: Role::Assistant,
        parts: vec![Part::Native {
            provider: provider.name().to_string(),
            model: provider.model().to_string(),
            rounds: vec![NativeRound {
                output: done.native.clone(),
                results: Vec::new(),
            }],
        }],
    });
    turns.push(answers);
    input
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fake;

    #[async_trait]
    impl ChatProvider for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn model(&self) -> &str {
            "m1"
        }
        async fn stream(
            &self,
            _request: ChatRequest,
        ) -> anyhow::Result<mpsc::Receiver<anyhow::Result<ChatEvent>>> {
            unimplemented!()
        }
        async fn download_file(&self, _file: &GeneratedFile) -> anyhow::Result<Vec<u8>> {
            unimplemented!()
        }
    }

    fn question() -> Turn {
        Turn {
            role: Role::User,
            parts: vec![Part::Text("hi".into())],
        }
    }

    fn results() -> Vec<ToolOutput> {
        vec![ToolOutput {
            call_id: "c1".into(),
            output: "12:00".into(),
        }]
    }

    #[test]
    fn next_input_continues_a_stored_response() {
        let done = Done {
            continuation: Some("resp_1".into()),
            ..Done::default()
        };
        let next = next_input(&Fake, Input::Full(vec![question()]), &done, &results());
        assert_eq!(
            next,
            Input::After {
                continuation: "resp_1".into(),
                new: vec![Turn {
                    role: Role::User,
                    parts: vec![Part::ToolResult {
                        call_id: "c1".into(),
                        output: "12:00".into(),
                    }],
                }],
            }
        );
    }

    #[test]
    fn next_input_resends_everything_without_a_continuation() {
        let done = Done {
            native: json!([{"type": "tool_use", "id": "c1"}]),
            ..Done::default()
        };
        let Input::Full(turns) =
            next_input(&Fake, Input::Full(vec![question()]), &done, &results())
        else {
            panic!("expected the whole conversation");
        };
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0], question());
        assert_eq!(
            turns[1].parts,
            [Part::Native {
                provider: "fake".into(),
                model: "m1".into(),
                rounds: vec![NativeRound {
                    output: json!([{"type": "tool_use", "id": "c1"}]),
                    results: Vec::new(),
                }],
            }]
        );
        assert_eq!(turns[2].role, Role::User);
    }

    #[test]
    fn rounds_round_trip_as_json() {
        let rounds = vec![NativeRound {
            output: json!([{"type": "text", "text": "hi"}]),
            results: results(),
        }];
        let text = serde_json::to_string(&rounds).unwrap();
        assert_eq!(NativeRound::parse_list(&text), Some(rounds));
        // Answers saved before rounds were stored don't parse, and are skipped.
        assert_eq!(NativeRound::parse_list("[[{\"type\":\"message\"}]]"), None);
        assert_eq!(NativeRound::parse_list("not json"), None);
    }
}
