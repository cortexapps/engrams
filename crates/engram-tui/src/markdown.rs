//! Markdown → display text plus style runs (ADR 0124, PR5). The writer
//! walks `pulldown-cmark` events once and produces the text the wrapper
//! lays out, with non-overlapping runs that carry inline flags. Tables
//! are buffered until closed so their columns can be padded.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use crate::wrap::display_width;

/// Inline flags a run carries. They combine (`BOLD | CODE`).
pub mod flags {
    pub const BOLD: u8 = 1;
    pub const ITALIC: u8 = 2;
    pub const CODE: u8 = 4;
    pub const STRIKE: u8 = 8;
    pub const LINK: u8 = 16;
    pub const HEADING: u8 = 32;
    pub const QUOTE: u8 = 64;
    /// Markers, rules, table borders, link targets.
    pub const DIM: u8 = 128;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub start: u32,
    pub end: u32,
    pub flags: u8,
}

/// Display text and its sorted, non-overlapping runs. Unstyled text has
/// no run.
#[derive(Debug, Default, Clone)]
pub struct Rendered {
    pub text: String,
    pub runs: Vec<Run>,
}

impl Rendered {
    pub fn render(md: &str) -> Rendered {
        let mut r = Rendered::default();
        r.append(md);
        r
    }

    /// Render `md` and append it after a block separator.
    pub fn append(&mut self, md: &str) {
        let mut w = Writer::new(self);
        w.run(md);
    }

    /// Append raw text with no styling (a streaming tail).
    pub fn push_plain(&mut self, s: &str) {
        self.text.push_str(s);
    }

    /// Cut the text and every run past `len`.
    pub fn truncate(&mut self, len: usize) {
        self.text.truncate(len);
        while self.runs.last().is_some_and(|r| r.end as usize > len) {
            let last = self.runs.last_mut().expect("checked");
            if (last.start as usize) < len {
                last.end = len as u32;
                break;
            }
            self.runs.pop();
        }
    }

    /// Index of the first run that ends after `offset` (binary search).
    pub fn runs_from(&self, offset: usize) -> usize {
        self.runs.partition_point(|r| r.end as usize <= offset)
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.runs.clear();
    }
}

struct TableBuf {
    align: Vec<Alignment>,
    rows: Vec<Vec<String>>,
    cell: String,
    in_head: bool,
}

struct Writer<'r> {
    out: &'r mut Rendered,
    flags: u8,
    /// Open lists: `None` = bullet, `Some(n)` = the next ordinal.
    lists: Vec<Option<u64>>,
    quote: u8,
    code_block: bool,
    /// The next block needs a blank line before it.
    pending_blank: bool,
    /// Nothing has been written on the current line yet.
    line_start: bool,
    /// The current list item has not written its first block yet.
    item_fresh: bool,
    table: Option<TableBuf>,
    link: Option<(String, usize)>,
}

impl<'r> Writer<'r> {
    fn new(out: &'r mut Rendered) -> Self {
        let line_start = out.text.is_empty() || out.text.ends_with('\n');
        let pending_blank = !out.text.is_empty();
        Writer {
            out,
            flags: 0,
            lists: Vec::new(),
            quote: 0,
            code_block: false,
            pending_blank,
            line_start,
            item_fresh: false,
            table: None,
            link: None,
        }
    }

    fn run(&mut self, md: &str) {
        let opts =
            Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
        for ev in Parser::new_ext(md, opts) {
            self.event(ev);
        }
        // Blocks end with a newline; the display text does not.
        while self.out.text.ends_with('\n') {
            self.out.text.pop();
        }
        let len = self.out.text.len();
        self.out.truncate(len);
    }

    fn raw(&mut self, s: &str, flags: u8) {
        if s.is_empty() {
            return;
        }
        let start = self.out.text.len() as u32;
        self.out.text.push_str(s);
        let end = self.out.text.len() as u32;
        if flags == 0 {
            return;
        }
        if let Some(last) = self.out.runs.last_mut() {
            if last.end == start && last.flags == flags {
                last.end = end;
                return;
            }
        }
        self.out.runs.push(Run { start, end, flags });
    }

