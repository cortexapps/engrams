//! Drawing. One frame function per screen mode; both paint from the same
//! `App`. The transcript walk starts at the bottom and lays out only the
//! cells the viewport reaches (ADR 0124 D3). Steady-state frames allocate
//! nothing: rows are painted grapheme by grapheme into the buffer.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use unicode_segmentation::UnicodeSegmentation;

use crate::app::{shell_pane_rows, App, Attached, NewSession, NewStage, Screen};
use crate::keymap::BINDINGS;
use crate::markdown::{flags, Run};
use crate::sessions::{age, marker};
use crate::transcript::{explore_running, Cell, Entry, Marker, Src, ToolState};
use crate::wrap::{display_width, grapheme_width};

/// The styles, resolved once from `NO_COLOR`.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub user: Style,
    pub agent: Style,
    pub dim: Style,
    pub tool: Style,
    pub err: Style,
    pub warn: Style,
    pub accent: Style,
    pub selected: Style,
    pub pending: Style,
    /// The band behind a prompt: the row is filled with it edge to edge.
    pub band: Style,
    pub code: Style,
    pub heading: Style,
    pub link: Style,
    pub add: Style,
    pub del: Style,
    /// Screen-reader mode: word gutters (`agent:`) instead of glyphs.
    pub ax: bool,
}

/// Width of the gutter column: a glyph and a space, or a word.
pub const GUTTER: u16 = 2;
pub const GUTTER_AX: u16 = 10;

impl Theme {
    pub fn new(color: bool) -> Self {
        let c = |s: Style, col: Color| if color { s.fg(col) } else { s };
        Theme {
            code: c(Style::new(), Color::Cyan),
            heading: c(Style::new().add_modifier(Modifier::BOLD), Color::Green),
            link: c(Style::new().add_modifier(Modifier::UNDERLINED), Color::Blue),
            add: c(Style::new(), Color::Green),
            del: c(Style::new(), Color::Red),
            ax: false,
            user: Style::new().add_modifier(Modifier::BOLD),
            agent: Style::new(),
            dim: c(Style::new().add_modifier(Modifier::DIM), Color::DarkGray),
            tool: c(Style::new(), Color::Cyan),
            err: c(Style::new(), Color::Red),
            warn: c(Style::new(), Color::Yellow),
            accent: c(Style::new().add_modifier(Modifier::BOLD), Color::Green),
            selected: Style::new().add_modifier(Modifier::REVERSED),
            pending: c(Style::new().add_modifier(Modifier::DIM), Color::DarkGray),
            band: if color {
                Style::new().bg(Color::DarkGray)
            } else {
                Style::new()
            },
        }
    }
}

impl Theme {
    pub fn gutter_width(&self) -> u16 {
        if self.ax {
            GUTTER_AX
        } else {
            GUTTER
        }
    }

    /// The style of a markdown run.
    fn run_style(&self, base: Style, f: u8) -> Style {
        let mut st = base;
        if f & flags::DIM != 0 {
            st = st.patch(self.dim);
        }
        if f & flags::CODE != 0 {
            st = st.patch(self.code);
        }
        if f & flags::HEADING != 0 {
            st = st.patch(self.heading);
        }
        if f & flags::LINK != 0 {
            st = st.patch(self.link);
        }
        if f & flags::BOLD != 0 {
            st = st.add_modifier(Modifier::BOLD);
        }
        if f & (flags::ITALIC | flags::QUOTE) != 0 {
            st = st.add_modifier(Modifier::ITALIC);
        }
        if f & flags::STRIKE != 0 {
            st = st.add_modifier(Modifier::CROSSED_OUT);
        }
        st
    }
}

/// Text with its style runs (a markdown cell's display text).
#[derive(Clone, Copy)]
struct Styled<'a> {
    text: &'a str,
    runs: &'a [Run],
}

/// Paint `text[start..end]` with its style runs. Zero allocations: the
/// first run is found by binary search, the rest are walked.
fn put_runs(
    buf: &mut Buffer,
    at: Position,
    styled: Styled<'_>,
    (start, end): (usize, usize),
    max_width: u16,
    base: Style,
    th: &Theme,
) -> u16 {
    let Styled { text, runs } = styled;
    let (x, y) = (at.x, at.y);
    let mut cx = 0u16;
    let mut pos = start;
    let mut i = runs.partition_point(|r| r.end as usize <= start);
    while pos < end && cx < max_width {
        let (seg_end, style) = match runs.get(i) {
            Some(r) if (r.start as usize) <= pos => {
                i += 1;
                ((r.end as usize).min(end), th.run_style(base, r.flags))
            }
            Some(r) => ((r.start as usize).min(end), base),
            None => (end, base),
        };
        if seg_end > pos {
            cx += put(buf, x + cx, y, &text[pos..seg_end], max_width - cx, style);
            pos = seg_end;
        }
    }
    cx
}

/// Case-insensitive find without allocating; `needle` is lowercase.
pub fn find_ci(hay: &str, needle: &str) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    'outer: for (start, _) in hay.char_indices() {
        let mut h = hay[start..].char_indices();
        let mut n = needle.chars();
        loop {
            let Some(nc) = n.next() else {
                let consumed = h.next().map(|(o, _)| o).unwrap_or(hay.len() - start);
                return Some((start, start + consumed));
            };
            let (_, hc) = h.next()?;
            let mut lower = hc.to_lowercase();
            if lower.next() != Some(nc) || lower.next().is_some() {
                continue 'outer;
            }
        }
    }
    None
}

/// Paint `text` at (x, y) clipped to `max_width` cells. Control characters
/// paint as a space. Returns the cells used.
pub fn put(buf: &mut Buffer, x: u16, y: u16, text: &str, max_width: u16, style: Style) -> u16 {
    let mut cx = x;
    let end = x.saturating_add(max_width);
    for g in text.graphemes(true) {
        let w = grapheme_width(g);
        if w == 0 {
            continue;
        }
        if cx + w > end {
            break;
        }
        if let Some(cell) = buf.cell_mut(Position::new(cx, y)) {
            if g.chars().any(char::is_control) {
                cell.set_char(' ');
            } else {
                cell.set_symbol(g);
            }
            cell.set_style(style);
        }
        for i in 1..w {
            if let Some(cell) = buf.cell_mut(Position::new(cx + i, y)) {
                cell.set_symbol("");
                cell.set_style(style);
            }
        }
        cx += w;
    }
    cx - x
}

fn fill(buf: &mut Buffer, area: Rect, style: Style) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(c) = buf.cell_mut(Position::new(x, y)) {
                c.set_char(' ');
                c.set_style(style);
            }
        }
    }
}

