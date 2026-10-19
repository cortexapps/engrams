//! The transcript store (ADR 0124 D3): cells hold their text once; a
//! per-width layout of byte ranges is computed lazily and dropped on
//! resize. Rendering walks from the bottom and lays out only what the
//! viewport reaches.

use std::collections::VecDeque;

use crate::markdown::{Markdown, Rendered, Run};
use crate::wrap::{wrap_into, Row};

/// How a tool call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Ok,
    Err,
}

/// A status row between turns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marker {
    Note,
    Resumed,
    Snapshot,
    Recovered,
    Rollback,
    ResumedFromDisk,
    Mode,
    PlanAttempt,
}

/// One rendered unit of the transcript.
#[derive(Debug)]
pub enum Cell {
    User {
        text: String,
        /// The echo has not arrived yet (ADR 0108 delivery gap).
        pending: bool,
        steered: bool,
    },
    Assistant {
        /// The message as markdown: source while streaming, display text
        /// plus style runs always.
        md: Markdown,
        /// Token deltas are still landing; the final message replaces them.
        streaming: bool,
        /// More durable text may still be appended (the run is open and no
        /// boundary has closed this cell).
        open: bool,
        /// The run that produced it ended badly.
        failed: bool,
    },
    Tool {
        name: Box<str>,
        args: Box<str>,
        result: Option<Box<str>>,
        state: ToolState,
        duration_ms: Option<u64>,
        expanded: bool,
    },
    Exec {
        command: Box<str>,
        output: String,
        exit: Option<i32>,
        done: bool,
        expanded: bool,
    },
    Question {
        tool_call_id: Box<str>,
        questions: Vec<crate::event::Question>,
        answered: bool,
    },
    Plan {
        tool_call_id: Box<str>,
        text: Rendered,
        approved: Option<bool>,
    },
    /// A file the agent wrote or edited (`file_changed`): the tool call
    /// re-renders as a diff. `text` holds `+`/`-` prefixed lines.
    Diff {
        path: Box<str>,
        text: Box<str>,
        added: u32,
        removed: u32,
        expanded: bool,
    },
    Marker {
        kind: Marker,
        text: Box<str>,
    },
    /// A cell that was withdrawn (an optimistic prompt the harness queued
    /// instead). Renders no rows; indices of later cells stay valid.
    Hidden,
}

/// Which text of a cell a row comes from, and how it is styled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Src {
    /// The main text (user, assistant, marker, plan).
    Text,
    /// A tool's name + args line, or an exec command line.
    Head,
    /// A tool result or exec output line (rendered behind a `⎿` gutter).
    Body,
    /// A question's text; the index selects the question.
    Question(u16),
    /// A question's option; `(question, option)`.
    Option(u16, u16),
    /// A one-line receipt: tool state, exit status, answered.
    Tail,
}

#[derive(Clone, Copy, Debug)]
pub struct LayoutRow {
    pub src: Src,
    pub row: Row,
}

/// The cached layout of one cell at one width.
#[derive(Debug, Default)]
pub struct Layout {
    pub width: u16,
    pub rows: Vec<LayoutRow>,
}

/// Lines of a collapsed tool/exec body before the `… N more lines` row.
pub const COLLAPSE_LINES: usize = 5;

#[derive(Debug)]
pub struct Entry {
    pub cell: Cell,
    layout: Layout,
}

impl Entry {
    pub fn new(cell: Cell) -> Self {
        Entry {
            cell,
            layout: Layout::default(),
        }
    }

    /// Drop the cached layout so the next render re-wraps.
    pub fn invalidate(&mut self) {
        self.layout.rows.clear();
        self.layout.width = 0;
    }

    /// Hide the cell (see [`Cell::Hidden`]).
    pub fn hide(&mut self) {
        self.cell = Cell::Hidden;
        self.invalidate();
    }

