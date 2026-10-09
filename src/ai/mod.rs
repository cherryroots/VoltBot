//! Talking to AI models.
//!
//! Every provider (OpenAI now, Claude later) implements [`ChatProvider`]. The rest of the bot
//! only sees the types in this file: a conversation is a list of [`Turn`]s, and an answer
//! arrives as a stream of [`ChatEvent`]s. Discord, message splitting, the tool loop and the
//! bot's own tools live outside, so every provider reuses them.

pub mod openai;
mod sse;

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::util::media::ModelImage;

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
    /// The answer to a [`ToolCall`], sent back to the model.
    ToolResult {
        call_id: String,
        output: String,
    },
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