/// A dim rule across `area` (one row, or nothing when it has no height):
/// the edge of the composer, as in Claude Code.
fn draw_rule(buf: &mut Buffer, area: Rect, th: &Theme) {
    if area.height == 0 {
        return;
    }
    for x in area.left()..area.right() {
        if let Some(c) = buf.cell_mut(Position::new(x, area.y)) {
            c.set_symbol("─");
            c.set_style(th.dim);
        }
    }
}

/// Paint one layout row of a cell at `y`. `hl` is a lowercase search
/// query to highlight in this row.
fn paint_row(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    e: &Entry,
    row: usize,
    th: &Theme,
    hl: Option<&str>,
) {
    let layout_row = e.layout_row(row);
    let Some(lr) = layout_row else {
        return;
    };
    let first = row == e.first_row();
    let x = area.x;
    let w = area.width;
    let gw = th.gutter_width();
    if lr.src == Src::Fold {
        // The folded turn: `▸ Ran 6 shell commands`; `▾` once opened.
        let open = matches!(e.fold, crate::transcript::Fold::Head { expanded: true, .. });
        if th.ax {
            put(buf, x, y, "tools:", gw, th.dim);
        } else {
            put(buf, x, y, if open { "▾" } else { "▸" }, 1, th.dim);
        }
        let text = e.source(Src::Fold);
        let cx = put(buf, x + gw, y, text, w.saturating_sub(gw), th.dim);
        if !open && !th.ax {
            put(
                buf,
                x + gw + cx,
                y,
                "  (click or Ctrl+O)",
                w.saturating_sub(gw + cx),
                th.dim,
            );
        }
        return;
    }
    let (gutter, word, style): (&str, &str, Style) = match &e.cell {
        Cell::User {
            pending, steered, ..
        } => (
            if *steered { "↳" } else { "›" },
            "you:",
            if *pending { th.pending } else { th.user },
        ),
        Cell::Assistant { failed, .. } => ("", "agent:", if *failed { th.dim } else { th.agent }),
        Cell::Tool { state, .. } => (
            match state {
                ToolState::Running => "⚙",
                ToolState::Ok => "✓",
                ToolState::Err => "✗",
            },
            "tool:",
            match state {
                ToolState::Err => th.err,
                _ => th.tool,
            },
        ),
        Cell::Diff { .. } => ("±", "edit:", th.tool),
        Cell::Explore { items, .. } => (
            if explore_running(items) {
                "⚙"
            } else if items.iter().any(|i| i.state == ToolState::Err) {
                "✗"
            } else {
                "✓"
            },
            "tool:",
            th.tool,
        ),
        Cell::Exec { done, exit, .. } => (
            if !*done {
                "⚙"
            } else if matches!(exit, Some(0) | None) {
                "$"
            } else {
                "✗"
            },
            "shell:",
            th.tool,
        ),
        Cell::Question { answered, .. } => {
            ("?", "question:", if *answered { th.dim } else { th.warn })
        }
        Cell::Plan { approved, .. } => (
            "≡",
            "plan:",
            if approved.is_some() { th.dim } else { th.warn },
        ),
        Cell::Marker { kind, .. } => (
            "·",
            "note:",
            match kind {
                Marker::Rollback | Marker::ResumedFromDisk | Marker::PlanAttempt => th.warn,
                _ => th.dim,
            },
        ),
        Cell::Hidden => return,
    };
    // A prompt sits on a band across the row, as in Claude Code.
    if matches!(e.cell, Cell::User { .. }) {
        fill(
            buf,
            Rect {
                x,
                y,
                width: w,
                height: 1,
            },
            th.band,
        );
    }
    if first {
        if th.ax {
            put(buf, x, y, word, gw, style);
        } else if !gutter.is_empty() {
            put(buf, x, y, gutter, 1, style);
        }
    }
    let body = Rect {
        x: x + gw,
        y,
        width: w.saturating_sub(gw),
        height: 1,
    };
    let painted = paint_src(buf, e, lr, row, body, style, th);
    // The search hit: repaint the match reversed over what is there. Rows
    // with a painted prefix (a tool name, an option number) are matched
    // but not overlaid.
    let overlay = match lr.src {
        Src::Text | Src::Body => true,
        Src::Head => !matches!(e.cell, Cell::Tool { .. }),
        _ => false,
    };
    if let (true, Some(q), Some(src_text)) = (overlay, hl, painted) {
        let s = &src_text[lr.row.start as usize..lr.row.end as usize];
        if let Some((a, b)) = find_ci(s, q) {
            let off = display_width(&s[..a]) as u16;
            let shift = if lr.src == Src::Body { 2 } else { 0 };
            let hx = body.x + shift + off;
            if hx < body.right() {
                put(buf, hx, y, &s[a..b], body.right() - hx, th.selected);
            }
        }
    }
}

