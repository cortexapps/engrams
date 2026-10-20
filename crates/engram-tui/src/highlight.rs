//! Syntax highlighting for code blocks and diffs, as Codex does it:
//! `syntect` with the `two-face` grammar bundle (about 220 languages) on
//! the Oniguruma regex engine. The grammars load once, lazily, on the
//! first block that needs them, and each grammar compiles on first use.
//!
//! Colors are not taken from a theme: a scope maps to a [`Tone`], and the
//! renderer paints a tone with the terminal's own ANSI palette, so code
//! follows the user's colors (bat's `ansi` theme does the same).
//!
//! Callers highlight a cell once, when it is first laid out, and keep the
//! spans: a steady frame never highlights. Input past the limits below is
//! left plain, so one huge block cannot stall a frame.

use std::sync::OnceLock;

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

/// Lines highlighted per block; the rest stays plain. At about 50 µs a
/// line this bounds one block to some 25 ms, once.
pub const MAX_LINES: usize = 500;
/// A line longer than this is left plain (minified code, data).
pub const MAX_LINE: usize = 4096;

/// What a span of code is, as the renderer colors it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Tone {
    Comment = 1,
    String,
    Keyword,
    Number,
    Constant,
    Function,
    Type,
    Tag,
}

impl Tone {
    /// The tone a run carries as a byte (0 is none).
    pub fn from_u8(v: u8) -> Option<Tone> {
        Some(match v {
            1 => Tone::Comment,
            2 => Tone::String,
            3 => Tone::Keyword,
            4 => Tone::Number,
            5 => Tone::Constant,
            6 => Tone::Function,
            7 => Tone::Type,
            8 => Tone::Tag,
            _ => return None,
        })
    }
}

/// A highlighted span: byte range in the caller's text, and its tone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
    pub tone: Tone,
}

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(two_face::syntax::extra_newlines)
}

/// Scope prefixes to tones, most specific first.
fn rules() -> &'static [(Scope, Tone)] {
    static RULES: OnceLock<Vec<(Scope, Tone)>> = OnceLock::new();
    RULES.get_or_init(|| {
        [
            ("comment", Tone::Comment),
            ("string", Tone::String),
            ("constant.numeric", Tone::Number),
            ("constant.character", Tone::Constant),
            ("constant.language", Tone::Constant),
            ("constant.other", Tone::Constant),
            ("support.constant", Tone::Constant),
            ("variable.language", Tone::Constant),
            ("entity.name.function", Tone::Function),
            ("support.function", Tone::Function),
            ("variable.function", Tone::Function),
            ("meta.function-call", Tone::Function),
            ("entity.name.type", Tone::Type),
            ("entity.name.class", Tone::Type),
            ("entity.other.inherited-class", Tone::Type),
            ("support.type", Tone::Type),
            ("support.class", Tone::Type),
            ("storage.type", Tone::Keyword),
            ("storage.modifier", Tone::Keyword),
            ("keyword", Tone::Keyword),
            ("entity.name.tag", Tone::Tag),
            ("entity.other.attribute-name", Tone::Function),
            ("markup.heading", Tone::Keyword),
            ("markup.inserted", Tone::String),
            ("markup.deleted", Tone::Tag),
        ]
        .into_iter()
        .filter_map(|(s, t)| Scope::new(s).ok().map(|s| (s, t)))
        .collect()
    })
}

/// The tone of the innermost scope that has one.
fn tone(stack: &ScopeStack) -> Option<Tone> {
    let rules = rules();
    stack.as_slice().iter().rev().find_map(|scope| {
        rules
            .iter()
            .find(|(prefix, _)| prefix.is_prefix_of(*scope))
            .map(|&(_, t)| t)
    })
}

/// The grammar for a code fence's info string (`rust`, `py`, `tsx`,
/// `sh`, …) or a file's extension.
pub fn syntax_for(token: &str) -> Option<&'static SyntaxReference> {
    let token = token.trim().split([' ', ',', '{']).next().unwrap_or("");
    if token.is_empty() {
        return None;
    }
    let token = match token.to_ascii_lowercase().as_str() {
        "shell" | "sh" | "zsh" | "console" | "shellsession" => "bash".to_string(),
        "jsx" => "js".to_string(),
        "yml" => "yaml".to_string(),
        "py3" => "python".to_string(),
        "golang" => "go".to_string(),
        other => other.to_string(),
    };
    let set = syntaxes();
    set.find_syntax_by_token(&token)
        .or_else(|| set.find_syntax_by_extension(&token))
}

/// The grammar for a file path, by its extension (or whole file name, for
/// `Makefile`, `Dockerfile`).
pub fn syntax_for_path(path: &str) -> Option<&'static SyntaxReference> {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((_, ext)) => syntax_for(ext),
        None => syntax_for(name),
    }
}

/// One parse: lines go in, spans come out, with the grammar's state kept
/// between lines.
pub struct Lines {
    state: ParseState,
    stack: ScopeStack,
    line: String,
    lines: usize,
}

