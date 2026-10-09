//! The `memory` chat tool, and the list of memory files chat adds to each question.
//!
//! The tool takes the same arguments as Anthropic's memory tool, so a future Claude provider
//! can send `{"type": "memory_20250818", "name": "memory"}` instead of this definition and
//! route the calls here unchanged.

use chrono::Utc;
use serde_json::{Value, json};
use tracing::info;

use super::folder::{self, Command, Folder};
use super::store;
use crate::ai::ToolDef;
use crate::core::{Asker, BotCtx, Result, user_error};

/// The most files listed next to a question. The model can view the folder for the rest.
const MAX_LISTED: usize = 50;

pub fn def() -> ToolDef {
    ToolDef {
        name: "memory",
        description: "Your long-term memory: a folder of text files under /memories that stays between conversations. In a server, everyone in that server shares it; in DMs it is private to that person. \
Save what will help later: facts people share about themselves, their preferences, running jokes, decisions, and anything someone asks you to remember. Keep notes short and factual, update them when they change, and don't save secrets, passwords or things said in passing. \
Layout: /memories/server.md for the server, /memories/users/<user id>.md for each person (first line: their name). \
A person is the authority on themselves: what they say about themselves replaces what others said. When someone tells you about another person, add who said it, like \"likes horror films (per Alice)\". \
Edit an existing file with str_replace or insert instead of making a new one. \
Commands: view (a file with line numbers, or a directory), create (write a whole file), str_replace (replace text that appears once), insert (add lines after insert_line; 0 is the top), delete, rename. A file holds at most 8K.",
        parameters: json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "enum": ["view", "create", "str_replace", "insert", "delete", "rename"]
                },
                "path": {"type": "string", "description": "A path under /memories. Every command but rename."},
                "view_range": {
                    "type": "array",
                    "items": {"type": "integer"},
                    "description": "view: [first, last] line, 1-based; last -1 means to the end."
                },
                "file_text": {"type": "string", "description": "create: the whole file."},
                "old_str": {"type": "string", "description": "str_replace: text that appears exactly once."},
                "new_str": {"type": "string", "description": "str_replace: the new text; leave out to delete old_str."},
                "insert_line": {"type": "integer", "description": "insert: the line to insert after."},
                "insert_text": {"type": "string", "description": "insert: the lines to add."},
                "old_path": {"type": "string", "description": "rename: what to move."},
                "new_path": {"type": "string", "description": "rename: where to move it."}
            },
            "required": ["command"]
        }),
    }
}

/// Runs one memory command for the asker. Mistakes in the command (a missing file, text
/// that appears twice) come back as text in the same words Claude's memory tool uses.
pub async fn run(ctx: &BotCtx, asker: &Asker, args: &Value) -> Result<String> {
    // Models sometimes fill unused arguments with null or []; leave those out.
    let mut args = args.clone();
    if let Some(fields) = args.as_object_mut() {
        fields.retain(|_, v| !v.is_null() && v.as_array().is_none_or(|a| !a.is_empty()));
    }
    let command: Command = serde_json::from_value(args)
        .map_err(|err| user_error(format!("not a valid memory command: {err}")))?;
    let scope = store::scope(asker.guild.map(|g| g.get()), asker.user.get());
    let user = asker.user.get();
    let writes = command.writes();
    let log_scope = scope.clone();

    let (text, changed) = ctx
        .db
        .call(move |conn| {
            // One transaction, so two answers editing the same file can't lose an edit.
            let tx = conn.transaction()?;
            let before = store::load(&tx, &scope)?;
            let mut after: Folder = before.clone();
            let result = folder::run(&mut after, command)
                .and_then(|text| folder::check_limits(&before, &after).map(|()| text));
            let text = match result {
                Ok(text) => text,
                Err(text) => return Ok((text, Vec::new())),
            };
            let changed: Vec<String> = folder::changes(&before, &after)
                .into_iter()
                .map(|(path, _)| path.to_string())
                .collect();
            if writes {
                store::save(&tx, &scope, &before, &after, user, Utc::now().timestamp())?;
                tx.commit()?;
            }
            Ok((text, changed))
        })
        .await?;
    for path in changed {
        info!(scope = log_scope, "memory: {path} changed for <@{user}>");
    }
    Ok(text)
}

/// The list of memory files that chat adds to the question, so the model knows what it
/// remembers without the files being in every request.
pub async fn file_list(ctx: &BotCtx, asker: &Asker) -> Result<String> {
    let scope = store::scope(asker.guild.map(|g| g.get()), asker.user.get());
    let files = ctx
        .db
        .call(move |conn| Ok(store::list(conn, &scope)?))
        .await?;
    Ok(render_list(&files))
}

fn render_list(files: &[(String, usize)]) -> String {
    if files.is_empty() {
        return "<memory_files>empty</memory_files>".to_string();
    }
    let mut lines = vec!["<memory_files>".to_string()];
    for (path, size) in files.iter().take(MAX_LISTED) {
        lines.push(format!("{path} ({})", folder::human_size(*size)));
    }
    if files.len() > MAX_LISTED {
        lines.push(format!(
            "…and {} more; view /memories for all of them",
            files.len() - MAX_LISTED
        ));
    }
    lines.push("</memory_files>".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_files_with_sizes() {
        assert_eq!(render_list(&[]), "<memory_files>empty</memory_files>");
        let files = vec![
            ("/memories/server.md".to_string(), 1229),
            ("/memories/users/1.md".to_string(), 300),
        ];
        assert_eq!(
            render_list(&files),
            "<memory_files>\n/memories/server.md (1.2K)\n/memories/users/1.md (0.3K)\n</memory_files>"
        );
        let many: Vec<(String, usize)> = (0..60).map(|n| (format!("/memories/{n}"), 1)).collect();
        let text = render_list(&many);
        assert_eq!(text.lines().count(), 53);
        assert!(text.contains("…and 10 more"));
    }

    #[test]
    fn the_tool_takes_every_command() {
        let def = def();
        let commands = def.parameters["properties"]["command"]["enum"].clone();
        for name in commands.as_array().unwrap() {
            let mut args = json!({"command": name, "path": "/memories/a", "file_text": "",
                "old_str": "x", "insert_line": 0, "insert_text": "", "old_path": "/memories/a",
                "new_path": "/memories/b"});
            if name == "view" {
                args["view_range"] = json!([1, -1]);
            }
            assert!(serde_json::from_value::<Command>(args).is_ok(), "{name}");
        }
    }
}
