//! The shell pane (ADR 0124 PR7): a `vt100` screen fed by the session
//! shell, and the key encoder that turns crossterm events into the bytes
//! a terminal would send. The socket itself lives in `net.rs`; the pane
//! only parses output and paints cells.

use std::num::NonZeroU16;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};

use crate::relay::Relay;

/// Each pane and each reopened shell gets its own generation, so the
/// program status can tell a fresh relay from the one it replaced.
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_generation() -> u64 {
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// No scrollback: the pane is a window on the live screen, and a
/// multiplexer inside the shell owns history if the user wants it.
const SCROLLBACK: usize = 0;

pub struct ShellPane {
    parser: vt100::Parser<Relay>,
    pub title: String,
    pub cols: u16,
    pub rows: u16,
    /// Keys go to the shell; otherwise to the composer.
    pub focused: bool,
    /// The socket is open.
    pub connected: bool,
    /// The server ended the shell (or the dial failed): the reason, shown
    /// until the pane is closed or reopened.
    pub ended: Option<String>,
    generation: u64,
    /// The last chunk ended on ESC: a `c` first in the next one is a full
    /// reset.
    esc_pending: bool,
}

impl std::fmt::Debug for ShellPane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellPane")
            .field("title", &self.title)
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .field("focused", &self.focused)
            .field("connected", &self.connected)
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

impl ShellPane {
    /// `program_status`: the outer terminal takes OSC 7501 reports, so
    /// the pane answers a program's detection query.
    pub fn new(cols: u16, rows: u16, program_status: bool) -> Self {
        let (c, r) = nonzero(cols, rows);
        ShellPane {
            parser: vt100::Parser::new_with_callbacks(r, c, SCROLLBACK, Relay::new(program_status)),
            title: String::new(),
            cols: c.get(),
            rows: r.get(),
            focused: true,
            connected: false,
            ended: None,
            generation: next_generation(),
            esc_pending: false,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Resize the screen; true when the size changed (the socket needs a
    /// resize frame).
    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        let (c, r) = nonzero(cols, rows);
        if c.get() == self.cols && r.get() == self.rows {
            return false;
        }
        self.cols = c.get();
        self.rows = r.get();
        self.parser.set_size(r, c);
        true
    }

    /// Parse output. Returns the bytes the program asked for (query
    /// replies), to send back on the socket.
    pub fn feed(&mut self, bytes: &[u8]) -> Option<Vec<u8>> {
        // A full reset (RIS, `ESC c`) removes every record, and vt100
        // handles it without a callback: split the chunk at each one. An
        // ESC ends any string or sequence vte is inside, so the pair is a
        // reset wherever it appears.
        let mut start = 0;
        for i in 0..bytes.len() {
            let after_esc = if i == 0 {
                self.esc_pending
            } else {
                bytes[i - 1] == 0x1b
            };
            if bytes[i] == b'c' && after_esc {
                self.parser.process(&bytes[start..=i]);
                self.parser.callbacks_mut().reset();
                start = i + 1;
            }
        }
        self.parser.process(&bytes[start..]);
        if let Some(&last) = bytes.last() {
            self.esc_pending = last == 0x1b;
        }
        self.parser.callbacks_mut().take_replies()
    }

    /// The program status records programs in the shell reported.
    pub fn relay(&self) -> &Relay {
        self.parser.callbacks()
    }

    /// The shell ended: what its programs were working on or blocked on
    /// is no longer true.
    pub fn end(&mut self, reason: String) {
        self.connected = false;
        self.ended = Some(reason);
        self.parser.callbacks_mut().drop_transient();
    }

    /// Fresh screen for a reopened shell.
    pub fn reset(&mut self) {
        let (c, r) = nonzero(self.cols, self.rows);
        let enabled = self.parser.callbacks().enabled;
        self.parser = vt100::Parser::new_with_callbacks(r, c, SCROLLBACK, Relay::new(enabled));
        self.ended = None;
        self.connected = false;
        self.generation = next_generation();
        self.esc_pending = false;
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Where the terminal cursor belongs when the pane has focus.
    pub fn cursor(&self, area: Rect) -> Option<Position> {
        let s = self.parser.screen();
        if s.hide_cursor() {
            return None;
        }
        let (row, col) = s.cursor_position();
        if row >= area.height || col >= area.width {
            return None;
        }
        Some(Position::new(area.x + col, area.y + row))
    }

    /// Paint the screen into `area` (clipped when the pane shrank below
    /// the screen size between a resize and its frame).
    pub fn paint(&self, buf: &mut Buffer, area: Rect) {
        let s = self.parser.screen();
        for row in 0..area.height.min(self.rows) {
            let mut col = 0u16;
            while col < area.width.min(self.cols) {
                let Some(cell) = s.cell(row, col) else {
                    break;
                };
                if cell.is_wide_continuation() {
                    col += 1;
                    continue;
                }
                let target = &mut buf[(area.x + col, area.y + row)];
                let text = cell.contents();
                target.set_symbol(if text.is_empty() { " " } else { text });
                target.set_style(style_of(cell));
                col += if cell.is_wide() { 2 } else { 1 };
            }
        }
    }

    /// Bytes for a paste: bracketed when the program asked for it.
    pub fn encode_paste(&self, text: &str, out: &mut Vec<u8>) {
        if self.parser.screen().bracketed_paste() {
            out.extend_from_slice(b"\x1b[200~");
            out.extend_from_slice(text.as_bytes());
            out.extend_from_slice(b"\x1b[201~");
        } else {
            out.extend_from_slice(text.as_bytes());
        }
    }

    /// Bytes for a key, honouring the program's cursor-key mode.
    pub fn encode_key(&self, key: &KeyEvent, out: &mut Vec<u8>) -> bool {
        encode_key(key, self.parser.screen().application_cursor(), out)
    }
}

fn nonzero(cols: u16, rows: u16) -> (NonZeroU16, NonZeroU16) {
    let one = NonZeroU16::MIN;
    (
        NonZeroU16::new(cols).unwrap_or(one),
        NonZeroU16::new(rows).unwrap_or(one),
    )
}

fn color_of(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn style_of(cell: &vt100::Cell) -> Style {
    let mut m = Modifier::empty();
    if cell.bold() {
        m |= Modifier::BOLD;
    }
    if cell.dim() {
        m |= Modifier::DIM;
    }
    if cell.italic() {
        m |= Modifier::ITALIC;
    }
    if cell.underline() {
        m |= Modifier::UNDERLINED;
    }
    if cell.inverse() {
        m |= Modifier::REVERSED;
    }
    Style::new()
        .fg(color_of(cell.fgcolor()))
        .bg(color_of(cell.bgcolor()))
        .add_modifier(m)
}

/// Encode a key the way an xterm-compatible terminal does. Returns false
/// for keys that have no byte sequence (a bare modifier, a release).
pub fn encode_key(key: &KeyEvent, app_cursor: bool, out: &mut Vec<u8>) -> bool {
    if key.kind == KeyEventKind::Release {
        return false;
    }
    let m = key.modifiers;
    let ctrl = m.contains(KeyModifiers::CONTROL);
    let alt = m.contains(KeyModifiers::ALT);
    let shift = m.contains(KeyModifiers::SHIFT);
    // xterm's modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
    let param = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let csi = |out: &mut Vec<u8>, letter: u8| {
        if param > 1 {
            out.extend_from_slice(b"\x1b[1;");
            out.push(b'0' + param);
            out.push(letter);
        } else if app_cursor {
            out.extend_from_slice(b"\x1bO");
            out.push(letter);
        } else {
            out.extend_from_slice(b"\x1b[");
            out.push(letter);
        }
    };
    let tilde = |out: &mut Vec<u8>, code: &[u8]| {
        out.extend_from_slice(b"\x1b[");
        out.extend_from_slice(code);
        if param > 1 {
            out.push(b';');
            out.push(b'0' + param);
        }
        out.push(b'~');
    };
    match key.code {
        KeyCode::Char(c) => {
            if alt {
                out.push(0x1b);
            }
            if ctrl {
                let b = match c.to_ascii_lowercase() {
                    c @ 'a'..='z' => (c as u8) & 0x1f,
                    ' ' | '@' | '`' => 0,
                    '[' => 0x1b,
                    '\\' => 0x1c,
                    ']' => 0x1d,
                    '^' => 0x1e,
                    '_' => 0x1f,
                    '?' => 0x7f,
                    _ => return false,
                };
                out.push(b);
            } else {
                let mut b = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            }
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Backspace => {
            if alt {
                out.push(0x1b);
            }
            out.push(if ctrl { 0x08 } else { 0x7f });
        }
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Null => out.push(0),
        KeyCode::Up => csi(out, b'A'),
        KeyCode::Down => csi(out, b'B'),
        KeyCode::Right => csi(out, b'C'),
        KeyCode::Left => csi(out, b'D'),
        KeyCode::Home => csi(out, b'H'),
        KeyCode::End => csi(out, b'F'),
        KeyCode::Insert => tilde(out, b"2"),
        KeyCode::Delete => tilde(out, b"3"),
        KeyCode::PageUp => tilde(out, b"5"),
        KeyCode::PageDown => tilde(out, b"6"),
        KeyCode::F(n @ 1..=4) => {
            if param > 1 {
                out.extend_from_slice(b"\x1b[1;");
                out.push(b'0' + param);
            } else {
                out.extend_from_slice(b"\x1bO");
            }
            out.push(b'P' + (n - 1));
        }
        KeyCode::F(n @ 5..=12) => {
            let code: &[u8] = match n {
                5 => b"15",
                6 => b"17",
                7 => b"18",
                8 => b"19",
                9 => b"20",
                10 => b"21",
                11 => b"23",
                _ => b"24",
            };
            tilde(out, code);
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(code: KeyCode, m: KeyModifiers, app: bool) -> Vec<u8> {
        let mut out = Vec::new();
        encode_key(&KeyEvent::new(code, m), app, &mut out);
        out
    }

    #[test]
    fn keys_encode_as_xterm_sequences() {
        let n = KeyModifiers::NONE;
        assert_eq!(enc(KeyCode::Char('a'), n, false), b"a");
        assert_eq!(enc(KeyCode::Char('é'), n, false), "é".as_bytes());
        assert_eq!(enc(KeyCode::Char('c'), KeyModifiers::CONTROL, false), [3]);
        assert_eq!(enc(KeyCode::Char('C'), KeyModifiers::CONTROL, false), [3]);
        assert_eq!(enc(KeyCode::Char(' '), KeyModifiers::CONTROL, false), [0]);
        assert_eq!(enc(KeyCode::Char('x'), KeyModifiers::ALT, false), b"\x1bx");
        assert_eq!(enc(KeyCode::Enter, n, false), b"\r");
        assert_eq!(enc(KeyCode::Backspace, n, false), [0x7f]);
        assert_eq!(enc(KeyCode::Esc, n, false), [0x1b]);
        assert_eq!(enc(KeyCode::Up, n, false), b"\x1b[A");
        assert_eq!(enc(KeyCode::Up, n, true), b"\x1bOA");
        assert_eq!(enc(KeyCode::Up, KeyModifiers::SHIFT, true), b"\x1b[1;2A");
        assert_eq!(enc(KeyCode::Delete, n, false), b"\x1b[3~");
        assert_eq!(
            enc(KeyCode::PageUp, KeyModifiers::CONTROL, false),
            b"\x1b[5;5~"
        );
        assert_eq!(enc(KeyCode::F(1), n, false), b"\x1bOP");
        assert_eq!(enc(KeyCode::F(5), n, false), b"\x1b[15~");
        assert_eq!(enc(KeyCode::F(12), n, false), b"\x1b[24~");
        assert_eq!(enc(KeyCode::BackTab, n, false), b"\x1b[Z");
        let mut rel = KeyEvent::new(KeyCode::Char('a'), n);
        rel.kind = KeyEventKind::Release;
        let mut out = Vec::new();
        assert!(!encode_key(&rel, false, &mut out));
        assert!(out.is_empty());
    }

    #[test]
    fn the_pane_parses_output_and_follows_the_programs_modes() {
        let mut p = ShellPane::new(10, 3, false);
        p.feed(b"hi\r\n\x1b[1;31mred\x1b[0m");
        let s = p.screen();
        assert_eq!(s.cell(0, 0).unwrap().contents(), "h");
        assert_eq!(s.cell(1, 0).unwrap().contents(), "r");
        assert!(s.cell(1, 0).unwrap().bold());
        assert_eq!(s.cell(1, 0).unwrap().fgcolor(), vt100::Color::Idx(1));
        assert_eq!(s.cursor_position(), (1, 3));

        let mut out = Vec::new();
        p.encode_paste("ab", &mut out);
        assert_eq!(out, b"ab");
        p.feed(b"\x1b[?2004h\x1b[?1h");
        out.clear();
        p.encode_paste("ab", &mut out);
        assert_eq!(out, b"\x1b[200~ab\x1b[201~");
        out.clear();
        p.encode_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &mut out);
        assert_eq!(out, b"\x1bOB");

        assert!(p.resize(20, 5));
        assert!(!p.resize(20, 5));
        assert_eq!((p.cols, p.rows), (20, 5));
        // Zero is never a screen size.
        let z = ShellPane::new(0, 0, false);
        assert_eq!((z.cols, z.rows), (1, 1));
    }

    #[test]
    fn paint_copies_cells_and_skips_wide_continuations() {
        let mut p = ShellPane::new(6, 2, false);
        p.feed("a日b".as_bytes());
        let area = Rect::new(0, 0, 6, 2);
        let mut buf = Buffer::empty(area);
        p.paint(&mut buf, area);
        assert_eq!(buf[(0, 0)].symbol(), "a");
        assert_eq!(buf[(1, 0)].symbol(), "日");
        assert_eq!(buf[(2, 0)].symbol(), " ");
        assert_eq!(buf[(3, 0)].symbol(), "b");
        assert_eq!(p.cursor(area), Some(Position::new(4, 0)));
        p.feed(b"\x1b[?25l");
        assert_eq!(p.cursor(area), None);
    }
}
