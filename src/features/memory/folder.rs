//! The memory folder and the commands that change it. Pure: a folder is a map from paths to
//! file contents, and nothing here touches Discord or the database.
//!
//! The commands, their arguments and the texts they answer with follow Anthropic's memory
//! tool (`memory_20250818`), so the same folder can be handed to a Claude model as its
//! built-in memory: <https://platform.claude.com/docs/en/agents-and-tools/tool-use/memory-tool>.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

/// Every path starts here.
pub const ROOT: &str = "/memories";
/// The most a file can hold, in bytes.
pub const MAX_FILE: usize = 8 * 1024;
/// The most a whole folder can hold, in bytes.
pub const MAX_FOLDER: usize = 256 * 1024;
/// The longest path, in characters.
const MAX_PATH: usize = 200;

/// Files by path, like `/memories/users/123.md`. Directories aren't stored: a directory
/// exists while a file is in it, and `/memories` always exists.
pub type Folder = BTreeMap<String, String>;

/// One memory command, as the model sends it. Fields of other commands are ignored, so
/// `{"command": "view", "path": "/memories", "file_text": null}` works too.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    /// A file with line numbers, or a directory listing two levels deep.
    View {
        path: String,
        /// `[first, last]` line, 1-based; `last` may be -1 for "to the end".
        #[serde(default)]
        view_range: Option<(i64, i64)>,
    },
    /// Creates a file, or overwrites it.
    Create { path: String, file_text: String },
    /// Replaces the one place `old_str` appears. No `new_str` deletes it.
    StrReplace {
        path: String,
        old_str: String,
        #[serde(default)]
        new_str: Option<String>,
    },
    /// Inserts text after line `insert_line`; 0 inserts at the top.
    Insert {
        path: String,
        insert_line: i64,
        insert_text: String,
    },
    /// Deletes a file, or a directory and everything in it.
    Delete { path: String },
    /// Renames or moves a file or a directory.
    Rename { old_path: String, new_path: String },
}

impl Command {
    /// Whether the command can change the folder.
    pub fn writes(&self) -> bool {
        !matches!(self, Command::View { .. })
    }
}

/// Checks a path the model sent and writes it the one way the folder stores it: no
/// trailing slash, no empty, `.` or `..` parts.
pub fn clean_path(path: &str) -> Result<String, String> {
    let path = path.trim();
    let trimmed = path.trim_end_matches('/');
    if trimmed != ROOT && !trimmed.starts_with("/memories/") {
        return Err(format!(
            "Error: The path {path} is outside the memory directory. Paths start with {ROOT}."
        ));
    }
    if trimmed.chars().count() > MAX_PATH {
        return Err(format!(
            "Error: The path is too long ({MAX_PATH} characters at most)."
        ));
    }
    for part in trimmed[ROOT.len()..].split('/').skip(1) {
        let bad = part.is_empty()
            || part == "."
            || part == ".."
            || part.contains('\\')
            || part.chars().any(char::is_control);
        if bad {
            return Err(format!("Error: The path {path} is not a valid path."));
        }
    }
    Ok(trimmed.to_string())
}

fn is_file(folder: &Folder, path: &str) -> bool {
    folder.contains_key(path)
}

fn is_dir(folder: &Folder, path: &str) -> bool {
    path == ROOT || files_in(folder, path).next().is_some()
}

/// The paths of the files inside directory `dir`, at any depth.
fn files_in<'a>(folder: &'a Folder, dir: &str) -> impl Iterator<Item = &'a String> {
    let prefix = format!("{dir}/");
    folder
        .range(prefix.clone()..)
        .map(|(path, _)| path)
        .take_while(move |path| path.starts_with(&prefix))
}

/// The error when a path's parent is a file, as in `/memories/notes.md/more`.
fn parent_is_file(folder: &Folder, path: &str) -> Option<String> {
    let mut end = path.len();
    while let Some(slash) = path[..end].rfind('/') {
        let parent = &path[..slash];
        if is_file(folder, parent) {
            return Some(format!("Error: {parent} is a file, not a directory."));
        }
        end = slash;
    }
    None
}

/// Sizes like `du -h`: 512 bytes is "0.5K".
pub fn human_size(bytes: usize) -> String {
    if bytes < 1024 * 1024 {
        format!("{:.1}K", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}M", bytes as f64 / 1024.0 / 1024.0)
    }
}