    /// The layout at `width` (indent already removed), computing it on a
    /// miss. Zero allocations on a hit.
    pub fn layout(&mut self, width: u16) -> &Layout {
        if self.layout.width != width {
            self.layout.width = width;
            self.layout.rows.clear();
            layout_cell(&self.cell, width, &mut self.layout.rows);
        }
        &self.layout
    }

    /// The text of a single-source cell changed from byte `offset` on.
    /// Rows never cross a newline, so every row that ends at or before the
    /// last newline before `offset` is unchanged; the rest is re-wrapped.
    /// A stale or empty cache is left for the next full layout.
    pub fn relayout_from(&mut self, offset: usize) {
        if self.layout.rows.is_empty() {
            return;
        }
        let width = self.layout.width;
        let text = source_of(&self.cell, Src::Text);
        let offset = offset.min(text.len());
        let nl = text[..offset].rfind('\n');
        let keep = match nl {
            Some(p) => self
                .layout
                .rows
                .iter()
                .take_while(|r| r.src == Src::Text && r.row.end as usize <= p)
                .count(),
            None => 0,
        };
        let start = nl.map(|p| p + 1).unwrap_or(0);
        self.layout.rows.truncate(keep);
        if start >= text.len() && keep > 0 {
            return;
        }
        let mut scratch = Vec::new();
        wrap_into(&text[start..], width, &mut scratch);
        self.layout
            .rows
            .extend(scratch.into_iter().map(|row| LayoutRow {
                src: Src::Text,
                row: Row {
                    start: row.start + start as u32,
                    end: row.end + start as u32,
                },
            }));
    }

    /// A cached layout row, if the layout is current.
    pub fn layout_row(&self, row: usize) -> Option<&LayoutRow> {
        self.layout.rows.get(row)
    }

    /// Row count at `width`.
    pub fn height(&mut self, width: u16) -> u16 {
        self.layout(width).rows.len().min(u16::MAX as usize) as u16
    }

    /// A cell is final when nothing can change its text: in native
    /// scrollback mode it is then written to the terminal and released.
    pub fn is_final(&self) -> bool {
        match &self.cell {
            Cell::User { pending, .. } => !pending,
            Cell::Assistant {
                streaming, open, ..
            } => !streaming && !open,
            Cell::Tool { state, .. } => *state != ToolState::Running,
            Cell::Exec { done, .. } => *done,
            Cell::Question { .. }
            | Cell::Plan { .. }
            | Cell::Diff { .. }
            | Cell::Marker { .. }
            | Cell::Hidden => true,
        }
    }

    /// The text a layout row points into.
    pub fn source(&self, src: Src) -> &str {
        source_of(&self.cell, src)
    }

    /// Does any text of the cell contain `needle` (lowercase)?
    pub fn matches(&self, needle: &str) -> bool {
        let srcs: &[Src] = match &self.cell {
            Cell::User { .. }
            | Cell::Assistant { .. }
            | Cell::Marker { .. }
            | Cell::Plan { .. } => &[Src::Text],
            Cell::Tool { .. } | Cell::Exec { .. } | Cell::Diff { .. } => &[Src::Head, Src::Body],
            Cell::Question { questions, .. } => {
                return questions.iter().any(|q| {
                    crate::render::find_ci(&q.question, needle).is_some()
                        || q.options
                            .iter()
                            .any(|o| crate::render::find_ci(&o.label, needle).is_some())
                });
            }
            Cell::Hidden => &[],
        };
        srcs.iter().any(|&s| {
            let text = self.source(s);
            text.lines()
                .any(|l| crate::render::find_ci(l, needle).is_some())
        })
    }

    /// Style runs over the `Text` source (markdown cells); empty for the
    /// rest.
    pub fn runs(&self) -> &[Run] {
        match &self.cell {
            Cell::Assistant { md, .. } => md.runs(),
            Cell::Plan { text, .. } => &text.runs,
            _ => &[],
        }
    }
}

