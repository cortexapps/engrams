//! The static key table (ADR 0124 D7). One table drives both dispatch and
//! the `?` overlay so the help never drifts from the bindings.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Send,
    Newline,
    Escape,
    CtrlC,
    Detach,
    Palette,
    ExpandLast,
    ExpandAll,
    ScrollUp,
    ScrollDown,
    ScrollTop,
    ScrollBottom,
    PrevSession,
    NextSession,
    Help,
    Redraw,
    /// Transcript search (fullscreen).
    Search,
    /// Edit the draft in `$EDITOR`.
    EditDraft,
    /// Open or focus the shell pane.
    Shell,
    /// The new-session picker.
    NewSession,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    WordLeft,
    WordRight,
    KillToEnd,
    KillToStart,
    KillWord,
    /// Alt+D: kill the word after the cursor.
    KillWordForward,
    Yank,
    Char(char),
}

pub struct Binding {
    pub keys: &'static str,
    pub action: Action,
    pub help: &'static str,
}

/// Bindings in help order. Dispatch resolves modifiers first, so an entry
/// here is documentation plus the canonical name of its chord.
pub const BINDINGS: &[Binding] = &[
    Binding {
        keys: "Enter",
        action: Action::Send,
        help: "send (queues while the agent works)",
    },
    Binding {
        keys: "Shift+Enter / Ctrl+J",
        action: Action::Newline,
        help: "newline",
    },
    Binding {
        keys: "Esc",
        action: Action::Escape,
        help: "interrupt the run / close a popup; twice clears the draft",
    },
    Binding {
        keys: "Ctrl+C",
        action: Action::CtrlC,
        help: "clear the draft; twice quits",
    },
    Binding {
        keys: "Ctrl+D",
        action: Action::Detach,
        help: "back to the session list; twice quits",
    },
    Binding {
        keys: "Ctrl+P",
        action: Action::Palette,
        help: "session palette",
    },
    Binding {
        keys: "Ctrl+N",
        action: Action::NewSession,
        help: "new session (pick a profile, type the first prompt)",
    },
    Binding {
        keys: "Ctrl+N/P · j/k/g/G",
        action: Action::Down,
        help: "move in a list or popup; session list: / filters, n new",
    },
    Binding {
        keys: "Alt+↑ / Alt+↓",
        action: Action::PrevSession,
        help: "previous / next session",
    },
    Binding {
        keys: "Ctrl+O / Ctrl+E",
        action: Action::ExpandLast,
        help: "open the last tool call / every one (empty draft)",
    },
    Binding {
        keys: "PgUp / PgDn",
        action: Action::ScrollUp,
        help: "scroll the transcript",
    },
    Binding {
        keys: "Ctrl+Home / Ctrl+End",
        action: Action::ScrollTop,
        help: "jump to the top / bottom",
    },
    Binding {
        keys: "Ctrl+A/E/B/F/D/K/U/W/Y",
        action: Action::Home,
        help: "edit as in Codex; Alt+B/F/D, Alt+Backspace by word",
    },
    Binding {
        keys: "1-9 / Enter / Tab",
        action: Action::Char('1'),
        help: "answer a question / decide on a plan",
    },
    Binding {
        keys: "Ctrl+T",
        action: Action::Search,
        help: "search the transcript (Enter next · Esc close)",
    },
    Binding {
        keys: "Ctrl+G",
        action: Action::EditDraft,
        help: "edit the draft in $EDITOR",
    },
    Binding {
        keys: "Ctrl+` / Ctrl+Space",
        action: Action::Shell,
        help: "shell in the sandbox (again: back to the agent)",
    },
    Binding {
        keys: "/",
        action: Action::Char('/'),
        help: "commands (/new /shell /resume /transcript /dump …); Tab completes",
    },
    Binding {
        keys: "?",
        action: Action::Help,
        help: "this list (on an empty draft)",
    },
    Binding {
        keys: "Ctrl+L",
        action: Action::Redraw,
        help: "redraw",
    },
];

