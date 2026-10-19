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
        keys: "Alt+↑ / Alt+↓",
        action: Action::PrevSession,
        help: "previous / next session",
    },
    Binding {
        keys: "Ctrl+O / Ctrl+E",
        action: Action::ExpandLast,
        help: "expand the last tool call / all",
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
        keys: "Ctrl+A/E/K/U/W/Y, Alt+B/F",
        action: Action::Home,
        help: "emacs editing",
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
        keys: "Ctrl+L",
        action: Action::Redraw,
        help: "redraw",
    },
    Binding {
        keys: "?",
        action: Action::Help,
        help: "this list (on an empty draft)",
    },
];

/// Resolve a key press. `None` for releases and chords with no binding.
pub fn resolve(key: &KeyEvent, draft_empty: bool) -> Option<Action> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let m = key.modifiers;
    let ctrl = m.contains(KeyModifiers::CONTROL);
    let alt = m.contains(KeyModifiers::ALT);
    let shift = m.contains(KeyModifiers::SHIFT);
    Some(match key.code {
        KeyCode::Enter if shift || alt => Action::Newline,
        KeyCode::Enter => Action::Send,
        KeyCode::Esc => Action::Escape,
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Delete => Action::Delete,
        KeyCode::Left if alt => Action::WordLeft,
        KeyCode::Right if alt => Action::WordRight,
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
        KeyCode::Char(c) if ctrl => match c.to_ascii_lowercase() {
            'c' => Action::CtrlC,
            'd' => Action::Detach,
            'p' => Action::Palette,
            'o' => Action::ExpandLast,
            'e' => Action::ExpandAll,
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
            _ => return None,
        },
        KeyCode::Char(c) if alt => match c {
            'b' => Action::WordLeft,
            'f' => Action::WordRight,
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

    #[test]
    fn enter_variants_and_control_chords() {
        assert_eq!(
            resolve(&key(KeyCode::Enter, KeyModifiers::NONE), true),
            Some(Action::Send)
        );
        assert_eq!(
            resolve(&key(KeyCode::Enter, KeyModifiers::SHIFT), true),
            Some(Action::Newline)
        );
        assert_eq!(
            resolve(&key(KeyCode::Char('j'), KeyModifiers::CONTROL), true),
            Some(Action::Newline)
        );
        assert_eq!(
            resolve(&key(KeyCode::Char('c'), KeyModifiers::CONTROL), true),
            Some(Action::CtrlC)
        );
        assert_eq!(
            resolve(&key(KeyCode::Char('?'), KeyModifiers::NONE), true),
            Some(Action::Help)
        );
        assert_eq!(
            resolve(&key(KeyCode::Char('?'), KeyModifiers::NONE), false),
            Some(Action::Char('?'))
        );
        let mut rel = key(KeyCode::Enter, KeyModifiers::NONE);
        rel.kind = KeyEventKind::Release;
        assert_eq!(resolve(&rel, true), None);
    }
}