    fn newline(&mut self) {
        self.out.text.push('\n');
        self.line_start = true;
    }

    fn ensure_line_start(&mut self) {
        if !self.line_start {
            self.newline();
        }
    }

    /// Separate a new block from the previous one.
    fn block_start(&mut self) {
        if self.table.is_some() {
            return;
        }
        if self.item_fresh {
            // The first block of a list item sits on the marker line.
            self.item_fresh = false;
            self.pending_blank = false;
            return;
        }
        self.ensure_line_start();
        if self.pending_blank && !self.out.text.is_empty() {
            self.newline();
        }
        self.pending_blank = false;
    }

    fn block_end(&mut self) {
        self.ensure_line_start();
        self.pending_blank = true;
    }

    /// The prefix every line inside quotes/lists carries.
    fn line_prefix(&mut self) {
        if !self.line_start {
            return;
        }
        self.line_start = false;
        for _ in 0..self.quote {
            self.raw("│ ", flags::QUOTE | flags::DIM);
        }
        let indent = self.lists.len() * 2 + if self.code_block { 2 } else { 0 };
        for _ in 0..indent {
            self.out.text.push(' ');
        }
    }

    fn text(&mut self, s: &str) {
        if let Some(t) = &mut self.table {
            t.cell.push_str(s);
            return;
        }
        let f = self.flags | if self.quote > 0 { flags::QUOTE } else { 0 };
        if self.code_block {
            // Code arrives line by line, possibly split mid-line; the
            // indent is part of the line prefix so a split never doubles it.
            for line in s.split_inclusive('\n') {
                let (body, nl) = match line.strip_suffix('\n') {
                    Some(b) => (b, true),
                    None => (line, false),
                };
                self.line_prefix();
                self.raw(body, f | flags::CODE);
                if nl {
                    self.newline();
                }
            }
            return;
        }
        self.line_prefix();
        self.raw(s, f);
    }