/// Resolve a key press. `None` for releases and chords with no binding.
///
/// The composer's editing keys are Codex's (`EditorKeymap` defaults):
/// Ctrl+A/E and Cmd+←/→ line start/end, Ctrl+B/F and Ctrl+P/N by
/// character and line, Alt+B/F and Alt/Ctrl+←/→ by word, Ctrl+H and
/// Ctrl+D delete a character, Alt/Ctrl+Backspace, Ctrl+W and Ctrl+Alt+H a
/// word back, Alt+D and Alt/Ctrl+Delete a word forward, Ctrl+U and
/// Cmd+Backspace kill to line start, Ctrl+K to line end, Ctrl+Y yanks.
/// Ctrl+D, Ctrl+E, Ctrl+N and Ctrl+P also have a meaning of their own
/// here (detach, open every tool call, new session, palette): that one
/// applies when `has_text` is false, where the editing key does nothing.
pub fn resolve(key: &KeyEvent, draft_empty: bool, has_text: bool) -> Option<Action> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let m = key.modifiers;
    let ctrl = m.contains(KeyModifiers::CONTROL);
    let alt = m.contains(KeyModifiers::ALT);
    let shift = m.contains(KeyModifiers::SHIFT);
    let cmd = m.contains(KeyModifiers::SUPER);
    Some(match key.code {
        KeyCode::Enter if shift || alt => Action::Newline,
        KeyCode::Enter => Action::Send,
        KeyCode::Esc => Action::Escape,
        KeyCode::Backspace if cmd => Action::KillToStart,
        KeyCode::Backspace if alt || ctrl => Action::KillWord,
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Delete if alt || ctrl => Action::KillWordForward,
        KeyCode::Delete => Action::Delete,
        KeyCode::Left if cmd => Action::Home,
        KeyCode::Right if cmd => Action::End,
        KeyCode::Left if alt || ctrl => Action::WordLeft,
        KeyCode::Right if alt || ctrl => Action::WordRight,
        KeyCode::Left => Action::Left,
        KeyCode::Right => Action::Right,
        KeyCode::Up if alt => Action::PrevSession,
        KeyCode::Down if alt => Action::NextSession,
        KeyCode::Up => Action::Up,
        KeyCode::Down => Action::Down,
        KeyCode::Home if ctrl => Action::ScrollTop,
        KeyCode::End if ctrl => Action::ScrollBottom,
        KeyCode::Home => Action::Home,
        KeyCode::End => Action::End,
        KeyCode::PageUp => Action::ScrollUp,
        KeyCode::PageDown => Action::ScrollDown,
        KeyCode::Tab => Action::Char('\t'),
        KeyCode::Char(c) if ctrl && alt => match c.to_ascii_lowercase() {
            'h' => Action::KillWord,
            _ => return None,
        },
        KeyCode::Char(c) if ctrl => match c.to_ascii_lowercase() {
            'c' => Action::CtrlC,
            'd' if has_text => Action::Delete,
            'd' => Action::Detach,
            'p' if has_text => Action::Up,
            'p' => Action::Palette,
            'n' if has_text => Action::Down,
            'n' => Action::NewSession,
            'e' if has_text => Action::End,
            'e' => Action::ExpandAll,
            'o' => Action::ExpandLast,
            'b' => Action::Left,
            'f' => Action::Right,
            'j' | 'm' => Action::Newline,
            'a' => Action::Home,
            'k' => Action::KillToEnd,
            'u' => Action::KillToStart,
            'w' => Action::KillWord,
            'y' => Action::Yank,
            'l' => Action::Redraw,
            'h' => Action::Backspace,
            't' => Action::Search,
            'g' => Action::EditDraft,
            // Ctrl+` arrives as NUL (Ctrl+Space / Ctrl+@) without the
            // kitty protocol, as '`' with it.
            '`' | ' ' | '@' => Action::Shell,
            _ => return None,
        },
        KeyCode::Char(c) if alt => match c {
            'b' => Action::WordLeft,
            'f' => Action::WordRight,
            'd' => Action::KillWordForward,
            _ => return None,
        },
        KeyCode::Char('?') if draft_empty => Action::Help,
        KeyCode::Char(c) => Action::Char(c),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    /// Codex's editing keys while the draft has text; the TUI's own
    /// meaning of the same chord on an empty draft.
    #[test]
    fn editing_keys_follow_codex_and_yield_on_an_empty_draft() {
        let k = |code, m| key(code, m);
        let ctrl = KeyModifiers::CONTROL;
        let alt = KeyModifiers::ALT;
        let with_text = |e: KeyEvent| resolve(&e, false, true);
        let empty = |e: KeyEvent| resolve(&e, true, false);
        assert_eq!(with_text(k(KeyCode::Char('e'), ctrl)), Some(Action::End));
        assert_eq!(empty(k(KeyCode::Char('e'), ctrl)), Some(Action::ExpandAll));
        assert_eq!(with_text(k(KeyCode::Char('d'), ctrl)), Some(Action::Delete));
        assert_eq!(empty(k(KeyCode::Char('d'), ctrl)), Some(Action::Detach));
        assert_eq!(with_text(k(KeyCode::Char('p'), ctrl)), Some(Action::Up));
        assert_eq!(empty(k(KeyCode::Char('p'), ctrl)), Some(Action::Palette));
        assert_eq!(with_text(k(KeyCode::Char('n'), ctrl)), Some(Action::Down));
        assert_eq!(empty(k(KeyCode::Char('n'), ctrl)), Some(Action::NewSession));
        assert_eq!(
            with_text(k(KeyCode::Char('d'), alt)),
            Some(Action::KillWordForward)
        );
        assert_eq!(
            with_text(k(KeyCode::Delete, alt)),
            Some(Action::KillWordForward)
        );
        assert_eq!(
            with_text(k(KeyCode::Backspace, alt)),
            Some(Action::KillWord)
        );
        assert_eq!(
            with_text(k(KeyCode::Char('h'), ctrl | alt)),
            Some(Action::KillWord)
        );
        assert_eq!(with_text(k(KeyCode::Char('b'), ctrl)), Some(Action::Left));
        assert_eq!(with_text(k(KeyCode::Char('f'), ctrl)), Some(Action::Right));
        assert_eq!(with_text(k(KeyCode::Left, ctrl)), Some(Action::WordLeft));
        assert_eq!(
            with_text(k(KeyCode::Right, KeyModifiers::SUPER)),
            Some(Action::End)
        );
        assert_eq!(
            with_text(k(KeyCode::Backspace, KeyModifiers::SUPER)),
            Some(Action::KillToStart)
        );
    }

    #[test]
    fn enter_variants_and_control_chords() {
        assert_eq!(
            resolve(&key(KeyCode::Enter, KeyModifiers::NONE), true, false),
            Some(Action::Send)
        );
        assert_eq!(
            resolve(&key(KeyCode::Enter, KeyModifiers::SHIFT), true, false),
            Some(Action::Newline)
        );
        assert_eq!(
            resolve(&key(KeyCode::Char('j'), KeyModifiers::CONTROL), true, false),
            Some(Action::Newline)
        );
        assert_eq!(
            resolve(&key(KeyCode::Char('c'), KeyModifiers::CONTROL), true, false),
            Some(Action::CtrlC)
        );
        assert_eq!(
            resolve(&key(KeyCode::Char('?'), KeyModifiers::NONE), true, false),
            Some(Action::Help)
        );
        assert_eq!(
            resolve(&key(KeyCode::Char('?'), KeyModifiers::NONE), false, false),
            Some(Action::Char('?'))
        );
        let mut rel = key(KeyCode::Enter, KeyModifiers::NONE);
        rel.kind = KeyEventKind::Release;
        assert_eq!(resolve(&rel, true, false), None);
    }
}