/// Paint the row's source text into the one-row `body` area; returns that
/// text when the row is searchable (so the caller can overlay a match).
fn paint_src<'e>(
    buf: &mut Buffer,
    e: &'e Entry,
    lr: &crate::transcript::LayoutRow,
    row: usize,
    body: Rect,
    style: Style,
    th: &Theme,
) -> Option<&'e str> {
    let (body_x, body_w, y) = (body.x, body.width, body.y);
    match lr.src {
        // Painted by `paint_row` before the cell's own gutter.
        Src::Fold => None,
        Src::Tail => {
            let tail_style = th.dim;
            match &e.cell {
                Cell::Tool { .. } | Cell::Diff { .. } => {
                    // A folded head sits at the head's indent; a folded
                    // body under its `⎿` gutter.
                    let after_head =
                        row > 0 && e.layout_row(row - 1).is_some_and(|p| p.src == Src::Head);
                    let shift = if after_head { 0 } else { 2 };
                    let folded = lr.row.end.saturating_sub(lr.row.start);
                    let mut cx = put(
                        buf,
                        body_x + shift,
                        y,
                        "… ",
                        body_w.saturating_sub(shift),
                        tail_style,
                    );
                    cx += put_num(buf, body_x + shift + cx, y, folded as u64, tail_style);
                    put(
                        buf,
                        body_x + shift + cx,
                        y,
                        " more lines (Ctrl+O)",
                        body_w.saturating_sub(shift + cx),
                        tail_style,
                    );
                }
                Cell::Exec { exit, done, .. } if lr.row.end > lr.row.start => {
                    let _ = (exit, done);
                    let folded = lr.row.end.saturating_sub(lr.row.start);
                    let mut cx = put(
                        buf,
                        body_x + 2,
                        y,
                        "… ",
                        body_w.saturating_sub(2),
                        tail_style,
                    );
                    cx += put_num(buf, body_x + 2 + cx, y, folded as u64, tail_style);
                    put(
                        buf,
                        body_x + 2 + cx,
                        y,
                        " more lines (Ctrl+O)",
                        body_w.saturating_sub(2 + cx),
                        tail_style,
                    );
                }
                Cell::Exec { exit, done, .. } => {
                    let text = match (done, exit) {
                        (false, _) => "running",
                        (true, Some(0)) => "exit 0",
                        (true, None) => "exited",
                        (true, Some(_)) => "exit ",
                    };
                    let mut cx = put(buf, body_x, y, text, body_w, tail_style);
                    if let (true, Some(code)) = (done, exit) {
                        if *code != 0 {
                            cx += put_num(buf, body_x + cx, y, code.unsigned_abs() as u64, th.err);
                        }
                    }
                    let _ = cx;
                }
                Cell::Plan {
                    approved, choice, ..
                } => match approved {
                    Some(true) => {
                        put(buf, body_x, y, "plan approved", body_w, tail_style);
                    }
                    Some(false) => {
                        put(buf, body_x, y, "plan rejected", body_w, tail_style);
                    }
                    None => {
                        let a = if *choice == Some(true) {
                            th.accent
                        } else {
                            tail_style
                        };
                        let r = if *choice == Some(false) {
                            th.accent
                        } else {
                            tail_style
                        };
                        let mut cx = put(buf, body_x, y, "1 approve", body_w, a);
                        cx += put(
                            buf,
                            body_x + cx,
                            y,
                            "  ",
                            body_w.saturating_sub(cx),
                            tail_style,
                        );
                        cx += put(
                            buf,
                            body_x + cx,
                            y,
                            "2 reject",
                            body_w.saturating_sub(cx),
                            r,
                        );
                        put(
                            buf,
                            body_x + cx,
                            y,
                            "  Tab feedback · Enter confirm",
                            body_w.saturating_sub(cx),
                            tail_style,
                        );
                    }
                },
                Cell::Question {
                    answered,
                    questions,
                    current,
                    ..
                } => {
                    let text = if *answered {
                        "answered"
                    } else if questions
                        .get(*current as usize)
                        .is_some_and(|q| q.multi_select)
                    {
                        "1-9 toggle · Enter confirm"
                    } else if questions.len() > 1 {
                        "1-9 pick · Enter next question"
                    } else {
                        "1-9 pick · Enter confirm"
                    };
                    put(buf, body_x, y, text, body_w, tail_style);
                }
                _ => {}
            }
            None
        }
        Src::Body => {
            let text = e.source(Src::Body);
            let s = &text[lr.row.start as usize..lr.row.end as usize];
            let is_first_body = row
                .checked_sub(1)
                .and_then(|r| e.layout_row(r))
                .is_none_or(|p| p.src != Src::Body);
            put(
                buf,
                body_x,
                y,
                if is_first_body && !th.ax { "⎿" } else { " " },
                1,
                th.dim,
            );
            if let Cell::Explore { .. } = &e.cell {
                // `Verb targets…`: the verb carries the tool colour on the
                // line's first row; wrapped rows are plain.
                let line_start = lr.row.start == 0
                    || text.as_bytes().get(lr.row.start as usize - 1) == Some(&b'\n');
                let verb_end = if line_start {
                    s.find(' ').unwrap_or(s.len())
                } else {
                    0
                };
                let mut cx = put(
                    buf,
                    body_x + 2,
                    y,
                    &s[..verb_end],
                    body_w.saturating_sub(2),
                    th.tool,
                );
                cx += put(
                    buf,
                    body_x + 2 + cx,
                    y,
                    &s[verb_end..],
                    body_w.saturating_sub(2 + cx),
                    th.dim,
                );
                let _ = cx;
                return Some(text);
            }
            let body_style = match &e.cell {
                Cell::Diff { .. } => {
                    // The line's first byte says what kind of line it is;
                    // a wrapped continuation row inherits it.
                    let line_start = text[..lr.row.start as usize]
                        .rfind('\n')
                        .map(|p| p + 1)
                        .unwrap_or(0);
                    match text.as_bytes().get(line_start) {
                        Some(b'+') => th.add,
                        Some(b'-') => th.del,
                        _ => th.dim,
                    }
                }
                _ => th.dim,
            };
            put(buf, body_x + 2, y, s, body_w.saturating_sub(2), body_style);
            Some(text)
        }
        Src::Head => {
            let mut cx = 0;
            // The tool's name, or the edit's verb, leads the first row.
            let lead = match &e.cell {
                Cell::Tool { name, .. } => Some(&**name),
                Cell::Diff { verb, .. } => Some(*verb),
                _ => None,
            };
            if let (Some(lead), true) = (lead, lr.row.start == 0) {
                cx = put(
                    buf,
                    body_x,
                    y,
                    lead,
                    body_w,
                    style.add_modifier(Modifier::BOLD),
                );
                cx += put(buf, body_x + cx, y, " ", body_w.saturating_sub(cx), style);
            }
            let text = e.source(Src::Head);
            let s = &text[lr.row.start as usize..lr.row.end as usize];
            let head_style = match &e.cell {
                Cell::Diff { .. } | Cell::Explore { .. } => style.add_modifier(Modifier::BOLD),
                _ => th.dim,
            };
            cx += put(
                buf,
                body_x + cx,
                y,
                s,
                body_w.saturating_sub(cx),
                head_style,
            );
            if let Cell::Diff { added, removed, .. } = &e.cell {
                if lr.row.end as usize == text.len() {
                    cx += put(
                        buf,
                        body_x + cx,
                        y,
                        "  +",
                        body_w.saturating_sub(cx),
                        th.add,
                    );
                    cx += put_num(buf, body_x + cx, y, *added as u64, th.add);
                    cx += put(buf, body_x + cx, y, " −", body_w.saturating_sub(cx), th.del);
                    cx += put_num(buf, body_x + cx, y, *removed as u64, th.del);
                }
            }
            let _ = cx;
            Some(text)
        }
        Src::Question(_) => {
            let text = e.source(lr.src);
            let s = &text[lr.row.start as usize..lr.row.end as usize];
            put(
                buf,
                body_x,
                y,
                s,
                body_w,
                style.add_modifier(Modifier::BOLD),
            );
            Some(text)
        }
        Src::Option(qi, j) => {
            let text = e.source(lr.src);
            let s = &text[lr.row.start as usize..lr.row.end as usize];
            let mut cx = 0;
            let (picked, multi, live, current) = match &e.cell {
                Cell::Question {
                    questions,
                    picks,
                    answered,
                    current,
                    ..
                } => (
                    picks.get(qi as usize).is_some_and(|p| p.contains(&j)),
                    questions.get(qi as usize).is_some_and(|q| q.multi_select),
                    !*answered,
                    *current == qi,
                ),
                _ => (false, false, false, false),
            };
            if lr.row.start == 0 && !live {
                cx = put(
                    buf,
                    body_x + 2,
                    y,
                    "answer: ",
                    body_w.saturating_sub(2),
                    th.dim,
                );
            } else if lr.row.start == 0 {
                cx = put_num(buf, body_x + 2, y, j as u64 + 1, th.dim);
                cx += put(buf, body_x + 2 + cx, y, ". ", body_w, th.dim);
                if live || picked {
                    let mark = match (multi, picked) {
                        (true, true) => "[x] ",
                        (true, false) => "[ ] ",
                        (false, true) => "(•) ",
                        (false, false) => "( ) ",
                    };
                    let ms = if picked { th.accent } else { th.dim };
                    cx += put(
                        buf,
                        body_x + 2 + cx,
                        y,
                        mark,
                        body_w.saturating_sub(2 + cx),
                        ms,
                    );
                }
            }
            let os = if !live {
                th.accent
            } else if picked {
                style.add_modifier(Modifier::BOLD)
            } else if live && !current {
                th.dim
            } else {
                style
            };
            put(
                buf,
                body_x + 2 + cx,
                y,
                s,
                body_w.saturating_sub(2 + cx),
                os,
            );
            Some(text)
        }
        Src::Text => {
            let text = e.source(Src::Text);
            let runs = e.runs();
            if runs.is_empty() {
                let s = &text[lr.row.start as usize..lr.row.end as usize];
                put(buf, body_x, y, s, body_w, style);
            } else {
                put_runs(
                    buf,
                    Position::new(body_x, y),
                    Styled { text, runs },
                    (lr.row.start as usize, lr.row.end as usize),
                    body_w,
                    style,
                    th,
                );
            }
            Some(text)
        }
    }
}