fn source_of(cell: &Cell, src: Src) -> &str {
    match (cell, src) {
        (Cell::User { text, .. }, Src::Text) => text,
        (Cell::Assistant { md, .. }, Src::Text) => md.text(),
        (Cell::Marker { text, .. }, Src::Text) => text,
        (Cell::Plan { text, .. }, Src::Text) => &text.text,
        (Cell::Diff { path, .. }, Src::Head) => path,
        (Cell::Diff { text, .. }, Src::Body) => text,
        (Cell::Tool { args, .. }, Src::Head) => args,
        (Cell::Tool { result, .. }, Src::Body) => result.as_deref().unwrap_or(""),
        (Cell::Exec { command, .. }, Src::Head) => command,
        (Cell::Exec { output, .. }, Src::Body) => output,
        (Cell::Question { questions, .. }, Src::Question(i)) => questions
            .get(i as usize)
            .map(|q| q.question.as_str())
            .unwrap_or(""),
        (Cell::Question { questions, .. }, Src::Option(i, j)) => questions
            .get(i as usize)
            .and_then(|q| q.options.get(j as usize))
            .map(|o| o.label.as_str())
            .unwrap_or(""),
        _ => "",
    }
}

fn push_rows(src: Src, text: &str, width: u16, out: &mut Vec<LayoutRow>, scratch: &mut Vec<Row>) {
    scratch.clear();
    wrap_into(text, width, scratch);
    out.extend(scratch.iter().map(|&row| LayoutRow { src, row }));
}

/// A body (tool result, exec output) collapses to its first lines unless
/// expanded. The `… N more` row is a `Tail`.
fn push_body(
    text: &str,
    expanded: bool,
    width: u16,
    out: &mut Vec<LayoutRow>,
    scratch: &mut Vec<Row>,
) {
    if text.is_empty() {
        return;
    }
    scratch.clear();
    wrap_into(text, width.saturating_sub(2), scratch);
    let total = scratch.len();
    let shown = if expanded {
        total
    } else {
        total.min(COLLAPSE_LINES)
    };
    out.extend(scratch[..shown].iter().map(|&row| LayoutRow {
        src: Src::Body,
        row,
    }));
    if shown < total {
        // The tail row carries `shown..total` so the renderer can say how
        // many rows are folded.
        out.push(LayoutRow {
            src: Src::Tail,
            row: Row {
                start: shown as u32,
                end: total as u32,
            },
        });
    }
}

fn layout_cell(cell: &Cell, width: u16, out: &mut Vec<LayoutRow>) {
    let mut scratch = Vec::new();
    match cell {
        Cell::User { text, .. } => push_rows(Src::Text, text, width, out, &mut scratch),
        Cell::Assistant { md, .. } => push_rows(Src::Text, md.text(), width, out, &mut scratch),
        Cell::Marker { text, .. } => push_rows(Src::Text, text, width, out, &mut scratch),
        Cell::Hidden => {}
        Cell::Diff {
            path,
            text,
            expanded,
            ..
        } => {
            push_rows(Src::Head, path, width, out, &mut scratch);
            push_body(text, *expanded, width, out, &mut scratch);
        }
        Cell::Plan { text, .. } => {
            push_rows(Src::Text, &text.text, width, out, &mut scratch);
            out.push(LayoutRow {
                src: Src::Tail,
                row: Row { start: 0, end: 0 },
            });
        }
        Cell::Tool {
            args,
            result,
            expanded,
            ..
        } => {
            push_rows(Src::Head, args, width, out, &mut scratch);
            push_body(
                result.as_deref().unwrap_or(""),
                *expanded,
                width,
                out,
                &mut scratch,
            );
        }
        Cell::Exec {
            command,
            output,
            expanded,
            ..
        } => {
            push_rows(Src::Head, command, width, out, &mut scratch);
            push_body(output, *expanded, width, out, &mut scratch);
            out.push(LayoutRow {
                src: Src::Tail,
                row: Row { start: 0, end: 0 },
            });
        }
        Cell::Question { questions, .. } => {
            for (i, q) in questions.iter().enumerate().take(u16::MAX as usize) {
                push_rows(
                    Src::Question(i as u16),
                    &q.question,
                    width,
                    out,
                    &mut scratch,
                );
                for (j, o) in q.options.iter().enumerate().take(u16::MAX as usize) {
                    push_rows(
                        Src::Option(i as u16, j as u16),
                        &o.label,
                        width,
                        out,
                        &mut scratch,
                    );
                }
            }
            out.push(LayoutRow {
                src: Src::Tail,
                row: Row { start: 0, end: 0 },
            });
        }
    }
}

