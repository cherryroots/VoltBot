//! What a request sends: the conversation as Claude's messages, and the tools.
//!
//! Claude keeps nothing between requests, so every request carries the whole conversation.
//! Answers Claude wrote before go back exactly as it wrote them ([`Part::Native`]), thinking
//! included. The conversation then only ever grows at the end, which keeps Claude's earlier
//! thinking valid and lets the prompt cache cover everything before the new message.

use std::collections::HashMap;

use base64::Engine as _;
use serde_json::{Value, json};

use super::ContentKey;
use crate::ai::{ModelFile, NativeRound, Part, Role, ToolDef, Turn};

/// Turns become messages. `uploaded` holds the Files API IDs of pictures and files (by
/// [`ContentKey`]); anything missing from it is sent inline, or as a note when it can't be.
/// `model` picks which earlier answers can go back as they were written.
pub fn to_messages(
    turns: &[Turn],
    model: &str,
    uploaded: &HashMap<ContentKey, String>,
    code_execution: bool,
) -> Vec<Value> {
    let mut messages = Vec::new();
    for turn in turns {
        if turn.role == Role::Assistant
            && let Some(rounds) = own_rounds(turn, model)
        {
            for round in rounds {
                add_round(&mut messages, round);
            }
            continue;
        }
        let mut content = Vec::new();
        for part in &turn.parts {
            match part {
                Part::Text(text) if text.trim().is_empty() => {}
                Part::Text(text) => content.push(json!({"type": "text", "text": text})),
                Part::Image(image) => {
                    let source = match uploaded.get(&ContentKey::of(&image.data)) {
                        Some(id) => json!({"type": "file", "file_id": id}),
                        None => json!({
                            "type": "base64",
                            "media_type": image.mime,
                            "data": base64::engine::general_purpose::STANDARD.encode(&image.data),
                        }),
                    };
                    content.push(json!({"type": "image", "source": source}));
                }
                Part::File(file) => content.extend(file_blocks(file, uploaded, code_execution)),
                Part::ToolResult { call_id, output } => content.push(tool_result(call_id, output)),
                // Another provider's answer: its text parts stand in for it.
                Part::Native { .. } => {}
            }
        }
        if content.is_empty() {
            continue;
        }
        let role = match turn.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        messages.push(json!({"role": role, "content": content}));
    }
    // A conversation has to start with a message from the user, but a chain can start
    // with one of the bot's messages (a reply to something it said on its own).
    if messages.first().is_some_and(|m| m["role"] == "assistant") {
        messages.insert(
            0,
            json!({"role": "user", "content": [
                {"type": "text", "text": "[The conversation starts with your message below.]"}
            ]}),
        );
    }
    messages
}

/// The rounds of an answer Claude wrote with this model.
fn own_rounds<'a>(turn: &'a Turn, model: &str) -> Option<&'a [NativeRound]> {
    turn.parts.iter().find_map(|part| match part {
        Part::Native {
            provider,
            model: written_by,
            rounds,
        } if provider == "claude" && written_by == model && !rounds.is_empty() => {
            Some(rounds.as_slice())
        }
        _ => None,
    })
}

/// One round of an earlier answer: what Claude wrote, then what the bot's tools answered.
fn add_round(messages: &mut Vec<Value>, round: &NativeRound) {
    let content = round.output.as_array().cloned().unwrap_or_default();
    if !content.is_empty() {
        messages.push(json!({"role": "assistant", "content": content}));
    }
    if !round.results.is_empty() {
        let results: Vec<Value> = round
            .results
            .iter()
            .map(|r| tool_result(&r.call_id, &r.output))
            .collect();
        messages.push(json!({"role": "user", "content": results}));
    }
}

fn tool_result(call_id: &str, output: &str) -> Value {
    // Claude doesn't take empty results.
    let output = if output.trim().is_empty() {
        "(no output)"
    } else {
        output
    };
    json!({"type": "tool_result", "tool_use_id": call_id, "content": output})
}

/// An attached file: a note naming it, the PDF itself so Claude can read it, and a copy in
/// the code execution container so code can open it.
fn file_blocks(
    file: &ModelFile,
    uploaded: &HashMap<ContentKey, String>,
    code_execution: bool,
) -> Vec<Value> {
    let id = uploaded.get(&ContentKey::of(&file.data));
    let pdf = file.mime == "application/pdf";
    let in_container = code_execution && id.is_some();
    let note = match (in_container, pdf) {
        (true, _) => format!(
            "[{} is attached, and copied into your code execution environment]",
            file.name
        ),
        (false, true) => format!("[{} is attached]", file.name),
        (false, false) => format!(
            "[{} is attached, but you can't open this kind of file here]",
            file.name
        ),
    };
    let mut blocks = vec![json!({"type": "text", "text": note})];
    if pdf {
        let source = match id {
            Some(id) => json!({"type": "file", "file_id": id}),
            None => json!({
                "type": "base64",
                "media_type": "application/pdf",
                "data": base64::engine::general_purpose::STANDARD.encode(&file.data),
            }),
        };
        blocks.push(json!({"type": "document", "source": source, "title": file.name}));
    }
    if in_container && let Some(id) = id {
        blocks.push(json!({"type": "container_upload", "file_id": id}));
    }
    blocks
}