/// Paint a number without allocating.
fn put_num(buf: &mut Buffer, x: u16, y: u16, mut n: u64, style: Style) -> u16 {
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    if n == 0 {
        i -= 1;
        digits[i] = b'0';
    }
    while n > 0 {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    let s = std::str::from_utf8(&digits[i..]).unwrap_or("");
    put(buf, x, y, s, s.len() as u16, style)
}

/// The rows of the transcript that fit `area`, newest at the bottom,
/// honoring `scroll_up`. Returns whether a scroll up reached the top of
/// memory (the caller may page older history in).
pub fn draw_transcript(
    buf: &mut Buffer,
    area: Rect,
    a: &mut Attached,
    th: &Theme,
    scratch: &mut Vec<(usize, u16)>,
) -> bool {
    let width = area.width.saturating_sub(th.gutter_width()).max(1);
    let want = area.height as usize;
    let hl = a
        .search
        .as_ref()
        .and_then(|s| s.cursor.map(|c| (c, s.lower.as_str())));
    let t = &mut a.reducer.transcript;
    let first = t.first_index();
    // Walk bottom-up, skipping `scroll_up` rows. A scroll past the top of
    // memory leaves the walk short of a page: clamp so the top of the
    // transcript fills the area and walk again in the same frame, so it
    // is painted now and not on some later redraw.
    let asked = a.scroll_up > 0;
    let mut clamped = false;
    let mut ran_out;
    loop {
        scratch.clear();
        let mut skip = a.scroll_up;
        let mut i = t.len();
        ran_out = true;
        while i > first {
            i -= 1;
            let Some(e) = t.get_mut(i) else {
                break;
            };
            let h = e.height(width);
            let mut row = h;
            while row > 0 {
                row -= 1;
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                scratch.push((i, row));
                if scratch.len() >= want {
                    ran_out = false;
                    break;
                }
            }
            if scratch.len() >= want {
                ran_out = false;
                break;
            }
        }
        if ran_out {
            let total = (a.scroll_up - skip) as usize + scratch.len();
            let clamp = total.saturating_sub(want) as u32;
            if a.scroll_up > clamp {
                a.scroll_up = clamp;
                clamped = true;
                continue;
            }
        }
        break;
    }
    a.hits.clear();
    a.hits_y = area.y;
    for &(idx, _) in scratch.iter().rev() {
        a.hits.push(idx);
    }
    // Paint top-down. A short transcript is top-aligned.
    for (y, &(idx, row)) in (area.y..).zip(scratch.iter().rev()) {
        if let Some(e) = t.get_mut(idx) {
            let _ = e.height(width);
            let q = hl.filter(|&(c, _)| c == idx).map(|(_, q)| q);
            paint_row(buf, area, y, e, row as usize, th, q);
        }
    }
    if a.unseen > 0 && a.scroll_up > 0 && area.height > 0 {
        let y = area.bottom() - 1;
        let mut cx = put(buf, area.x, y, " ↓ ", 3, th.accent);
        cx += put_num(buf, area.x + cx, y, a.unseen as u64, th.accent);
        put(buf, area.x + cx, y, " new (Ctrl+End) ", 16, th.accent);
    }
    if !a.loaded && scratch.is_empty() {
        put(
            buf,
            area.x + 2,
            area.y,
            "loading…",
            area.width.saturating_sub(2),
            th.dim,
        );
    } else if a.loading_older {
        put(
            buf,
            area.x + 2,
            area.y,
            "loading older…",
            area.width.saturating_sub(2),
            th.dim,
        );
    }
    // A scroll up that reached the top of memory asks for older history.
    asked && (ran_out || clamped)
}

fn draw_queue(buf: &mut Buffer, area: Rect, a: &Attached, th: &Theme) {
    for (y, (_, summary)) in (area.top()..area.bottom()).zip(&a.reducer.queued) {
        put(buf, area.x, y, "›", 1, th.pending);
        let cx = put(
            buf,
            area.x + 2,
            y,
            summary,
            area.width.saturating_sub(2),
            th.pending,
        );
        put(
            buf,
            area.x + 2 + cx,
            y,
            "  queued · ↑ pulls back",
            area.width.saturating_sub(2 + cx),
            th.dim,
        );
    }
}

/// Paint the composer; returns the cursor position.
fn draw_composer(buf: &mut Buffer, area: Rect, a: &mut Attached, th: &Theme) -> Position {
    let width = area.width.saturating_sub(2).max(1);
    let style = if a.can_send() { th.user } else { th.dim };
    let running = a.reducer.is_running();
    let can_send = a.can_send();
    let (crow, ccol) = a.composer.layout(width);
    let rows = a.composer.rows();
    let text = a.composer.text();
    // Keep the cursor row visible when the draft is taller than the area.
    let visible = area.height as usize;
    let top = (crow as usize + 1).saturating_sub(visible);
    put(buf, area.x, area.y, "›", 1, style);
    for (y, r) in (area.top()..area.bottom()).zip(rows.iter().skip(top)) {
        let s = &text[r.start as usize..r.end as usize];
        put(buf, area.x + 2, y, s, width, Style::new());
    }
    if text.is_empty() {
        let hint = if a.feedback {
            "feedback for the agent · Enter rejects the plan with it · Esc cancels"
        } else if a.reducer.awaiting_decision() {
            "answer above, or type to queue a message"
        } else if running {
            "type to queue a message · Esc interrupts"
        } else if !can_send {
            "this session is finished"
        } else {
            "message the agent"
        };
        put(buf, area.x + 2, area.y, hint, width, th.dim);
    }
    Position::new(
        area.x + 2 + ccol.min(width.saturating_sub(1)),
        area.y + (crow as usize - top) as u16,
    )
}

fn status_word(a: &Attached) -> &'static str {
    if a.reducer.waking {
        return "waking";
    }
    if a.reconnecting.is_some() {
        return "reconnecting";
    }
    if a.reducer.awaiting_decision() {
        return "needs you";
    }
    if a.reducer.is_running() {
        return "working";
    }
    match a.reducer.status.as_deref() {
        Some("active") => "ready",
        Some("idle") => "idle",
        Some("parked") => "parked",
        Some("evicting") => "evicting",
        Some("pending") | Some("queued") | Some("created") => "booting",
        Some("failed") => "failed",
        Some("dead") => "dead",
        Some("completed") => "completed",
        Some("unreachable") => "unreachable",
        Some("host_lost") => "host lost",
        Some("evacuating") => "moving",
        _ => "",
    }
}