    fn event(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.text(&t),
            Event::Code(c) => {
                if let Some(t) = &mut self.table {
                    t.cell.push_str(&c);
                } else {
                    self.line_prefix();
                    let f = self.flags | flags::CODE;
                    self.raw(&c, f);
                }
            }
            Event::InlineMath(m) | Event::DisplayMath(m) => self.text(&m),
            Event::Html(h) | Event::InlineHtml(h) => self.text(&h),
            Event::FootnoteReference(r) => {
                self.line_prefix();
                self.raw("[", flags::DIM);
                self.raw(&r, flags::DIM);
                self.raw("]", flags::DIM);
            }
            Event::SoftBreak => {
                if let Some(t) = &mut self.table {
                    t.cell.push(' ');
                } else {
                    self.raw(" ", 0);
                }
            }
            Event::HardBreak => {
                if self.table.is_none() {
                    self.newline();
                }
            }
            Event::Rule => {
                self.block_start();
                self.line_prefix();
                self.raw("────────────────────", flags::DIM);
                self.block_end();
            }
            Event::TaskListMarker(done) => {
                self.line_prefix();
                self.raw(if done { "[x] " } else { "[ ] " }, flags::DIM);
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.block_start(),
            Tag::Heading { .. } => {
                self.block_start();
                self.flags |= flags::HEADING | flags::BOLD;
            }
            Tag::BlockQuote(_) => {
                self.block_start();
                self.quote += 1;
            }
            Tag::CodeBlock(kind) => {
                self.block_start();
                if let CodeBlockKind::Fenced(lang) = kind {
                    if !lang.is_empty() {
                        self.line_prefix();
                        self.raw(&lang, flags::DIM);
                        self.newline();
                    }
                }
                self.code_block = true;
            }
            Tag::List(start) => {
                if self.lists.is_empty() {
                    self.block_start();
                } else {
                    self.ensure_line_start();
                }
                self.lists.push(start);
            }
            Tag::Item => {
                self.ensure_line_start();
                let depth = self.lists.len();
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                self.line_start = false;
                for _ in 0..self.quote {
                    self.raw("│ ", flags::QUOTE | flags::DIM);
                }
                for _ in 0..(depth.saturating_sub(1) * 2) {
                    self.out.text.push(' ');
                }
                self.raw(&marker, flags::DIM);
                self.item_fresh = true;
            }
            Tag::Emphasis => self.flags |= flags::ITALIC,
            Tag::Strong => self.flags |= flags::BOLD,
            Tag::Strikethrough => self.flags |= flags::STRIKE,
            Tag::Link { dest_url, .. } => {
                self.flags |= flags::LINK;
                self.link = Some((dest_url.into_string(), self.out.text.len()));
            }
            Tag::Image { dest_url, .. } => {
                self.line_prefix();
                self.raw("image: ", flags::DIM);
                self.flags |= flags::LINK;
                self.link = Some((dest_url.into_string(), self.out.text.len()));
            }
            Tag::Table(align) => {
                self.block_start();
                self.table = Some(TableBuf {
                    align,
                    rows: Vec::new(),
                    cell: String::new(),
                    in_head: false,
                });
            }
            Tag::TableHead => {
                if let Some(t) = &mut self.table {
                    t.in_head = true;
                    t.rows.push(Vec::new());
                }
            }
            Tag::TableRow => {
                if let Some(t) = &mut self.table {
                    t.rows.push(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(t) = &mut self.table {
                    t.cell.clear();
                }
            }
            Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_) => self.block_start(),
            Tag::Superscript | Tag::Subscript => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.block_end(),
            TagEnd::Heading(_) => {
                self.flags &= !(flags::HEADING | flags::BOLD);
                self.block_end();
            }
            TagEnd::BlockQuote(_) => {
                self.quote = self.quote.saturating_sub(1);
                self.block_end();
            }
            TagEnd::CodeBlock => {
                self.code_block = false;
                self.block_end();
            }
            TagEnd::List(_) => {
                self.lists.pop();
                if self.lists.is_empty() {
                    self.block_end();
                } else {
                    self.ensure_line_start();
                }
            }
            TagEnd::Item => {
                self.item_fresh = false;
                self.ensure_line_start();
                self.pending_blank = false;
            }
            TagEnd::Emphasis => self.flags &= !flags::ITALIC,
            TagEnd::Strong => self.flags &= !flags::BOLD,
            TagEnd::Strikethrough => self.flags &= !flags::STRIKE,
            TagEnd::Link | TagEnd::Image => {
                self.flags &= !flags::LINK;
                if let Some((dest, from)) = self.link.take() {
                    let shown = &self.out.text[from.min(self.out.text.len())..];
                    let same = shown == dest
                        || shown.trim_end_matches('/') == dest.trim_end_matches('/')
                        || dest.is_empty();
                    if !same {
                        let s = format!(" ({dest})");
                        self.raw(&s, flags::DIM);
                    }
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.write_table(t);
                    self.block_end();
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    t.in_head = false;
                }
            }
            TagEnd::TableRow => {}
            TagEnd::TableCell => {
                if let Some(t) = &mut self.table {
                    let cell = std::mem::take(&mut t.cell);
                    if let Some(row) = t.rows.last_mut() {
                        row.push(cell);
                    }
                }
            }
            TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_) => self.block_end(),
            TagEnd::Superscript | TagEnd::Subscript => {}
        }
    }

