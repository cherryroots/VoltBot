//! Running a request to the end without showing it anywhere: stream the answer, run the
//! tools the model asks for, and continue until it's done. For background work like
//! chiming in or tidying memory; chat's answers stream into Discord with their own loop.

use super::{ChatEvent, ChatProvider, ChatRequest, NativeRound, ToolCall, ToolOutput, next_input};
use async_trait::async_trait;

/// Runs the tool calls of [`complete`]. Whatever happens, the model gets text back.
#[async_trait]
pub trait ToolRunner: Send + Sync {
    async fn run(&self, call: &ToolCall) -> String;
}

/// A finished answer.
#[derive(Debug, Default)]
pub struct Completed {
    /// The text of the last round: the answer after any tool calls.
    pub text: String,
    /// How to continue from this answer.
    pub continuation: Option<String>,
    /// The provider's raw output of each round, with the tools' results.
    pub rounds: Vec<NativeRound>,
}

/// Sends `request`, runs tools for at most `max_rounds` rounds, and returns the answer.
pub async fn complete(
    provider: &dyn ChatProvider,
    request: ChatRequest,
    tools: &dyn ToolRunner,
    max_rounds: usize,
) -> anyhow::Result<Completed> {
    let ChatRequest {
        system,
        mut input,
        tools: defs,
        cache_key,
    } = request;
    let mut completed = Completed::default();
    for _round in 0..max_rounds {
        let request = ChatRequest {
            system: system.clone(),
            input: input.clone(),
            tools: defs.clone(),
            cache_key: cache_key.clone(),
        };
        let mut events = provider.stream(request).await?;
        completed.text.clear();
        let done = loop {
            match events.recv().await {
                None => anyhow::bail!("the model's stream closed early"),
                Some(Err(err)) => return Err(err),
                Some(Ok(ChatEvent::TextDelta(delta))) => completed.text.push_str(&delta),
                Some(Ok(ChatEvent::Activity(_))) => {}
                Some(Ok(ChatEvent::Done(done))) => break done,
            }
        };
        completed.continuation = done.continuation.clone();
        if done.tool_calls.is_empty() {
            completed.rounds.push(NativeRound {
                output: done.native,
                results: Vec::new(),
            });
            return Ok(completed);
        }
        let mut results = Vec::new();
        for call in &done.tool_calls {
            results.push(ToolOutput {
                call_id: call.id.clone(),
                output: tools.run(call).await,
            });
        }
        input = next_input(provider, input, &done, &results);
        completed.rounds.push(NativeRound {
            output: done.native,
            results,
        });
    }
    anyhow::bail!("the model kept calling tools ({max_rounds} rounds)")
}