/// The footer's state glyph: one of five marks that read without colour.
fn status_glyph(a: &Attached) -> &'static str {
    if a.reducer.waking || a.reconnecting.is_some() {
        return "◐";
    }
    if a.reducer.awaiting_decision() {
        return "?";
    }
    if a.reducer.is_running() {
        return "●";
    }
    match a.reducer.status.as_deref() {
        Some("failed") | Some("dead") | Some("host_lost") => "✕",
        Some("completed") => "✓",
        Some("pending") | Some("queued") | Some("created") | Some("evacuating") => "◐",
        _ => "○",
    }
}

fn draw_footer(buf: &mut Buffer, area: Rect, app: &App, th: &Theme) {
    let y = area.y;
    let w = area.width;
    fill(buf, area, th.dim);
    let mut cx = 0u16;
    if let Some((note, _)) = &app.note {
        cx += put(buf, area.x, y, " ", 1, th.warn);
        put(buf, area.x + cx, y, note, w.saturating_sub(cx), th.warn);
        return;
    }
    if let Some(s) = app.attached.as_ref().and_then(|a| a.search.as_ref()) {
        cx += put(buf, area.x, y, " /", 2, th.accent);
        cx += put(
            buf,
            area.x + cx,
            y,
            &s.query,
            w.saturating_sub(cx + 1),
            th.user,
        );
        let hint = if s.cursor.is_some() {
            "  Enter next (older) · Esc close"
        } else if s.query.is_empty() {
            "  type to search · Esc close"
        } else {
            "  Enter search · Esc close"
        };
        put(
            buf,
            area.x + cx + 1,
            y,
            hint,
            w.saturating_sub(cx + 1),
            th.dim,
        );
        return;
    }
    if let Some(ns) = &app.new_session {
        let hint = match ns.stage {
            NewStage::Pick => " new session  ↑↓ pick · type to filter · Enter choose · Esc close",
            NewStage::Prompt => " new session  Enter create · Shift+Enter newline · Esc back",
        };
        put(buf, area.x, y, hint, w, th.dim);
        return;
    }
    match (&app.attached, app.screen) {
        (Some(a), Screen::Attached) => {
            let s = status_word(a);
            let st = if a.reducer.awaiting_decision() {
                th.warn
            } else if a.reducer.is_running() {
                th.accent
            } else {
                th.dim
            };
            cx += put(buf, area.x + cx, y, " ", 1, st);
            cx += put(buf, area.x + cx, y, status_glyph(a), 1, st);
            cx += put(buf, area.x + cx, y, " ", 1, st);
            cx += put(buf, area.x + cx, y, s, w.saturating_sub(cx), st);
            if a.composer_mode() == "plan" {
                cx += put(buf, area.x + cx, y, " · plan", 7, th.warn);
            }
            if let Some(n) = a.reconnecting {
                cx += put(buf, area.x + cx, y, " (", 2, st);
                cx += put_num(buf, area.x + cx, y, n as u64, st);
                cx += put(buf, area.x + cx, y, ")", 1, st);
            }
            if let Some(t) = a.run_started_at {
                let secs = t.elapsed().as_secs();
                cx += put(buf, area.x + cx, y, " · ", 3, th.dim);
                cx += put_num(buf, area.x + cx, y, secs, th.dim);
                cx += put(buf, area.x + cx, y, "s", 1, th.dim);
            }
            if let Some(e) = &a.error {
                cx += put(buf, area.x + cx, y, " · ", 3, th.err);
                cx += put(buf, area.x + cx, y, e, w.saturating_sub(cx), th.err);
            }
            let hints = if a.shell.as_ref().is_some_and(|s| s.focused) {
                "  shell has the keys · Ctrl+` back to the agent"
            } else if a.shell.is_some() {
                "  Ctrl+` shell · Esc closes it · Ctrl+D sessions · ? keys"
            } else if a.reducer.is_running() {
                "  Esc interrupt · Enter queue · Ctrl+D sessions · ? keys"
            } else {
                "  Enter send · Ctrl+D sessions · Ctrl+P switch · ? keys"
            };
            let short = if a.shell.as_ref().is_some_and(|s| s.focused) {
                "  Ctrl+` back to the agent"
            } else {
                "  Ctrl+D sessions · ? keys"
            };
            let avail = w.saturating_sub(cx + 1);
            if avail as usize >= hints.len() {
                put(buf, area.x + cx, y, hints, avail, th.dim);
            } else if avail as usize >= short.len() {
                put(buf, area.x + cx, y, short, avail, th.dim);
            }
        }
        _ => {
            let n = app.sessions.rows.len();
            cx += put(buf, area.x, y, " ", 1, th.dim);
            cx += put_num(buf, area.x + cx, y, n as u64, th.dim);
            cx += put(buf, area.x + cx, y, " sessions", 9, th.dim);
            if app.sessions.loading {
                cx += put(buf, area.x + cx, y, " · loading…", 11, th.dim);
            }
            if let Some(e) = &app.sessions.error {
                cx += put(buf, area.x + cx, y, " · ", 3, th.err);
                cx += put(buf, area.x + cx, y, e, w.saturating_sub(cx), th.err);
            }
            put(
                buf,
                area.x + cx,
                y,
                "  type to filter · Enter attach · Esc quit · ? keys",
                w.saturating_sub(cx),
                th.dim,
            );
        }
    }
}

