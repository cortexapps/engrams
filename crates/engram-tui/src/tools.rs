//! The one line a tool row shows for a call's arguments. The harness
//! sends the arguments as JSON; a user wants the command, the path or
//! the pattern, not the object around it.

use serde_json::{Map, Value};

/// `mcp__x__WebFetch`, `WebFetch`, `web_fetch` → `web_fetch`.
pub fn canonical(name: &str) -> String {
    let base = name.rsplit("__").next().unwrap_or(name);
    let mut out = String::with_capacity(base.len() + 4);
    for (i, c) in base.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// A tool that only reads: consecutive calls fold into one `Explored`
/// row.
pub fn is_read_only(canonical_name: &str) -> bool {
    matches!(canonical_name, "read" | "glob" | "grep" | "ls")
}

/// The verb an `Explored` row uses for a read-only tool.
pub fn explore_verb(canonical_name: &str) -> &'static str {
    match canonical_name {
        "read" => "Read",
        "grep" => "Search",
        "glob" => "Find",
        _ => "List",
    }
}

fn string<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

/// The summary of `args` (JSON, or already a summary) for tool `name`.
pub fn summarize(name: &str, args: &str) -> String {
    let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(args) else {
        return args.trim().to_string();
    };
    summarize_object(&canonical(name), &obj)
}

fn summarize_object(name: &str, obj: &Map<String, Value>) -> String {
    let s = |k: &str| string(obj, k);
    let out = match name {
        "bash" => s("command")
            .or_else(|| s("description"))
            .map(str::to_string),
        "read" | "edit" | "write" | "multi_edit" | "notebook_edit" | "ls" => s("file_path")
            .or_else(|| s("path"))
            .or_else(|| s("notebook_path"))
            .map(str::to_string),
        "glob" | "grep" => s("pattern").map(|p| match s("path") {
            Some(path) => format!("{p} in {path}"),
            None => p.to_string(),
        }),
        "web_fetch" => s("url").map(str::to_string),
        "web_search" => s("query").map(str::to_string),
        "task" | "agent" => s("description")
            .or_else(|| s("prompt").map(first_line))
            .map(str::to_string),
        "todo_write" => obj.get("todos").and_then(Value::as_array).map(|t| {
            let n = t.len();
            format!("{n} {}", if n == 1 { "todo" } else { "todos" })
        }),
        "skill" => s("skill").map(str::to_string),
        "tool_search" => s("query").map(str::to_string),
        _ => None,
    };
    if let Some(out) = out {
        return out;
    }
    // Unknown tool: the first field that looks like the subject, else a
    // short `key: value` list of the scalar fields.
    const SUBJECT: [&str; 10] = [
        "command",
        "file_path",
        "path",
        "pattern",
        "query",
        "url",
        "description",
        "title",
        "name",
        "prompt",
    ];
    if let Some(v) = SUBJECT.iter().find_map(|k| s(k)) {
        return first_line(v).to_string();
    }
    let mut out = String::new();
    for (k, v) in obj {
        let shown = match v {
            Value::String(s) => first_line(s).to_string(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => continue,
        };
        if !out.is_empty() {
            out.push_str(" · ");
        }
        out.push_str(k);
        out.push_str(": ");
        out.push_str(&shown);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_tools_show_their_subject() {
        assert_eq!(
            summarize("Bash", r#"{"command":"ls -la","description":"List"}"#),
            "ls -la"
        );
        assert_eq!(summarize("Read", r#"{"file_path":"src/a.rs"}"#), "src/a.rs");
        assert_eq!(
            summarize("Grep", r#"{"pattern":"fn main","path":"src"}"#),
            "fn main in src"
        );
        assert_eq!(summarize("Glob", r#"{"pattern":"**/*.rs"}"#), "**/*.rs");
        assert_eq!(
            summarize("mcp__web__WebFetch", r#"{"url":"https://x.test/"}"#),
            "https://x.test/"
        );
        assert_eq!(
            summarize("TodoWrite", r#"{"todos":[{"a":1},{"b":2}]}"#),
            "2 todos"
        );
        assert_eq!(
            summarize(
                "Task",
                r#"{"prompt":"first line\nsecond","subagent_type":"x"}"#
            ),
            "first line"
        );
    }

    #[test]
    fn unknown_tools_fall_back_to_a_subject_or_a_field_list() {
        assert_eq!(summarize("Thing", r#"{"title":"T","n":2}"#), "T");
        assert_eq!(
            summarize("Thing", r#"{"n":2,"on":true,"obj":{}}"#),
            "n: 2 · on: true"
        );
    }

    #[test]
    fn a_plain_summary_passes_through() {
        assert_eq!(summarize("Bash", " ls -la "), "ls -la");
        assert_eq!(canonical("mcp__x__ExitPlanMode"), "exit_plan_mode");
        assert!(is_read_only("grep"));
        assert!(!is_read_only("bash"));
    }
}