/// The cells of one session, oldest first. Entries released to the
/// terminal's scrollback (inline mode) are popped from the front; `base`
/// keeps indices stable for the reducer's bookkeeping.
#[derive(Debug, Default)]
pub struct Transcript {
    entries: VecDeque<Entry>,
    base: usize,
    /// Count of entries written to native scrollback so far (never
    /// decreases; equals `base` once they are released).
    committed: usize,
}

impl Transcript {
    pub fn len(&self) -> usize {
        self.base + self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Index of the first entry still in memory.
    pub fn first_index(&self) -> usize {
        self.base
    }

    pub fn push(&mut self, cell: Cell) -> usize {
        self.entries.push_back(Entry::new(cell));
        self.len() - 1
    }

    /// Prepend another transcript's cells (a history backfill). Only
    /// valid while nothing has been released; indices of existing cells
    /// move up by the prepended count.
    pub fn prepend(&mut self, older: Transcript) -> usize {
        debug_assert_eq!(self.base, 0);
        let n = older.entries.len();
        for e in older.entries.into_iter().rev() {
            self.entries.push_front(e);
        }
        n
    }

    pub fn get(&self, index: usize) -> Option<&Entry> {
        index
            .checked_sub(self.base)
            .and_then(|i| self.entries.get(i))
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut Entry> {
        index
            .checked_sub(self.base)
            .and_then(|i| self.entries.get_mut(i))
    }

    pub fn last_mut(&mut self) -> Option<&mut Entry> {
        self.entries.back_mut()
    }

    pub fn last_index(&self) -> Option<usize> {
        self.len()
            .checked_sub(1)
            .filter(|_| !self.entries.is_empty())
    }

    pub fn iter_mut(&mut self) -> impl DoubleEndedIterator<Item = &mut Entry> {
        self.entries.iter_mut()
    }

    pub fn invalidate_all(&mut self) {
        for e in &mut self.entries {
            e.invalidate();
        }
    }

    /// The next entry that native scrollback mode can write out: the
    /// oldest uncommitted entry, if it is final and not the newest open
    /// one. The caller writes it and then calls [`Transcript::release`].
    pub fn next_committable(&mut self) -> Option<(usize, &mut Entry)> {
        let local = self.committed.checked_sub(self.base)?;
        let e = self.entries.get_mut(local)?;
        if e.is_final() {
            Some((self.committed, e))
        } else {
            None
        }
    }

    /// Release the entry `next_committable` returned: it now lives in the
    /// terminal. Indices of later entries do not change.
    pub fn release(&mut self) {
        if self.committed.checked_sub(self.base) == Some(0) {
            self.entries.pop_front();
            self.base += 1;
        }
        self.committed += 1;
    }

    /// Rows from the first row of `index` to the end of the transcript at
    /// `width` (lays out what it counts).
    pub fn rows_from(&mut self, index: usize, width: u16) -> u32 {
        let from = index.saturating_sub(self.base);
        self.entries
            .iter_mut()
            .skip(from)
            .map(|e| e.height(width) as u32)
            .sum()
    }

    /// Total rows at `width`, laying out every entry still in memory. Used
    /// by tests and by the jump-to-top path; the renderer walks from the
    /// bottom instead.
    pub fn total_height(&mut self, width: u16) -> u32 {
        self.entries
            .iter_mut()
            .map(|e| e.height(width) as u32)
            .sum()
    }
}