fn draw_sessions(buf: &mut Buffer, area: Rect, app: &App, th: &Theme, title: &str) {
    if area.height == 0 {
        return;
    }
    let y0 = area.y;
    let mut cx = put(
        buf,
        area.x + 1,
        y0,
        title,
        area.width.saturating_sub(1),
        th.accent,
    );
    if !app.sessions.filter.is_empty() || app.palette {
        cx += put(buf, area.x + 1 + cx, y0, "  / ", 4, th.dim);
        put(
            buf,
            area.x + 1 + cx,
            y0,
            &app.sessions.filter,
            area.width.saturating_sub(1 + cx),
            th.user,
        );
    }
    let list = Rect {
        y: y0 + 1,
        height: area.height.saturating_sub(1),
        ..area
    };
    let n = list.height as usize;
    let selected = app.sessions.selected;
    let top = (selected + 1).saturating_sub(n);
    let mut y = list.y;
    let mut shown = 0usize;
    for (vis, i) in app.sessions.visible().enumerate() {
        if vis < top {
            continue;
        }
        if y >= list.bottom() {
            break;
        }
        let r = &app.sessions.rows[i];
        let style = if vis == selected {
            th.selected
        } else {
            Style::new()
        };
        fill(
            buf,
            Rect {
                y,
                height: 1,
                ..list
            },
            style,
        );
        // A session that waits on the user shows the question mark the
        // footer uses for a pending decision.
        let mark = if r.needs_you { "?" } else { marker(&r.status) };
        let mut cx = put(buf, list.x + 1, y, mark, 1, style);
        cx += put(buf, list.x + 1 + cx, y, " ", 1, style);
        let age_s = age(r.age_secs);
        let title_w = list
            .width
            .saturating_sub(1 + cx + 1 + age_s.len() as u16 + 1);
        put(buf, list.x + 1 + cx, y, &r.title, title_w, style);
        put(
            buf,
            list.right().saturating_sub(age_s.len() as u16 + 1),
            y,
            &age_s,
            age_s.len() as u16,
            style.patch(th.dim),
        );
        y += 1;
        shown += 1;
    }
    if shown == 0 && !app.sessions.loading {
        let msg = if app.auth_failed {
            "the API key was refused · run engrams auth login"
        } else if app.sessions.rows.is_empty() && app.sessions.error.is_some() {
            "could not load the sessions"
        } else if app.sessions.rows.is_empty() {
            "no sessions yet · engrams task create --profile <name> --prompt …"
        } else {
            "no match"
        };
        put(
            buf,
            list.x + 1,
            list.y,
            msg,
            list.width.saturating_sub(1),
            th.dim,
        );
    }
}

/// Rows the slash-command popup takes above the composer: one per match,
/// at most six. Zero when the draft is not a command prefix.
pub fn slash_rows(draft: &str) -> u16 {
    crate::commands::matches(draft).count().min(6) as u16
}

/// The slash-command popup: the matching commands, the first one (what
/// Tab completes to and Enter runs when it is the only match) marked.
fn draw_slash(buf: &mut Buffer, area: Rect, draft: &str, th: &Theme) {
    if area.height == 0 {
        return;
    }
    fill(buf, area, Style::new());
    for (i, c) in crate::commands::matches(draft)
        .take(area.height as usize)
        .enumerate()
    {
        let y = area.y + i as u16;
        let style = if i == 0 { th.selected } else { Style::new() };
        let line = Rect {
            y,
            height: 1,
            ..area
        };
        fill(buf, line, style);
        let mut x = area.x + 2;
        x += put(buf, x, y, "/", 1, style);
        x += put(buf, x, y, c.name, 12, style);
        let pad = 14u16.saturating_sub(x - area.x);
        x += put(buf, x, y, "              ", pad, style);
        put(
            buf,
            x,
            y,
            c.help,
            area.right().saturating_sub(x + 1),
            style.patch(th.dim),
        );
    }
}

