//! Output helpers. The contract carried over from the Bun CLI (and the Rust
//! CLI before it), so the jq pipelines in deploy/dev scripts port unchanged:
//!   - default output is a human-readable table / key-value view
//!   - `--json` prints pretty JSON to stdout and NOTHING else
//!   - errors go to stderr prefixed `engrams:` and exit 1
//!
//! String widths follow JavaScript: `truncate` counts code points (the Bun
//! CLI spread the string), padding counts UTF-16 units (`padEnd`).

use std::io::Write;

use serde::Serialize;

/// How a verb ends other than success.
#[derive(Debug)]
pub enum CliError {
    /// Print `engrams: <message>` on stderr and exit 1.
    Message(String),
    /// Exit with this code; the verb already printed what it had to.
    Exit(i32),
}

pub type CliResult<T = ()> = Result<T, CliError>;

/// `fail(message)`: the one error edge for verbs.
pub fn fail<T>(message: impl Into<String>) -> CliResult<T> {
    Err(CliError::Message(message.into()))
}

/// `.catch(failWith)`: an RPC/transport failure rendered as
/// `<Code>: <message>`.
impl From<engram_app_client::Error> for CliError {
    fn from(e: engram_app_client::Error) -> Self {
        CliError::Message(e.to_string())
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        CliError::Message(e.to_string())
    }
}

impl From<serde_json::Error> for CliError {
    fn from(e: serde_json::Error) -> Self {
        CliError::Message(e.to_string())
    }
}

/// `JSON.stringify(value, null, 2)` + `console.log`.
pub fn print_json<T: Serialize>(value: &T) {
    // Serialization of our own view structs cannot fail.
    let text = serde_json::to_string_pretty(value).expect("view structs serialize");
    println!("{text}");
}

/// `JSON.stringify(s)` for a string: the quoted spelling error messages use.
pub fn js_quote(s: &str) -> String {
    serde_json::to_string(s).expect("strings serialize")
}

/// Code-point based truncation with an ellipsis.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `String.prototype.padEnd`.
pub fn pad_end(s: &str, width: usize) -> String {
    let mut out = s.to_string();
    for _ in utf16_len(s)..width {
        out.push(' ');
    }
    out
}

/// `String.prototype.padStart`.
pub fn pad_start(s: &str, width: usize) -> String {
    let mut out = String::new();
    for _ in utf16_len(s)..width {
        out.push(' ');
    }
    out.push_str(s);
    out
}

/// Left-aligned fixed-width columns; the last column is never padded or
/// truncated.
pub fn table(header: &[&str], rows: &[Vec<String>], widths: &[usize]) {
    let line = |cells: &[String]| -> String {
        let last = cells.len().saturating_sub(1);
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == last {
                    c.clone()
                } else {
                    pad_end(&truncate(c, widths[i]), widths[i])
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
    };
    let header: Vec<String> = header.iter().map(|s| s.to_string()).collect();
    println!("{}", line(&header));
    for row in rows {
        println!("{}", line(row));
    }
}

/// `key             : value` detail view; pairs with no value are skipped.
pub fn detail(pairs: &[(&str, Option<String>)]) {
    for (k, v) in pairs {
        if let Some(v) = v {
            if !v.is_empty() {
                println!("{}: {v}", pad_end(k, 16));
            }
        }
    }
}

/// Write to stderr without a newline and flush (progress lines).
pub fn eprint_flush(s: &str) {
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(s.as_bytes());
    let _ = err.flush();
}

/// `JSON.stringify(JSON.parse(s))` for valid JSON: whitespace outside strings
/// removed, key order kept. `None` when `s` is not JSON.
pub fn compact_json(s: &str) -> Option<String> {
    serde_json::from_str::<serde::de::IgnoredAny>(s).ok()?;
    let mut out = String::with_capacity(s.len());
    let mut in_str = false;
    let mut escaped = false;
    for c in s.chars() {
        if in_str {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            out.push(c);
        } else if !c.is_whitespace() {
            out.push(c);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_is_char_based_with_ellipsis() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("🦀🦀🦀", 2), "🦀…");
        assert_eq!(truncate("hi", 10), "hi");
    }

    #[test]
    fn padding_counts_utf16_units_like_javascript() {
        assert_eq!(pad_end("ab", 4), "ab  ");
        assert_eq!(pad_start("7", 6), "     7");
        // An astral char is two UTF-16 units: JS pads one space, not two.
        assert_eq!(pad_end("🦀", 3), "🦀 ");
    }

    #[test]
    fn compact_json_keeps_order_and_rejects_non_json() {
        assert_eq!(
            compact_json(" {\"b\" : 1 , \"a\": [ 2, \"x y\" ] } ").as_deref(),
            Some(r#"{"b":1,"a":[2,"x y"]}"#)
        );
        assert_eq!(
            compact_json(r#"{"s":"a\"b"}"#).as_deref(),
            Some(r#"{"s":"a\"b"}"#)
        );
        assert!(compact_json("not-json").is_none());
    }
}