    fn write_table(&mut self, t: TableBuf) {
        let cols = t.rows.iter().map(Vec::len).max().unwrap_or(0);
        if cols == 0 {
            return;
        }
        let mut widths = vec![0usize; cols];
        for row in &t.rows {
            for (i, c) in row.iter().enumerate() {
                widths[i] = widths[i].max(display_width(c));
            }
        }
        for (ri, row) in t.rows.iter().enumerate() {
            self.line_prefix();
            for (i, w) in widths.iter().enumerate() {
                if i > 0 {
                    self.raw(" │ ", flags::DIM);
                }
                let c = row.get(i).map(String::as_str).unwrap_or("");
                let pad = w.saturating_sub(display_width(c));
                let (left, right) = match t.align.get(i) {
                    Some(Alignment::Right) => (pad, 0),
                    Some(Alignment::Center) => (pad / 2, pad - pad / 2),
                    _ => (0, pad),
                };
                for _ in 0..left {
                    self.out.text.push(' ');
                }
                self.raw(c, if ri == 0 { flags::BOLD } else { 0 });
                for _ in 0..right {
                    self.out.text.push(' ');
                }
            }
            self.newline();
            if ri == 0 {
                self.line_prefix();
                for (i, w) in widths.iter().enumerate() {
                    if i > 0 {
                        self.raw("─┼─", flags::DIM);
                    }
                    let bar: String = "─".repeat(*w);
                    self.raw(&bar, flags::DIM);
                }
                self.newline();
            }
        }
    }
}

/// The byte length of the prefix of `raw` that is safe to render on its
/// own while more text is still arriving: up to the last blank line that
/// is outside a code fence and not inside a table. Everything after it is
/// the uncommitted tail. (Codex's stream rule.)
pub fn committed_boundary(raw: &str) -> usize {
    let mut fence_open = false;
    let mut boundary = 0usize;
    let mut pos = 0usize;
    let mut in_table = false;
    for line in raw.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fence_open = !fence_open;
        }
        if !fence_open {
            in_table = trimmed.starts_with('|');
        }
        if line == "\n" && !fence_open && !in_table {
            boundary = pos + 1;
        }
        pos += line.len();
    }
    boundary
}

/// An assistant message while it streams: the markdown source, the display
/// text rendered so far, and how much of the source is committed. Chunks
/// append to the source; the committed prefix (up to the last blank line
/// outside a fence or table) is rendered once and never again, the tail is
/// shown plain until it commits. `finalize` renders the whole source and
/// drops it, so a finished cell holds its text once (ADR 0124 D3).
#[derive(Debug, Default)]
pub struct Markdown {
    raw: String,
    shown: Rendered,
    /// Bytes of `raw` rendered into `shown[..shown_done]`.
    raw_done: usize,
    shown_done: usize,
}

impl Markdown {
    /// The display text.
    pub fn text(&self) -> &str {
        &self.shown.text
    }

    pub fn runs(&self) -> &[Run] {
        &self.shown.runs
    }

    /// The markdown source. Empty once finalized.
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// Replace the source from byte `base` with `text` (a final message
    /// replacing its chunk stream). Returns the display offset from which
    /// the text changed.
    pub fn replace_from(&mut self, base: usize, text: &str) -> usize {
        self.raw.truncate(base.min(self.raw.len()));
        if !self.raw.is_empty() {
            self.raw.push_str("\n\n");
        }
        self.raw.push_str(text);
        self.sync()
    }

    /// Append a chunk. Returns the display offset from which the text
    /// changed.
    pub fn push(&mut self, chunk: &str) -> usize {
        self.raw.push_str(chunk);
        self.sync()
    }

    /// Render the whole source, tail included (a durable message is
    /// complete even while the cell stays open for more). Returns the
    /// display offset from which the text changed.
    pub fn commit_all(&mut self) -> usize {
        if self.raw_done == self.raw.len() {
            return self.shown.text.len();
        }
        let from = self.shown_done;
        self.shown.truncate(self.shown_done);
        self.shown.append(&self.raw[self.raw_done..]);
        self.raw_done = self.raw.len();
        self.shown_done = self.shown.text.len();
        from
    }