/// The new-session overlay: the profile list with its filter, or the
/// prompt composer under the chosen profile. Returns the cursor.
fn draw_new_session(
    buf: &mut Buffer,
    area: Rect,
    ns: &mut NewSession,
    th: &Theme,
) -> Option<Position> {
    if area.height < 2 {
        return None;
    }
    fill(buf, area, Style::new());
    let y0 = area.y;
    let mut cx = put(
        buf,
        area.x + 1,
        y0,
        "new session",
        area.width.saturating_sub(1),
        th.accent,
    );
    let inner = Rect {
        y: y0 + 1,
        height: area.height - 1,
        ..area
    };
    match ns.stage {
        NewStage::Pick => {
            cx += put(buf, area.x + 1 + cx, y0, "  / ", 4, th.dim);
            put(
                buf,
                area.x + 1 + cx,
                y0,
                &ns.filter,
                area.width.saturating_sub(1 + cx),
                th.user,
            );
            if let Some(e) = &ns.error {
                put(
                    buf,
                    inner.x + 1,
                    inner.y,
                    e,
                    inner.width.saturating_sub(1),
                    th.err,
                );
                return None;
            }
            if ns.loading {
                put(buf, inner.x + 1, inner.y, "loading profiles…", 17, th.dim);
                return None;
            }
            let n = inner.height as usize;
            let selected = ns.selected;
            let first = selected.saturating_sub(n.saturating_sub(1));
            let mut shown = 0usize;
            for (row, i) in ns.visible().skip(first).take(n).enumerate() {
                let p = &ns.profiles[i];
                let y = inner.y + row as u16;
                let style = if first + row == selected {
                    th.selected
                } else {
                    Style::new()
                };
                let line = Rect {
                    y,
                    height: 1,
                    ..inner
                };
                fill(buf, line, style);
                let mut x = inner.x + 1;
                x += put(buf, x, y, &p.name, inner.width.saturating_sub(1), style);
                if !p.description.is_empty() && x + 3 < inner.right() {
                    x += put(buf, x, y, "  ", 2, style);
                    put(
                        buf,
                        x,
                        y,
                        &p.description,
                        inner.right().saturating_sub(x + 1),
                        style.patch(th.dim),
                    );
                }
                shown += 1;
            }
            if shown == 0 {
                let msg = if ns.profiles.is_empty() {
                    "no profiles; an admin must create one"
                } else {
                    "no profile matches"
                };
                put(
                    buf,
                    inner.x + 1,
                    inner.y,
                    msg,
                    inner.width.saturating_sub(1),
                    th.dim,
                );
            }
            None
        }
        NewStage::Prompt => {
            if let Some(p) = ns.selected_profile() {
                cx += put(buf, area.x + 1 + cx, y0, " · ", 3, th.dim);
                put(
                    buf,
                    area.x + 1 + cx,
                    y0,
                    &p.name,
                    area.width.saturating_sub(1 + cx),
                    th.user,
                );
            }
            let width = inner.width.saturating_sub(3).max(1);
            let (crow, ccol) = ns.composer.layout(width);
            let rows = ns.composer.rows();
            let text = ns.composer.text();
            let visible = inner.height.saturating_sub(1) as usize;
            let top = (crow as usize + 1).saturating_sub(visible.max(1));
            put(buf, inner.x + 1, inner.y, "›", 1, th.user);
            for (y, r) in (inner.top()..inner.bottom().saturating_sub(1)).zip(rows.iter().skip(top))
            {
                let line = &text[r.start as usize..r.end as usize];
                put(buf, inner.x + 3, y, line, width, Style::new());
            }
            if text.is_empty() {
                put(
                    buf,
                    inner.x + 3,
                    inner.y,
                    "describe the task",
                    width,
                    th.dim,
                );
            }
            let hint_y = inner.bottom() - 1;
            if let Some(e) = &ns.error {
                put(
                    buf,
                    inner.x + 1,
                    hint_y,
                    e,
                    inner.width.saturating_sub(1),
                    th.err,
                );
                return None;
            }
            if ns.creating {
                put(buf, inner.x + 1, hint_y, "creating…", 9, th.dim);
            }
            Some(Position::new(
                inner.x + 3 + ccol.min(width.saturating_sub(1)),
                inner.y + (crow as usize - top) as u16,
            ))
        }
    }
}

fn draw_help(buf: &mut Buffer, area: Rect, th: &Theme) {
    let h = (BINDINGS.len() as u16 + 2).min(area.height);
    // Full width on a narrow terminal so no transcript shows through.
    let w = if area.width <= 96 { area.width } else { 80 };
    let r = Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    };
    fill(buf, r, Style::new());
    put(buf, r.x + 1, r.y, "keys", 4, th.accent);
    put(buf, r.x + 6, r.y, "any key closes", 14, th.dim);
    for (i, b) in BINDINGS.iter().enumerate() {
        let y = r.y + 1 + i as u16;
        if y >= r.bottom() {
            break;
        }
        put(buf, r.x + 1, y, b.keys, 26, th.user);
        put(buf, r.x + 28, y, b.help, w.saturating_sub(29), Style::new());
    }
}

/// Fullscreen frame: transcript, queue rail, composer, footer; the
/// session list or palette over it. Returns the cursor position and
/// whether the transcript walk wants older history.
pub fn draw_fullscreen(
    buf: &mut Buffer,
    area: Rect,
    app: &mut App,
    th: &Theme,
    scratch: &mut Vec<(usize, u16)>,
) -> (Option<Position>, bool) {
    fill(buf, area, Style::new());
    if area.height < 2 {
        return (None, false);
    }
    let footer = Rect {
        y: area.bottom() - 1,
        height: 1,
        ..area
    };
    let body = Rect {
        height: area.height - 1,
        ..area
    };
    let mut cursor = None;
    let mut want_older = false;
    match app.screen {
        Screen::Sessions => draw_sessions(buf, body, app, th, "engrams · sessions"),
        Screen::Attached => {
            let a = app
                .attached
                .as_mut()
                .expect("attached screen has a session");
            let width = body.width.saturating_sub(2).max(1);
            a.composer.layout(width);
            let composer_rows = a.composer.rows().len().clamp(1, 8) as u16;
            let queue_rows = (a.reducer.queued.len() as u16).min(4);
            // The composer and its queue sit between two rules when the
            // screen has the rows for them.
            let rules = u16::from(body.height >= 8);
            let rule_below = Rect {
                y: body.bottom().saturating_sub(1),
                height: rules,
                ..body
            };
            let composer = Rect {
                y: body.bottom().saturating_sub(rules + composer_rows),
                height: composer_rows.min(body.height),
                ..body
            };
            let queue = Rect {
                y: composer.y.saturating_sub(queue_rows),
                height: queue_rows.min(composer.y - body.y),
                ..body
            };
            let rule_above = Rect {
                y: queue.y.saturating_sub(rules),
                height: rules,
                ..body
            };
            let title_h = u16::from(body.height > 4);
            let shell_rows = if a.shell.is_some() {
                shell_pane_rows(body.height).min(rule_above.y.saturating_sub(body.y + title_h))
            } else {
                0
            };
            let shell = Rect {
                y: rule_above.y.saturating_sub(shell_rows),
                height: shell_rows,
                ..body
            };
            let slash_h =
                slash_rows(a.composer.text()).min(shell.y.saturating_sub(body.y + title_h));
            let slash = Rect {
                y: shell.y.saturating_sub(slash_h),
                height: slash_h,
                ..body
            };
            let transcript = Rect {
                y: body.y + title_h,
                height: slash.y.saturating_sub(body.y + title_h),
                ..body
            };
            a.page_rows = transcript.height;
            if title_h == 1 {
                put(
                    buf,
                    body.x + 1,
                    body.y,
                    &a.title,
                    body.width.saturating_sub(1),
                    th.accent,
                );
            }
            want_older = draw_transcript(buf, transcript, a, th, scratch);
            if slash.height > 0 {
                draw_slash(buf, slash, a.composer.text(), th);
            }
            draw_rule(buf, rule_above, th);
            draw_queue(buf, queue, a, th);
            cursor = Some(draw_composer(buf, composer, a, th));
            draw_rule(buf, rule_below, th);
            if shell.height > 0 {
                if let Some(c) = draw_shell(buf, shell, a, th) {
                    cursor = Some(c);
                }
            }
            if let Some(s) = &a.search {
                let qx = 2 + display_width(&s.query) as u16;
                cursor = Some(Position::new(
                    footer.x + qx.min(footer.width.saturating_sub(1)),
                    footer.y,
                ));
            }
        }
    }
    draw_footer(buf, footer, app, th);
    if app.palette {
        let h = (app.sessions.rows.len() as u16 + 2).clamp(3, body.height.saturating_sub(2).max(3));
        let r = Rect {
            x: body.x + 2,
            y: body.y + 1,
            width: body.width.saturating_sub(4),
            height: h.min(body.height),
        };
        fill(buf, r, Style::new());
        draw_sessions(buf, r, app, th, "switch session");
        cursor = None;
    }
    if let Some(ns) = &mut app.new_session {
        let rows = match ns.stage {
            NewStage::Pick => ns.visible().count().max(1) as u16 + 1,
            NewStage::Prompt => 8,
        };
        let h = (rows + 1).clamp(3, body.height.saturating_sub(1).max(3));
        let r = Rect {
            y: body.y + 1,
            height: h.min(body.height),
            ..body
        };
        cursor = draw_new_session(buf, r, ns, th);
    }
    if app.help {
        draw_help(buf, body, th);
        cursor = None;
    }
    (cursor, want_older)
}