impl Lines {
    pub fn new(syntax: &'static SyntaxReference) -> Lines {
        Lines {
            state: ParseState::new(syntax),
            stack: ScopeStack::new(),
            line: String::new(),
            lines: 0,
        }
    }

    /// Highlight `text` (one line, no newline) that sits at byte `at` of
    /// the caller's text. Returns false, and adds nothing, past the
    /// limits or when the grammar fails: the line stays plain.
    pub fn line(&mut self, text: &str, at: usize, out: &mut Vec<Span>) -> bool {
        if self.lines >= MAX_LINES || text.len() > MAX_LINE {
            return false;
        }
        self.lines += 1;
        // The `newlines` grammars expect each line to end with one.
        self.line.clear();
        self.line.push_str(text);
        self.line.push('\n');
        let Ok(ops) = self.state.parse_line(&self.line, syntaxes()) else {
            return false;
        };
        let mut last = 0;
        let push = |from: usize, to: usize, stack: &ScopeStack, out: &mut Vec<Span>| {
            let to = to.min(text.len());
            if from >= to {
                return;
            }
            if let Some(t) = tone(stack) {
                out.push(Span {
                    start: (at + from) as u32,
                    end: (at + to) as u32,
                    tone: t,
                });
            }
        };
        for (pos, op) in ops {
            push(last, pos, &self.stack, out);
            if self.stack.apply(&op).is_err() {
                return false;
            }
            last = pos;
        }
        push(last, text.len(), &self.stack, out);
        true
    }
}

/// Highlight the lines of a diff (`+`, `-`, or ` ` then the code; `@@` and
/// `⋯` rows are left alone). Removed lines are parsed as the old file and
/// added lines as the new one, context lines as both, so a construct that
/// spans lines stays coherent on each side.
pub fn diff(path: &str, text: &str) -> Vec<Span> {
    let mut out = Vec::new();
    let Some(syntax) = syntax_for_path(path) else {
        return out;
    };
    let mut old = Lines::new(syntax);
    let mut new = Lines::new(syntax);
    let mut scratch = Vec::new();
    let mut at = 0;
    for line in text.split('\n') {
        let code = line.get(1..).unwrap_or("");
        match line.as_bytes().first() {
            Some(b'-') => {
                old.line(code, at + 1, &mut out);
            }
            Some(b'+') => {
                new.line(code, at + 1, &mut out);
            }
            Some(b' ') => {
                scratch.clear();
                old.line(code, at + 1, &mut scratch);
                new.line(code, at + 1, &mut out);
            }
            _ => {}
        }
        at += line.len() + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tones(spans: &[Span], text: &str) -> Vec<(String, Tone)> {
        spans
            .iter()
            .map(|s| (text[s.start as usize..s.end as usize].to_string(), s.tone))
            .collect()
    }

    #[test]
    fn rust_code_gets_keywords_strings_and_comments() {
        let syntax = syntax_for("rust").expect("rust grammar");
        let text = "let s = \"hi\"; // greet";
        let mut out = Vec::new();
        assert!(Lines::new(syntax).line(text, 0, &mut out));
        let t = tones(&out, text);
        assert!(t.contains(&("let".into(), Tone::Keyword)), "{t:?}");
        assert!(
            t.iter()
                .any(|(s, k)| s.contains("hi") && *k == Tone::String),
            "{t:?}"
        );
        assert!(
            t.iter()
                .any(|(s, k)| s.contains("greet") && *k == Tone::Comment),
            "{t:?}"
        );
    }

    #[test]
    fn fences_and_paths_find_their_grammar() {
        for token in [
            "rust", "rs", "py", "python", "sh", "bash", "ts", "tsx", "json", "yaml", "go",
        ] {
            assert!(syntax_for(token).is_some(), "{token}");
        }
        assert!(syntax_for("not-a-language").is_none());
        assert!(syntax_for("").is_none());
        assert!(syntax_for_path("crates/a/src/lib.rs").is_some());
        assert!(syntax_for_path("Makefile").is_some());
    }

    #[test]
    fn a_diff_highlights_code_after_the_sign_and_skips_markers() {
        let text = "-let a = 1;\n+let a = 2;\n⋯\n let b = 3;";
        let spans = diff("x.rs", text);
        assert!(!spans.is_empty());
        // Never the sign column, never the hunk separator.
        for s in &spans {
            let start = s.start as usize;
            let line_start = text[..start].rfind('\n').map_or(0, |p| p + 1);
            assert!(start > line_start, "a span starts on a sign: {s:?}");
            assert!(!text[start..s.end as usize].contains('⋯'));
        }
        let t = tones(&spans, text);
        assert!(
            t.iter()
                .filter(|(s, k)| s == "let" && *k == Tone::Keyword)
                .count()
                >= 3,
            "{t:?}"
        );
    }

    #[test]
    fn a_long_line_stays_plain() {
        let syntax = syntax_for("rust").unwrap();
        let mut out = Vec::new();
        assert!(!Lines::new(syntax).line(&"x".repeat(MAX_LINE + 1), 0, &mut out));
        assert!(out.is_empty());
    }
}