/// The total size of all files.
pub fn total_size(folder: &Folder) -> usize {
    folder.values().map(String::len).sum()
}

/// Runs a command on the folder. Returns the text for the model, or the error text.
pub fn run(folder: &mut Folder, command: Command) -> Result<String, String> {
    match command {
        Command::View { path, view_range } => view(folder, &clean_path(&path)?, view_range),
        Command::Create { path, file_text } => create(folder, &clean_path(&path)?, file_text),
        Command::StrReplace {
            path,
            old_str,
            new_str,
        } => str_replace(
            folder,
            &clean_path(&path)?,
            &old_str,
            new_str.as_deref().unwrap_or(""),
        ),
        Command::Insert {
            path,
            insert_line,
            insert_text,
        } => insert(folder, &clean_path(&path)?, insert_line, &insert_text),
        Command::Delete { path } => delete(folder, &clean_path(&path)?),
        Command::Rename { old_path, new_path } => {
            rename(folder, &clean_path(&old_path)?, &clean_path(&new_path)?)
        }
    }
}

fn view(folder: &Folder, path: &str, range: Option<(i64, i64)>) -> Result<String, String> {
    if let Some(content) = folder.get(path) {
        let lines: Vec<&str> = content.lines().collect();
        let (first, last) = match range {
            None => (1, lines.len().max(1)),
            Some((first, last)) => {
                let n = lines.len() as i64;
                let last = if last == -1 { n } else { last };
                if first < 1 || first > n.max(1) || last < first || last > n {
                    return Err(format!(
                        "Error: Invalid `view_range`: [{first}, {last}]. The file has {n} lines."
                    ));
                }
                (first as usize, last as usize)
            }
        };
        let numbered = number_lines(&lines, first, last);
        return Ok(format!(
            "Here's the content of {path} with line numbers:\n{numbered}"
        ));
    }
    if is_dir(folder, path) {
        return Ok(listing(folder, path));
    }
    Err(format!(
        "The path {path} does not exist. Please provide a valid path."
    ))
}