/// Inline (native scrollback) viewport: the in-flight cells, the queue
/// rail, the composer and the footer. Finalized cells were already
/// written above it by `commit_inline`.
pub fn draw_inline(
    buf: &mut Buffer,
    area: Rect,
    app: &mut App,
    th: &Theme,
    scratch: &mut Vec<(usize, u16)>,
) -> Option<Position> {
    fill(buf, area, Style::new());
    if area.height < 2 {
        return None;
    }
    let footer = Rect {
        y: area.bottom() - 1,
        height: 1,
        ..area
    };
    let body = Rect {
        height: area.height - 1,
        ..area
    };
    let mut cursor = None;
    match app.screen {
        Screen::Sessions => draw_sessions(buf, body, app, th, "engrams · sessions"),
        Screen::Attached => {
            let a = app
                .attached
                .as_mut()
                .expect("attached screen has a session");
            let width = body.width.saturating_sub(2).max(1);
            a.composer.layout(width);
            let composer_rows = a.composer.rows().len().clamp(1, 6) as u16;
            let queue_rows = (a.reducer.queued.len() as u16).min(2);
            let composer = Rect {
                y: body.bottom().saturating_sub(composer_rows),
                height: composer_rows.min(body.height),
                ..body
            };
            let queue = Rect {
                y: composer.y.saturating_sub(queue_rows),
                height: queue_rows.min(composer.y - body.y),
                ..body
            };
            // One rule above the composer; the viewport is short.
            let rule_above = Rect {
                y: queue.y.saturating_sub(1),
                height: u16::from(body.height >= 4 && queue.y > body.y),
                ..body
            };
            let slash_h = slash_rows(a.composer.text()).min(rule_above.y.saturating_sub(body.y));
            let slash = Rect {
                y: rule_above.y.saturating_sub(slash_h),
                height: slash_h,
                ..body
            };
            let live = Rect {
                y: body.y,
                height: slash.y.saturating_sub(body.y),
                ..body
            };
            a.scroll_up = 0;
            a.page_rows = live.height;
            draw_transcript(buf, live, a, th, scratch);
            if slash.height > 0 {
                draw_slash(buf, slash, a.composer.text(), th);
            }
            draw_rule(buf, rule_above, th);
            draw_queue(buf, queue, a, th);
            cursor = Some(draw_composer(buf, composer, a, th));
        }
    }
    draw_footer(buf, footer, app, th);
    if app.palette {
        fill(buf, body, Style::new());
        draw_sessions(buf, body, app, th, "switch session");
        cursor = None;
    }
    if let Some(ns) = &mut app.new_session {
        cursor = draw_new_session(buf, body, ns, th);
    }
    if app.help {
        draw_help(buf, body, th);
        cursor = None;
    }
    cursor
}

/// The shell pane: a header row and the emulated screen. Returns the
/// cursor when the pane has focus.
fn draw_shell(buf: &mut Buffer, area: Rect, a: &Attached, th: &Theme) -> Option<Position> {
    let Some(sh) = &a.shell else {
        return None;
    };
    let head = Rect { height: 1, ..area };
    let style = if sh.focused { th.selected } else { th.dim };
    fill(buf, head, style);
    let mut cx = put(buf, head.x, head.y, " shell", 6, style);
    if !sh.title.is_empty() {
        cx += put(buf, head.x + cx, head.y, " · ", 3, style);
        cx += put(
            buf,
            head.x + cx,
            head.y,
            &sh.title,
            head.width.saturating_sub(cx),
            style,
        );
    }
    let hint = match (&sh.ended, sh.focused) {
        (Some(_), true) => "  ended · Enter reopens · Esc closes · Ctrl+` back",
        (Some(_), false) => "  ended · Ctrl+` focus · Esc closes",
        (None, true) if !sh.connected => "  connecting… · Ctrl+` back",
        (None, true) => "  Ctrl+` back to the agent",
        (None, false) => "  Ctrl+` focus · Esc closes",
    };
    cx += put(
        buf,
        head.x + cx,
        head.y,
        hint,
        head.width.saturating_sub(cx),
        style,
    );
    if let Some(e) = &sh.ended {
        cx += put(buf, head.x + cx, head.y, " · ", 3, style);
        put(
            buf,
            head.x + cx,
            head.y,
            e,
            head.width.saturating_sub(cx),
            style,
        );
    }
    let screen = Rect {
        y: area.y + 1,
        height: area.height - 1,
        ..area
    };
    sh.paint(buf, screen);
    if sh.focused {
        sh.cursor(screen)
    } else {
        None
    }
}

/// Native scrollback mode shows the shell as an alternate-screen overlay:
/// the pane over the whole terminal, the footer below it.
pub fn draw_overlay(buf: &mut Buffer, area: Rect, app: &App, th: &Theme) -> Option<Position> {
    fill(buf, area, Style::new());
    if area.height < 2 {
        return None;
    }
    let footer = Rect {
        y: area.bottom() - 1,
        height: 1,
        ..area
    };
    let body = Rect {
        height: area.height - 1,
        ..area
    };
    let cursor = app
        .attached
        .as_ref()
        .and_then(|a| draw_shell(buf, body, a, th));
    draw_footer(buf, footer, app, th);
    cursor
}

/// Paint one finalized cell into a buffer `height` rows tall (the
/// `insert_before` callback for native scrollback mode).
pub fn paint_cell(buf: &mut Buffer, e: &mut Entry, th: &Theme) {
    let area = buf.area;
    let width = area.width.saturating_sub(th.gutter_width()).max(1);
    let h = e.height(width);
    for row in 0..h {
        paint_row(buf, area, area.y + row, e, row as usize, th, None);
    }
}