/// Claude's built-in tools, then the bot's, always in the same order (they're part of the
/// cached prefix). The bot's `memory` tool is sent as a normal tool, not as Claude's own
/// memory tool: that one comes with Anthropic's instruction to log task progress in memory.
pub fn tools(defs: &[ToolDef], code_execution: bool) -> Vec<Value> {
    let mut tools = vec![
        json!({"type": "web_search_20260209", "name": "web_search"}),
        json!({"type": "web_fetch_20260209", "name": "web_fetch"}),
    ];
    if code_execution {
        tools.push(json!({"type": "code_execution_20260521", "name": "code_execution"}));
    }
    for def in defs {
        tools.push(json!({
            "name": def.name,
            "description": def.description,
            "input_schema": def.parameters,
            // Arguments stream as they're written; [`super::collect`] checks they're JSON.
            "eager_input_streaming": true,
        }));
    }
    tools
}

/// The system prompt as one block, marked as the end of the part every request shares, for
/// the prompt cache.
pub fn system(prompt: &str) -> Vec<Value> {
    if prompt.trim().is_empty() {
        return Vec::new();
    }
    vec![json!({"type": "text", "text": prompt, "cache_control": {"type": "ephemeral"}})]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::ToolOutput;
    use crate::util::media::ModelImage;

    fn text(role: Role, text: &str) -> Turn {
        Turn {
            role,
            parts: vec![Part::Text(text.into())],
        }
    }

    #[test]
    fn earlier_answers_go_back_as_written() {
        let thinking = json!({"type": "thinking", "thinking": "", "signature": "sig"});
        let answer = Turn {
            role: Role::Assistant,
            parts: vec![
                Part::Text("It's noon.".into()),
                Part::Native {
                    provider: "claude".into(),
                    model: "claude-opus-5-5".into(),
                    rounds: vec![
                        NativeRound {
                            output: json!([thinking, {"type": "tool_use", "id": "t1", "name": "get_current_time", "input": {}}]),
                            results: vec![ToolOutput {
                                call_id: "t1".into(),
                                output: "12:00".into(),
                            }],
                        },
                        NativeRound {
                            output: json!([{"type": "text", "text": "It's noon."}]),
                            results: Vec::new(),
                        },
                    ],
                },
            ],
        };
        let turns = vec![
            text(Role::User, "time?"),
            answer,
            text(Role::User, "thanks"),
        ];
        let messages = to_messages(&turns, "claude-opus-5-5", &HashMap::new(), true);
        assert_eq!(
            messages,
            [
                json!({"role": "user", "content": [{"type": "text", "text": "time?"}]}),
                json!({"role": "assistant", "content": [thinking, {"type": "tool_use", "id": "t1", "name": "get_current_time", "input": {}}]}),
                json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "12:00"}]}),
                json!({"role": "assistant", "content": [{"type": "text", "text": "It's noon."}]}),
                json!({"role": "user", "content": [{"type": "text", "text": "thanks"}]}),
            ]
        );

        // Another model gets the text only.
        let messages = to_messages(&turns, "claude-sonnet-5-5", &HashMap::new(), true);
        assert_eq!(messages.len(), 3);
        assert_eq!(
            messages[1],
            json!({"role": "assistant", "content": [{"type": "text", "text": "It's noon."}]})
        );
    }

    #[test]
    fn pictures_and_files_use_uploads_when_there_are_some() {
        let picture = ModelImage {
            mime: "image/png",
            data: vec![1],
        };
        let sheet = ModelFile {
            name: "sales.xlsx".into(),
            mime: "application/vnd.ms-excel".into(),
            data: vec![2],
        };
        let pdf = ModelFile {
            name: "menu.pdf".into(),
            mime: "application/pdf".into(),
            data: vec![3],
        };
        let turns = vec![Turn {
            role: Role::User,
            parts: vec![
                Part::Image(picture),
                Part::File(sheet),
                Part::File(pdf),
                Part::Text(String::new()),
            ],
        }];
        let uploaded = HashMap::from([
            (ContentKey::of(&[1]), "file_pic".to_string()),
            (ContentKey::of(&[2]), "file_sheet".to_string()),
        ]);
        let content = &to_messages(&turns, "m", &uploaded, true)[0]["content"];
        assert_eq!(
            content,
            &json!([
                {"type": "image", "source": {"type": "file", "file_id": "file_pic"}},
                {"type": "text", "text": "[sales.xlsx is attached, and copied into your code execution environment]"},
                {"type": "container_upload", "file_id": "file_sheet"},
                {"type": "text", "text": "[menu.pdf is attached]"},
                {"type": "document", "title": "menu.pdf", "source": {"type": "base64", "media_type": "application/pdf", "data": "Aw=="}},
            ])
        );

        // Without code execution the spreadsheet can't be opened.
        let content = &to_messages(&turns, "m", &uploaded, false)[0]["content"];
        assert_eq!(
            content[1]["text"],
            "[sales.xlsx is attached, but you can't open this kind of file here]"
        );
        assert_eq!(content.as_array().unwrap().len(), 4);
    }

    #[test]
    fn starts_with_the_user() {
        let turns = vec![
            text(Role::Assistant, "I said this on my own."),
            text(Role::User, "why?"),
        ];
        let messages = to_messages(&turns, "m", &HashMap::new(), true);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
    }

    fn def(name: &'static str) -> ToolDef {
        ToolDef {
            name,
            description: "Does things.",
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    #[test]
    fn built_in_tools_come_first() {
        let defs = [def("get_current_time"), def("memory")];
        let tools = tools(&defs, true);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "web_search",
                "web_fetch",
                "code_execution",
                "get_current_time",
                "memory"
            ]
        );
        assert_eq!(tools[4]["eager_input_streaming"], true);
        assert_eq!(super::tools(&defs, false).len(), 4);

        let blocks = system("Be nice.");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["cache_control"], json!({"type": "ephemeral"}));
    }
}