/// Lines `first..=last` (1-based) with right-aligned line numbers and a tab.
fn number_lines(lines: &[&str], first: usize, last: usize) -> String {
    (first..=last)
        .map(|n| format!("{n:>6}\t{}", lines.get(n - 1).unwrap_or(&"")))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A directory and what's in it, two levels deep, without hidden files.
fn listing(folder: &Folder, dir: &str) -> String {
    let mut entries: BTreeSet<String> = BTreeSet::new();
    for path in files_in(folder, dir) {
        let parts: Vec<&str> = path[dir.len() + 1..].split('/').collect();
        for depth in 1..=parts.len().min(2) {
            if parts[..depth].iter().any(|p| p.starts_with('.')) {
                break;
            }
            entries.insert(format!("{dir}/{}", parts[..depth].join("/")));
        }
    }
    let size = |path: &str| match folder.get(path) {
        Some(content) => content.len(),
        None => files_in(folder, path).map(|p| folder[p].len()).sum(),
    };
    let mut lines = vec![format!(
        "Here're the files and directories up to 2 levels deep in {dir}, excluding hidden items and node_modules:"
    )];
    lines.push(format!("{}\t{dir}", human_size(size(dir))));
    for entry in &entries {
        lines.push(format!("{}\t{entry}", human_size(size(entry))));
    }
    lines.join("\n")
}

fn create(folder: &mut Folder, path: &str, text: String) -> Result<String, String> {
    if is_dir(folder, path) {
        return Err(format!("Error: {path} is a directory."));
    }
    if let Some(err) = parent_is_file(folder, path) {
        return Err(err);
    }
    // Claude's tool description says create "creates or overwrites", so it overwrites.
    folder.insert(path.to_string(), text);
    Ok(format!("File created successfully at: {path}"))
}

fn str_replace(folder: &mut Folder, path: &str, old: &str, new: &str) -> Result<String, String> {
    let Some(content) = folder.get(path) else {
        return Err(format!(
            "Error: The path {path} does not exist. Please provide a valid path."
        ));
    };
    if old.is_empty() {
        return Err("Error: old_str is empty. Give the exact text to replace.".to_string());
    }
    let found: Vec<usize> = content.match_indices(old).map(|(at, _)| at).collect();
    let line_of = |at: usize| content[..at].matches('\n').count() + 1;
    match found.as_slice() {
        [] => Err(format!(
            "No replacement was performed, old_str `{old}` did not appear verbatim in {path}."
        )),
        [at] => {
            let first_line = line_of(*at);
            let edited = format!("{}{new}{}", &content[..*at], &content[at + old.len()..]);
            let lines: Vec<&str> = edited.lines().collect();
            let from = first_line.saturating_sub(4).max(1);
            let to = (first_line + new.matches('\n').count() + 4).min(lines.len());
            let snippet = if lines.is_empty() {
                String::new()
            } else {
                number_lines(&lines, from.min(lines.len()), to.max(from.min(lines.len())))
            };
            folder.insert(path.to_string(), edited);
            Ok(format!(
                "The memory file has been edited. Here's a snippet of {path}:\n{snippet}"
            ))
        }
        many => {
            let lines: Vec<String> = many.iter().map(|at| line_of(*at).to_string()).collect();
            Err(format!(
                "No replacement was performed. Multiple occurrences of old_str `{old}` in lines: {}. Please ensure it is unique",
                lines.join(", ")
            ))
        }
    }
}

fn insert(folder: &mut Folder, path: &str, line: i64, text: &str) -> Result<String, String> {
    let Some(content) = folder.get(path) else {
        return Err(format!("Error: The path {path} does not exist"));
    };
    let mut lines: Vec<&str> = content.lines().collect();
    let n = lines.len();
    if line < 0 || line as usize > n {
        return Err(format!(
            "Error: Invalid `insert_line` parameter: {line}. It should be within the range of lines of the file: [0, {n}]"
        ));
    }
    let text = text.strip_suffix('\n').unwrap_or(text);
    let at = line as usize;
    lines.splice(at..at, text.split('\n'));
    let mut edited = lines.join("\n");
    if content.is_empty() || content.ends_with('\n') {
        edited.push('\n');
    }
    folder.insert(path.to_string(), edited);
    Ok(format!("The file {path} has been edited."))
}

fn delete(folder: &mut Folder, path: &str) -> Result<String, String> {
    if path == ROOT {
        return Err(format!(
            "Error: The {ROOT} directory itself can't be deleted."
        ));
    }
    if folder.remove(path).is_none() {
        let inside: Vec<String> = files_in(folder, path).cloned().collect();
        if inside.is_empty() {
            return Err(format!("Error: The path {path} does not exist"));
        }
        for file in inside {
            folder.remove(&file);
        }
    }
    Ok(format!("Successfully deleted {path}"))
}

fn rename(folder: &mut Folder, old: &str, new: &str) -> Result<String, String> {
    if old == ROOT {
        return Err(format!(
            "Error: The {ROOT} directory itself can't be renamed."
        ));
    }
    if !is_file(folder, old) && !is_dir(folder, old) {
        return Err(format!("Error: The path {old} does not exist"));
    }
    if is_file(folder, new) || is_dir(folder, new) {
        return Err(format!("Error: The destination {new} already exists"));
    }
    if new.starts_with(&format!("{old}/")) {
        return Err(format!("Error: {old} can't be moved into itself."));
    }
    if let Some(err) = parent_is_file(folder, new) {
        return Err(err);
    }
    if let Some(content) = folder.remove(old) {
        folder.insert(new.to_string(), content);
    } else {
        let inside: Vec<String> = files_in(folder, old).cloned().collect();
        for file in inside {
            let content = folder.remove(&file).unwrap_or_default();
            folder.insert(format!("{new}{}", &file[old.len()..]), content);
        }
    }
    Ok(format!("Successfully renamed {old} to {new}"))
}

/// Checks the size limits after a command. `before` is the folder before it ran; a command
/// that only shrinks an oversized folder is allowed.
pub fn check_limits(before: &Folder, after: &Folder) -> Result<(), String> {
    for (path, content) in after {
        if content.len() > MAX_FILE && before.get(path) != Some(content) {
            return Err(format!(
                "Error: A memory file holds at most {}; {path} would be {}. Keep it shorter or split it.",
                human_size(MAX_FILE),
                human_size(content.len())
            ));
        }
    }
    let (old, new) = (total_size(before), total_size(after));
    if new > MAX_FOLDER && new > old {
        return Err(format!(
            "Error: The memory folder is full ({} of {}). Delete or shorten files that are no longer useful first.",
            human_size(old),
            human_size(MAX_FOLDER)
        ));
    }
    Ok(())
}

/// What a command changed: files written (new or edited) and files removed.
pub fn changes<'a>(before: &'a Folder, after: &'a Folder) -> Vec<(&'a str, Option<&'a str>)> {
    let mut changed = Vec::new();
    for (path, content) in after {
        if before.get(path) != Some(content) {
            changed.push((path.as_str(), Some(content.as_str())));
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            changed.push((path.as_str(), None));
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn folder(files: &[(&str, &str)]) -> Folder {
        files
            .iter()
            .map(|(p, c)| (p.to_string(), c.to_string()))
            .collect()
    }

    fn cmd(value: serde_json::Value) -> Command {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn paths_stay_inside_the_folder() {
        assert_eq!(clean_path("/memories/").unwrap(), "/memories");
        assert_eq!(
            clean_path(" /memories/a/b.md ").unwrap(),
            "/memories/a/b.md"
        );
        for bad in [
            "/",
            "memories/a",
            "/memoriesx/a",
            "/etc/passwd",
            "/memories/../secrets.env",
            "/memories/a/../../x",
            "/memories//a",
            "/memories/./a",
            "/memories/a\\b",
        ] {
            assert!(clean_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn commands_parse_like_claude_sends_them() {
        assert_eq!(
            cmd(json!({"command": "view", "path": "/memories", "file_text": null})),
            Command::View {
                path: "/memories".into(),
                view_range: None
            }
        );
        assert_eq!(
            cmd(json!({"command": "view", "path": "/memories/a", "view_range": [1, -1]})),
            Command::View {
                path: "/memories/a".into(),
                view_range: Some((1, -1))
            }
        );
        assert_eq!(
            cmd(json!({"command": "str_replace", "path": "/memories/a", "old_str": "x"})),
            Command::StrReplace {
                path: "/memories/a".into(),
                old_str: "x".into(),
                new_str: None
            }
        );
        assert!(serde_json::from_value::<Command>(json!({"command": "format_disk"})).is_err());
    }

    #[test]
    fn view_lists_two_levels_without_hidden_files() {
        let f = folder(&[
            ("/memories/server.md", &"x".repeat(1024)),
            ("/memories/users/1.md", "a"),
            ("/memories/users/deep/2.md", "b"),
            ("/memories/.hidden", "c"),
        ]);
        let text = view(&f, ROOT, None).unwrap();
        assert_eq!(
            text,
            "Here're the files and directories up to 2 levels deep in /memories, excluding hidden items and node_modules:\n\
             1.0K\t/memories\n\
             1.0K\t/memories/server.md\n\
             0.0K\t/memories/users\n\
             0.0K\t/memories/users/1.md\n\
             0.0K\t/memories/users/deep"
        );
        // An empty folder is not an error.
        assert!(view(&Folder::new(), ROOT, None).is_ok());
        assert!(view(&f, "/memories/nope", None).is_err());
    }

    #[test]
    fn view_numbers_lines_and_ranges() {
        let f = folder(&[("/memories/a.md", "one\ntwo\nthree\n")]);
        assert_eq!(
            view(&f, "/memories/a.md", None).unwrap(),
            "Here's the content of /memories/a.md with line numbers:\n     1\tone\n     2\ttwo\n     3\tthree"
        );
        assert_eq!(
            view(&f, "/memories/a.md", Some((2, -1))).unwrap(),
            "Here's the content of /memories/a.md with line numbers:\n     2\ttwo\n     3\tthree"
        );
        assert!(view(&f, "/memories/a.md", Some((3, 2))).is_err());
        assert!(view(&f, "/memories/a.md", Some((1, 9))).is_err());
    }

    #[test]
    fn create_overwrites_but_not_directories() {
        let mut f = folder(&[("/memories/users/1.md", "a")]);
        let create = |f: &mut Folder, path: &str| {
            run(
                f,
                cmd(json!({"command": "create", "path": path, "file_text": "new"})),
            )
        };
        assert_eq!(
            create(&mut f, "/memories/users/1.md").unwrap(),
            "File created successfully at: /memories/users/1.md"
        );
        assert_eq!(f["/memories/users/1.md"], "new");
        assert!(create(&mut f, "/memories/users").is_err());
        assert!(create(&mut f, "/memories").is_err());
        assert!(create(&mut f, "/memories/users/1.md/x").is_err());
    }

    #[test]
    fn str_replace_needs_one_match() {
        let mut f = folder(&[("/memories/a.md", "likes: cats\nlikes: dogs\n")]);
        let replace =
            |f: &mut Folder, old: &str, new: &str| str_replace(f, "/memories/a.md", old, new);
        assert!(
            replace(&mut f, "likes", "x")
                .unwrap_err()
                .contains("in lines: 1, 2")
        );
        assert!(
            replace(&mut f, "birds", "x")
                .unwrap_err()
                .contains("did not appear verbatim")
        );
        let ok = replace(&mut f, "dogs", "horror films").unwrap();
        assert!(ok.starts_with("The memory file has been edited."), "{ok}");
        assert!(ok.contains("     2\tlikes: horror films"), "{ok}");
        assert_eq!(f["/memories/a.md"], "likes: cats\nlikes: horror films\n");
        // No new_str deletes the text.
        run(
            &mut f,
            cmd(json!({"command": "str_replace", "path": "/memories/a.md", "old_str": "likes: cats\n"})),
        )
        .unwrap();
        assert_eq!(f["/memories/a.md"], "likes: horror films\n");
        assert!(str_replace(&mut f, "/memories/b.md", "x", "y").is_err());
    }

    #[test]
    fn insert_after_a_line() {
        let mut f = folder(&[("/memories/todo.md", "a\nb\n")]);
        insert(&mut f, "/memories/todo.md", 0, "top\n").unwrap();
        insert(&mut f, "/memories/todo.md", 3, "end").unwrap();
        insert(&mut f, "/memories/todo.md", 1, "x\ny").unwrap();
        assert_eq!(f["/memories/todo.md"], "top\nx\ny\na\nb\nend\n");
        assert!(
            insert(&mut f, "/memories/todo.md", 9, "z")
                .unwrap_err()
                .contains("[0, 6]")
        );
        let mut empty = folder(&[("/memories/e.md", "")]);
        insert(&mut empty, "/memories/e.md", 0, "first").unwrap();
        assert_eq!(empty["/memories/e.md"], "first\n");
    }

    #[test]
    fn delete_files_and_directories() {
        let mut f = folder(&[
            ("/memories/a.md", "a"),
            ("/memories/users/1.md", "1"),
            ("/memories/users/2.md", "2"),
            ("/memories/usersx.md", "x"),
        ]);
        assert!(delete(&mut f, ROOT).is_err());
        assert_eq!(
            delete(&mut f, "/memories/users").unwrap(),
            "Successfully deleted /memories/users"
        );
        assert_eq!(
            f.keys().collect::<Vec<_>>(),
            ["/memories/a.md", "/memories/usersx.md"]
        );
        assert!(delete(&mut f, "/memories/users").is_err());
    }

    #[test]
    fn rename_files_and_directories() {
        let mut f = folder(&[("/memories/a.md", "a"), ("/memories/old/1.md", "1")]);
        rename(&mut f, "/memories/old", "/memories/new").unwrap();
        rename(&mut f, "/memories/a.md", "/memories/new/a.md").unwrap();
        assert_eq!(
            f.keys().collect::<Vec<_>>(),
            ["/memories/new/1.md", "/memories/new/a.md"]
        );
        assert!(rename(&mut f, "/memories/new/1.md", "/memories/new/a.md").is_err());
        assert!(rename(&mut f, "/memories/new", "/memories/new/inner").is_err());
        assert!(rename(&mut f, "/memories/missing", "/memories/x").is_err());
        assert!(rename(&mut f, ROOT, "/memories/x").is_err());
    }

    #[test]
    fn limits_and_changes() {
        let before = folder(&[("/memories/a.md", "a"), ("/memories/b.md", "b")]);
        let mut after = before.clone();
        after.insert("/memories/a.md".into(), "x".repeat(MAX_FILE + 1));
        assert!(check_limits(&before, &after).is_err());
        after.insert("/memories/a.md".into(), "aa".into());
        after.remove("/memories/b.md");
        assert!(check_limits(&before, &after).is_ok());
        assert_eq!(
            changes(&before, &after),
            [("/memories/a.md", Some("aa")), ("/memories/b.md", None)]
        );

        let full: Folder = (0..40)
            .map(|i| (format!("/memories/{i}.md"), "x".repeat(MAX_FILE)))
            .collect();
        let mut more = full.clone();
        more.insert("/memories/new.md".into(), "y".into());
        assert!(check_limits(&full, &more).is_err());
        let mut less = full.clone();
        less.remove("/memories/0.md");
        assert!(check_limits(&full, &less).is_ok());
    }
}
