//! Word wrap that produces byte ranges, not strings (ADR 0124 D3).
//!
//! A row is `[start, end)` into the source text. Rows never split a
//! grapheme, never exceed `width` display cells, and break at the last
//! whitespace that fits when there is one. A `\n` always ends a row. The
//! whitespace a row breaks on is dropped from both rows.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// One wrapped row: a byte range into the text it was wrapped from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub start: u32,
    pub end: u32,
}

/// Display width of one grapheme. Control characters count as one cell:
/// the renderer paints them as a space so the buffer and the terminal agree.
pub fn grapheme_width(g: &str) -> u16 {
    let w = g.width();
    if w == 0 && g.chars().any(char::is_control) {
        1
    } else {
        w as u16
    }
}

/// Wrap `text` at `width` cells, appending rows to `out`. An empty text
/// yields one empty row so every cell occupies at least one line.
pub fn wrap_into(text: &str, width: u16, out: &mut Vec<Row>) {
    let width = width.max(1) as usize;
    let mut row_start = 0usize;
    let mut row_width = 0usize;
    // The byte offset and width-at of the last whitespace seen in this row.
    let mut last_space: Option<(usize, usize)> = None;
    let mut pushed = false;

    for (off, g) in text.grapheme_indices(true) {
        if g == "\n" || g == "\r\n" {
            out.push(Row {
                start: row_start as u32,
                end: off as u32,
            });
            pushed = true;
            row_start = off + g.len();
            row_width = 0;
            last_space = None;
            continue;
        }
        let gw = grapheme_width(g) as usize;
        // A space that would overflow is itself the break: it is dropped.
        if g == " " && row_width + gw > width {
            out.push(Row {
                start: row_start as u32,
                end: off as u32,
            });
            pushed = true;
            row_start = off + 1;
            row_width = 0;
            last_space = None;
            continue;
        }
        if row_width + gw > width && row_width > 0 {
            match last_space {
                Some((sp, _)) => {
                    out.push(Row {
                        start: row_start as u32,
                        end: sp as u32,
                    });
                    // Skip the break whitespace itself; re-measure the
                    // carried tail from the grapheme after it.
                    let carry_start = sp + 1;
                    row_start = carry_start;
                    row_width = text[carry_start..off]
                        .graphemes(true)
                        .map(|g| grapheme_width(g) as usize)
                        .sum();
                }
                None => {
                    out.push(Row {
                        start: row_start as u32,
                        end: off as u32,
                    });
                    row_start = off;
                    row_width = 0;
                }
            }
            pushed = true;
            last_space = None;
            // The carried tail can itself exceed the width when a long
            // word follows a space; fall through to hard-break it.
            while row_width + gw > width && row_width > 0 {
                let mut acc = 0usize;
                let mut cut = row_start;
                for (o, h) in text[row_start..off].grapheme_indices(true) {
                    let hw = grapheme_width(h) as usize;
                    if acc + hw > width {
                        break;
                    }
                    acc += hw;
                    cut = row_start + o + h.len();
                }
                if cut == row_start {
                    break;
                }
                out.push(Row {
                    start: row_start as u32,
                    end: cut as u32,
                });
                row_width -= acc;
                row_start = cut;
            }
        }
        if g == " " {
            last_space = Some((off, row_width));
        }
        row_width += gw;
    }
    if row_start < text.len() || !pushed {
        out.push(Row {
            start: row_start as u32,
            end: text.len() as u32,
        });
    }
}

/// Display width of a string with the same control-character rule as
/// [`grapheme_width`].
pub fn display_width(s: &str) -> usize {
    s.graphemes(true).map(|g| grapheme_width(g) as usize).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &str, width: u16) -> Vec<&str> {
        let mut out = Vec::new();
        wrap_into(text, width, &mut out);
        out.iter()
            .map(|r| &text[r.start as usize..r.end as usize])
            .collect()
    }

    #[test]
    fn breaks_at_spaces_and_drops_the_break_space() {
        assert_eq!(rows("hello wide world", 6), vec!["hello", "wide", "world"]);
        assert_eq!(rows("ab cd", 5), vec!["ab cd"]);
        assert_eq!(rows("one two three", 7), vec!["one two", "three"]);
        assert_eq!(rows("ab cd ef", 5), vec!["ab cd", "ef"]);
    }

    #[test]
    fn hard_breaks_long_words() {
        assert_eq!(rows("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(rows("a bcdefgh", 3), vec!["a", "bcd", "efg", "h"]);
    }

    #[test]
    fn newlines_force_rows_and_empty_text_is_one_row() {
        assert_eq!(rows("a\n\nb\n", 10), vec!["a", "", "b"]);
        assert_eq!(rows("", 10), vec![""]);
        assert_eq!(rows("\n", 10), vec![""]);
    }

    #[test]
    fn wide_graphemes_count_two_cells() {
        assert_eq!(rows("日本語", 4), vec!["日本", "語"]);
    }
}
