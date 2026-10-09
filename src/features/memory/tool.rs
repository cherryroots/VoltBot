//! The `memory` chat tool, and the list of memory files chat adds to each question.
//!
//! The tool takes the same arguments as Anthropic's memory tool, so a future Claude provider
//! can send `{"type": "memory_20250818", "name": "memory"}` instead of this definition and
//! route the calls here unchanged.

use chrono::Utc;
use serde_json::{Value, json};
use tracing::info;

use super::folder::{self, Command, Folder, ROOT};
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
Layout: one folder per person, /memories/users/<user id>/, with about.md (their name first, then basics) and one file per topic, like games.md or movies.md; things about the whole server go in /memories/server/<topic>.md. View a person's folder before saving about them, and add to the topic file that fits before starting a new one. \
A person is the authority on themselves: what they say about themselves replaces what others said. When someone tells you about another person, add who said it, like \"likes horror films (per Alice)\". \
Edit files with str_replace or insert instead of rewriting them. \
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
    Ok(render_list(&files, asker.user.get()))
}

/// The asker's own files and the server's files one by one, and the other people's folders
/// as one line each, so the list stays short in a big server.
fn render_list(files: &[(String, usize)], asker: u64) -> String {
    if files.is_empty() {
        return "<memory_files>empty</memory_files>".to_string();
    }
    let own = format!("{ROOT}/users/{asker}/");
    let users = format!("{ROOT}/users/");
    let mut lines = Vec::new();
    // Other people's folders: path → (files, bytes), in path order.
    let mut folders: Vec<(String, usize, usize)> = Vec::new();
    for (path, size) in files {
        let other = path
            .strip_prefix(&users)
            .filter(|_| !path.starts_with(&own))
            .and_then(|rest| rest.split_once('/'))
            .map(|(id, _)| format!("{users}{id}/"));
        match other {
            Some(dir) => match folders.last_mut() {
                Some((last, count, bytes)) if *last == dir => {
                    *count += 1;
                    *bytes += size;
                }
                _ => folders.push((dir, 1, *size)),
            },
            None => lines.push(format!("{path} ({})", folder::human_size(*size))),
        }
    }
    for (dir, count, bytes) in folders {
        let files = if count == 1 { "file" } else { "files" };
        lines.push(format!(
            "{dir} ({count} {files}, {})",
            folder::human_size(bytes)
        ));
    }
    let extra = lines.len().saturating_sub(MAX_LISTED);
    lines.truncate(MAX_LISTED);
    if extra > 0 {
        lines.push(format!("…and {extra} more; view {ROOT} for the rest"));
    }
    format!("<memory_files>\n{}\n</memory_files>", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_own_files_and_other_folders() {
        assert_eq!(render_list(&[], 1), "<memory_files>empty</memory_files>");
        let files: Vec<(String, usize)> = [
            ("/memories/server/events.md", 1229),
            ("/memories/users/1/about.md", 300),
            ("/memories/users/1/games.md", 100),
            ("/memories/users/2/about.md", 512),
            ("/memories/users/2/movies.md", 512),
            ("/memories/users/3/about.md", 50),
        ]
        .iter()
        .map(|(p, s)| (p.to_string(), *s))
        .collect();
        assert_eq!(
            render_list(&files, 1),
            "<memory_files>
/memories/server/events.md (1.2K)
/memories/users/1/about.md (0.3K)
/memories/users/1/games.md (0.1K)
/memories/users/2/ (2 files, 1.0K)
/memories/users/3/ (1 file, 0.0K)
</memory_files>"
        );
        let many: Vec<(String, usize)> = (0..60)
            .map(|n| (format!("/memories/users/{n}/about.md"), 1))
            .collect();
        let text = render_list(&many, 1000);
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
