//! Drawing. One frame function per screen mode; both paint from the same
//! `App`. The transcript walk starts at the bottom and lays out only the
//! cells the viewport reaches (ADR 0124 D3). Steady-state frames allocate
//! nothing: rows are painted grapheme by grapheme into the buffer.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use unicode_segmentation::UnicodeSegmentation;

use crate::app::{App, Attached, Screen};
use crate::keymap::BINDINGS;
use crate::markdown::{flags, Run};
use crate::sessions::{age, marker};
use crate::transcript::{Cell, Entry, Marker, Src, ToolState};
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
    let first = row == 0;
    let x = area.x;
    let w = area.width;
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
    let gw = th.gutter_width();
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
        Src::Tail => {
            let tail_style = th.dim;
            match &e.cell {
                Cell::Tool { .. } | Cell::Diff { .. } => {
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
            let is_first_body = e
                .layout_row(row.saturating_sub(1))
                .is_some_and(|p| p.src != Src::Body)
                || row == 0;
            put(
                buf,
                body_x,
                y,
                if is_first_body && !th.ax { "⎿" } else { " " },
                1,
                th.dim,
            );
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
            if let Cell::Tool { name, .. } = &e.cell {
                if lr.row.start == 0 {
                    cx = put(
                        buf,
                        body_x,
                        y,
                        name,
                        body_w,
                        style.add_modifier(Modifier::BOLD),
                    );
                    cx += put(buf, body_x + cx, y, " ", body_w.saturating_sub(cx), style);
                }
            }
            let text = e.source(Src::Head);
            let s = &text[lr.row.start as usize..lr.row.end as usize];
            let head_style = match &e.cell {
                Cell::Diff { .. } => style.add_modifier(Modifier::BOLD),
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
            if lr.row.start == 0 {
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
            let os = if picked {
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
/// honoring `scroll_up`. Returns whether the walk ran out of cells while
/// skipping (the caller may page older history in).
pub fn draw_transcript(
    buf: &mut Buffer,
    area: Rect,
    a: &mut Attached,
    th: &Theme,
    scratch: &mut Vec<(usize, u16)>,
) -> bool {
    scratch.clear();
    let width = area.width.saturating_sub(th.gutter_width()).max(1);
    let want = area.height as usize;
    let mut skip = a.scroll_up;
    let hl = a
        .search
        .as_ref()
        .and_then(|s| s.cursor.map(|c| (c, s.lower.as_str())));
    let t = &mut a.reducer.transcript;
    let first = t.first_index();
    let mut i = t.len();
    let mut ran_out = true;
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
    // The walk stopped above the top: clamp so the next frame is stable.
    if ran_out && skip > 0 {
        a.scroll_up -= skip;
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
    ran_out
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
    match (&app.attached, app.screen) {
        (Some(a), Screen::Attached) => {
            let m = marker(a.reducer.status.as_deref().unwrap_or(""));
            let s = status_word(a);
            let st = if a.reducer.awaiting_decision() {
                th.warn
            } else if a.reducer.is_running() {
                th.accent
            } else {
                th.dim
            };
            cx += put(buf, area.x + cx, y, " ", 1, st);
            cx += put(buf, area.x + cx, y, m, 1, st);
            cx += put(buf, area.x + cx, y, " ", 1, st);
            cx += put(buf, area.x + cx, y, s, w.saturating_sub(cx), st);
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
            let hints = if a.reducer.is_running() {
                "  Esc interrupt · Enter queue · Ctrl+D sessions · ? keys"
            } else {
                "  Enter send · Ctrl+D sessions · Ctrl+P switch · ? keys"
            };
            let short = "  Ctrl+D sessions · ? keys";
            let right_w = 14u16;
            let avail = w.saturating_sub(cx + right_w);
            if avail as usize >= hints.len() {
                cx += put(buf, area.x + cx, y, hints, avail, th.dim);
            } else if avail as usize >= short.len() {
                cx += put(buf, area.x + cx, y, short, avail, th.dim);
            }
            let _ = cx;
            if let Some(idx) = a.newest_idx {
                let rx = area.right().saturating_sub(right_w);
                let mut rc = put(buf, rx, y, " idx ", 5, th.dim);
                rc += put_num(buf, rx + rc, y, idx.max(0) as u64, th.dim);
                let _ = rc;
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
        let mut cx = put(buf, list.x + 1, y, marker(&r.status), 1, style);
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
        let msg = if app.sessions.rows.is_empty() {
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
            let title_h = u16::from(body.height > 4);
            let transcript = Rect {
                y: body.y + title_h,
                height: queue.y.saturating_sub(body.y + title_h),
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
            want_older = draw_transcript(buf, transcript, a, th, scratch) && a.scroll_up > 0;
            draw_queue(buf, queue, a, th);
            cursor = Some(draw_composer(buf, composer, a, th));
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
            let live = Rect {
                y: body.y,
                height: queue.y.saturating_sub(body.y),
                ..body
            };
            a.scroll_up = 0;
            a.page_rows = live.height;
            draw_transcript(buf, live, a, th, scratch);
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
    if app.help {
        draw_help(buf, body, th);
        cursor = None;
    }
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
