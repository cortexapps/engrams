//! The prompt editor: one `String`, a byte cursor, grapheme-aware motion,
//! emacs keys, soft wrap through the shared wrapper, bracketed-paste
//! collapsing, and prompt history.

use unicode_segmentation::UnicodeSegmentation;

use crate::wrap::{display_width, wrap_into, Row};

/// A paste longer than this many lines or bytes is collapsed to a
/// placeholder in the draft and expanded again on send.
const PASTE_LINES: usize = 3;
const PASTE_BYTES: usize = 800;
const HISTORY_MAX: usize = 100;

#[derive(Debug, Default)]
pub struct Composer {
    text: String,
    cursor: usize,
    kill: String,
    pastes: Vec<String>,
    history: Vec<String>,
    /// Index into `history` while browsing; the draft is parked in
    /// `parked` meanwhile.
    browsing: Option<usize>,
    parked: String,
    rows: Vec<Row>,
    rows_width: u16,
}

impl Composer {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn set_text(&mut self, text: &str) {
        self.text.clear();
        self.text.push_str(text);
        self.cursor = self.text.len();
        self.rows_width = 0;
    }

    /// The draft with paste placeholders expanded; clears the draft and
    /// records it in history. `None` for a blank draft.
    pub fn take(&mut self) -> Option<String> {
        if self.text.trim().is_empty() {
            return None;
        }
        let mut out = String::with_capacity(self.text.len());
        let mut rest = self.text.as_str();
        while let Some(pos) = rest.find("[Pasted text #") {
            out.push_str(&rest[..pos]);
            let after = &rest[pos + "[Pasted text #".len()..];
            let close = after.find(']');
            let n = close.and_then(|c| after[..c].split_whitespace().next()?.parse::<usize>().ok());
            match (close, n.and_then(|n| self.pastes.get(n.checked_sub(1)?))) {
                (Some(c), Some(p)) => {
                    out.push_str(p);
                    rest = &after[c + 1..];
                }
                _ => {
                    out.push_str("[Pasted text #");
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        self.pastes.clear();
        self.text.clear();
        self.push_history(out.clone());
        self.cursor = 0;
        self.rows_width = 0;
        self.browsing = None;
        Some(out)
    }

    /// Clear the draft, keeping it in history so `Up` can recall it.
    pub fn clear(&mut self) {
        if !self.text.is_empty() {
            let t = std::mem::take(&mut self.text);
            self.push_history(t);
        }
        self.cursor = 0;
        self.pastes.clear();
        self.rows_width = 0;
        self.browsing = None;
    }

    fn push_history(&mut self, t: String) {
        if self.history.last() != Some(&t) {
            self.history.push(t);
            if self.history.len() > HISTORY_MAX {
                self.history.remove(0);
            }
        }
    }

    pub fn insert_str(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.rows_width = 0;
        self.browsing = None;
    }

    pub fn insert_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.insert_str(c.encode_utf8(&mut buf));
    }

    /// A bracketed paste: long ones collapse to `[Pasted text #N +L lines]`.
    pub fn paste(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        let lines = s.lines().count();
        if lines > PASTE_LINES || s.len() > PASTE_BYTES {
            self.pastes.push(s);
            let n = self.pastes.len();
            let tag = format!("[Pasted text #{n} +{lines} lines]");
            self.insert_str(&tag);
        } else {
            self.insert_str(&s);
        }
    }

    fn prev_boundary(&self) -> usize {
        self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    fn next_boundary(&self) -> usize {
        self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map(|g| self.cursor + g.len())
            .unwrap_or(self.cursor)
    }

    fn word_start(&self) -> usize {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end();
        let base = trimmed.len();
        trimmed
            .rfind(|c: char| c.is_whitespace())
            .map(|i| i + 1)
            .unwrap_or(0)
            .min(base)
    }

    fn word_end(&self) -> usize {
        let after = &self.text[self.cursor..];
        let skip = after.len() - after.trim_start().len();
        let rest = &after[skip..];
        let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
        self.cursor + skip + end
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map(|i| self.cursor + i)
            .unwrap_or(self.text.len())
    }

    pub fn backspace(&mut self) {
        let p = self.prev_boundary();
        self.text.replace_range(p..self.cursor, "");
        self.cursor = p;
        self.rows_width = 0;
    }

    pub fn delete(&mut self) {
        let n = self.next_boundary();
        self.text.replace_range(self.cursor..n, "");
        self.rows_width = 0;
    }

    pub fn left(&mut self) {
        self.cursor = self.prev_boundary();
    }

    pub fn right(&mut self) {
        self.cursor = self.next_boundary();
    }

    pub fn word_left(&mut self) {
        self.cursor = self.word_start();
    }

    pub fn word_right(&mut self) {
        self.cursor = self.word_end();
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start();
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end();
    }

    /// `Ctrl+K`: kill to end of line (or the newline when already there).
    pub fn kill_to_end(&mut self) {
        let mut e = self.line_end();
        if e == self.cursor && e < self.text.len() {
            e += 1;
        }
        self.kill = self.text[self.cursor..e].to_string();
        self.text.replace_range(self.cursor..e, "");
        self.rows_width = 0;
    }

    /// `Ctrl+U`: kill to start of line.
    pub fn kill_to_start(&mut self) {
        let s = self.line_start();
        self.kill = self.text[s..self.cursor].to_string();
        self.text.replace_range(s..self.cursor, "");
        self.cursor = s;
        self.rows_width = 0;
    }

    /// `Ctrl+W`: kill the word before the cursor.
    pub fn kill_word(&mut self) {
        let s = self.word_start();
        self.kill = self.text[s..self.cursor].to_string();
        self.text.replace_range(s..self.cursor, "");
        self.cursor = s;
        self.rows_width = 0;
    }

    /// `Ctrl+Y`: yank the last kill.
    pub fn yank(&mut self) {
        let k = std::mem::take(&mut self.kill);
        self.insert_str(&k);
        self.kill = k;
    }

    /// Recall the previous history entry. `true` when the draft changed.
    pub fn history_prev(&mut self) -> bool {
        let next = match self.browsing {
            None if self.history.is_empty() => return false,
            None => {
                self.parked = std::mem::take(&mut self.text);
                self.history.len() - 1
            }
            Some(0) => return false,
            Some(i) => i - 1,
        };
        self.browsing = Some(next);
        self.text.clone_from(&self.history[next]);
        self.cursor = self.text.len();
        self.rows_width = 0;
        true
    }

    pub fn history_next(&mut self) -> bool {
        let Some(i) = self.browsing else {
            return false;
        };
        if i + 1 < self.history.len() {
            self.browsing = Some(i + 1);
            self.text.clone_from(&self.history[i + 1]);
        } else {
            self.browsing = None;
            self.text = std::mem::take(&mut self.parked);
        }
        self.cursor = self.text.len();
        self.rows_width = 0;
        true
    }

    /// The rows of the last `layout`.
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// Wrap at `width` (cached) and return the cursor's (row, col).
    pub fn layout(&mut self, width: u16) -> (u16, u16) {
        if self.rows_width != width || self.rows.is_empty() {
            self.rows.clear();
            wrap_into(&self.text, width, &mut self.rows);
            self.rows_width = width;
        }
        let mut pos = (0u16, 0u16);
        let n = self.rows.len();
        for (i, r) in self.rows.iter().enumerate() {
            let (s, e) = (r.start as usize, r.end as usize);
            if self.cursor < s || self.cursor > e {
                continue;
            }
            // On a hard break the cursor sits at both the end of this row
            // and the start of the next: it belongs to the next row.
            if self.cursor == e && i + 1 < n && self.rows[i + 1].start as usize == self.cursor {
                continue;
            }
            pos = (
                i as u16,
                display_width(&self.text[s..self.cursor]).min(width as usize) as u16,
            );
            break;
        }
        pos
    }

    /// Row the cursor is on, for `Up`/`Down` dispatch.
    pub fn cursor_row(&mut self, width: u16) -> (u16, u16) {
        let pos = self.layout(width);
        (pos.0, self.rows.len() as u16)
    }

    /// Move the cursor one row up/down, keeping the column. `false` when
    /// already on the edge row.
    pub fn move_row(&mut self, width: u16, down: bool) -> bool {
        let (row, col) = self.layout(width);
        let rows = &self.rows;
        let target = if down {
            if row as usize + 1 >= rows.len() {
                return false;
            }
            row + 1
        } else {
            if row == 0 {
                return false;
            }
            row - 1
        };
        let r = rows[target as usize];
        let line = &self.text[r.start as usize..r.end as usize];
        let mut acc = 0usize;
        let mut off = line.len();
        for (i, g) in line.grapheme_indices(true) {
            if acc >= col as usize {
                off = i;
                break;
            }
            acc += crate::wrap::grapheme_width(g) as usize;
        }
        self.cursor = r.start as usize + off;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_and_kills_are_grapheme_aware() {
        let mut c = Composer::default();
        c.insert_str("héllo wörld");
        c.backspace();
        assert_eq!(c.text(), "héllo wörl");
        c.kill_word();
        assert_eq!(c.text(), "héllo ");
        c.yank();
        assert_eq!(c.text(), "héllo wörl");
        c.home();
        c.kill_to_end();
        assert_eq!(c.text(), "");
    }

    #[test]
    fn take_expands_collapsed_pastes_and_records_history() {
        let mut c = Composer::default();
        c.insert_str("see: ");
        c.paste("a\nb\nc\nd\ne");
        assert_eq!(c.text(), "see: [Pasted text #1 +5 lines]");
        assert_eq!(c.take().as_deref(), Some("see: a\nb\nc\nd\ne"));
        assert!(c.is_empty());
        assert!(c.history_prev());
        assert_eq!(c.text(), "see: a\nb\nc\nd\ne");
        assert!(c.history_next());
        assert_eq!(c.text(), "");
    }

    #[test]
    fn cursor_position_follows_soft_wrap() {
        let mut c = Composer::default();
        c.insert_str("one two three");
        let pos = c.layout(7);
        assert_eq!(c.rows().len(), 2);
        assert_eq!(pos, (1, 5));
        c.home();
        assert_eq!(c.layout(7), (0, 0));
        assert!(c.move_row(7, true));
        assert_eq!(c.layout(7), (1, 0));
        assert!(!c.move_row(7, true));
        c.end();
        assert_eq!(c.layout(7), (1, 5));
    }
}