    /// Render everything and drop the source.
    pub fn finalize(&mut self) {
        if self.raw.is_empty() {
            return;
        }
        self.shown = Rendered::render(&self.raw);
        self.raw = String::new();
        self.raw_done = 0;
        self.shown_done = self.shown.text.len();
    }

    fn sync(&mut self) -> usize {
        let boundary = committed_boundary(&self.raw);
        if boundary < self.raw_done {
            // The source was cut below the committed prefix: start over.
            self.shown.clear();
            self.raw_done = 0;
            self.shown_done = 0;
        }
        let changed_from;
        if boundary > self.raw_done {
            changed_from = self.shown_done;
            self.shown.truncate(self.shown_done);
            self.shown.append(&self.raw[self.raw_done..boundary]);
            self.raw_done = boundary;
            self.shown_done = self.shown.text.len();
        } else {
            // Only the plain tail grew: keep what is on screen.
            changed_from = self.shown.text.len();
        }
        let tail = self.raw[self.raw_done..].trim_end_matches('\n');
        // The tail on screen is `shown[shown_done..]`; it must equal
        // "\n\n" + tail (or nothing). Re-sync only when it differs.
        let have = &self.shown.text[self.shown_done..];
        let want_sep = self.shown_done > 0 && !tail.is_empty();
        let matches = if want_sep {
            have.strip_prefix("\n\n").is_some_and(|h| h == tail)
        } else {
            have == tail
        };
        if matches {
            return changed_from;
        }
        // Common case: the old tail is a prefix of the new one.
        let old_len = self.shown.text.len();
        let want_prefix_ok = if want_sep {
            have.strip_prefix("\n\n")
                .is_some_and(|h| tail.starts_with(h))
        } else {
            tail.starts_with(have)
        };
        if want_prefix_ok && old_len > self.shown_done {
            let had = if want_sep { have.len() - 2 } else { have.len() };
            self.shown.push_plain(&tail[had..]);
            return changed_from.min(old_len);
        }
        self.shown.truncate(self.shown_done);
        if want_sep {
            self.shown.push_plain("\n\n");
        }
        self.shown.push_plain(tail);
        changed_from.min(self.shown_done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs_of(r: &Rendered, f: u8) -> Vec<&str> {
        r.runs
            .iter()
            .filter(|run| run.flags & f != 0)
            .map(|run| &r.text[run.start as usize..run.end as usize])
            .collect()
    }

    #[test]
    fn blocks_lists_and_inline_styles() {
        let r = Rendered::render(
            "# Title\n\nSome **bold** and *it* and `code`.\n\n- one\n- two\n  - nested\n\n1. first\n2. second\n\n> quoted\n\n```rust\nfn x() {}\n```\n\nAfter.",
        );
        assert_eq!(
            r.text,
            "Title\n\nSome bold and it and code.\n\n• one\n• two\n  • nested\n\n1. first\n2. second\n\n│ quoted\n\nrust\n  fn x() {}\n\nAfter."
        );
        assert_eq!(runs_of(&r, flags::HEADING), vec!["Title"]);
        assert_eq!(runs_of(&r, flags::BOLD), vec!["Title", "bold"]);
        assert_eq!(runs_of(&r, flags::ITALIC), vec!["it"]);
        assert_eq!(runs_of(&r, flags::CODE), vec!["code", "fn x() {}"]);
        assert_eq!(runs_of(&r, flags::QUOTE), vec!["│ ", "quoted"]);
        // Runs are sorted and never overlap.
        for w in r.runs.windows(2) {
            assert!(w[0].end <= w[1].start, "{:?}", w);
        }
    }

    #[test]
    fn links_tables_and_tasks() {
        let r = Rendered::render(
            "See [docs](https://example.com/d) and <https://example.com>.\n\n| a | bb |\n|---|---:|\n| 1 | 22 |\n\n- [x] done\n- [ ] open",
        );
        assert_eq!(
            r.text,
            "See docs (https://example.com/d) and https://example.com.\n\na │ bb\n──┼───\n1 │ 22\n\n• [x] done\n• [ ] open"
        );
        assert_eq!(
            runs_of(&r, flags::LINK),
            vec!["docs", "https://example.com"]
        );
    }

    #[test]
    fn append_and_truncate_keep_runs_consistent() {
        let mut r = Rendered::render("**a**");
        r.append("**b**");
        assert_eq!(r.text, "a\n\nb");
        assert_eq!(runs_of(&r, flags::BOLD), vec!["a", "b"]);
        r.truncate(1);
        assert_eq!(r.text, "a");
        assert_eq!(r.runs.len(), 1);
        r.push_plain(" tail");
        assert_eq!(r.runs_from(0), 0);
        assert_eq!(r.runs_from(1), 1);
    }

    #[test]
    fn committed_boundary_holds_fences_and_tables_back() {
        assert_eq!(committed_boundary("para one\n\npara two"), 10);
        assert_eq!(committed_boundary("```\ncode\n\nmore\n"), 0);
        assert_eq!(committed_boundary("```\ncode\n```\n\nnext"), 14);
        assert_eq!(committed_boundary("x\n\n| a |\n|---|\n| 1 |\n"), 3);
        assert_eq!(committed_boundary("no boundary yet"), 0);
    }

    #[test]
    fn streaming_commits_at_blank_lines_and_keeps_the_tail_plain() {
        let mut m = Markdown::default();
        let c = m.push("Some **bo");
        assert_eq!((c, m.text()), (0, "Some **bo"));
        let c = m.push("ld** text.\n\nNext *para");
        assert_eq!(c, 0, "the first paragraph re-rendered");
        assert_eq!(m.text(), "Some bold text.\n\nNext *para");
        assert_eq!(m.runs().len(), 1);
        let c = m.push("graph*");
        assert_eq!(c, "Some bold text.\n\nNext *para".len());
        assert_eq!(m.text(), "Some bold text.\n\nNext *paragraph*");
        m.finalize();
        assert_eq!(m.text(), "Some bold text.\n\nNext paragraph");
        assert_eq!(m.runs().len(), 2);
        assert!(m.raw().is_empty());
        // Finalizing twice is a no-op.
        m.finalize();
        assert_eq!(m.text(), "Some bold text.\n\nNext paragraph");
    }

    #[test]
    fn commit_all_renders_the_tail_and_keeps_streaming_possible() {
        let mut m = Markdown::default();
        m.push("a\n\nsee [x](https://e.com)");
        assert_eq!(m.text(), "a\n\nsee [x](https://e.com)");
        let c = m.commit_all();
        assert_eq!((c, m.text()), (1, "a\n\nsee x (https://e.com)"));
        assert_eq!(m.commit_all(), m.text().len(), "nothing new: no change");
        m.push("\n\nmore *t*");
        assert_eq!(m.text(), "a\n\nsee x (https://e.com)\n\nmore *t*");
    }

    #[test]
    fn replace_from_restarts_the_stream() {
        let mut m = Markdown::default();
        m.push("First.\n\n`code` tail");
        let base = m.raw().len();
        m.push("\n\nstreamed");
        let c = m.replace_from(base, "final **msg**");
        assert_eq!(m.text(), "First.\n\ncode tail\n\nfinal **msg**");
        assert_eq!(c, "First.\n\ncode tail".len());
        m.replace_from(0, "only");
        assert_eq!(m.text(), "only");
        assert!(m.runs().is_empty());
    }

    #[test]
    fn garbage_never_panics() {
        for s in [
            "",
            "```",
            "| |",
            "> > >",
            "- \n- ",
            "[x](",
            "#",
            "\n\n\n",
            "a\n```\nb",
        ] {
            let _ = Rendered::render(s);
            let _ = committed_boundary(s);
        }
    }
}
