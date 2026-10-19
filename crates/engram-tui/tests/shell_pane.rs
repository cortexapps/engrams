//! The shell pane (ADR 0124 PR7): opened and focused from the keyboard,
//! keys become shell bytes, Ctrl+` hands focus back, Esc closes, and the
//! pane's screen size follows the terminal.

mod common;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use engram_tui::app::{App, Cmd, Msg, NetMsg};
use engram_tui::render::{draw_fullscreen, draw_overlay, Theme};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn key(app: &mut App, code: KeyCode, m: KeyModifiers) {
    app.update(Msg::Key(KeyEvent::new(code, m)));
}

fn ctrl_backtick(app: &mut App) {
    key(app, KeyCode::Char('`'), KeyModifiers::CONTROL);
}

fn frame(app: &mut App, w: u16, h: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    let theme = Theme::new(false);
    let mut scratch = Vec::new();
    terminal
        .draw(|f| {
            let area = f.area();
            draw_fullscreen(f.buffer_mut(), area, app, &theme, &mut scratch);
        })
        .unwrap();
    terminal.backend().to_string()
}

fn opened(app: &mut App) {
    app.update(Msg::Net(NetMsg::ShellOpened {
        session_id: "s-1".into(),
    }));
}

fn output(app: &mut App, bytes: &'static [u8]) {
    app.update(Msg::Net(NetMsg::ShellOutput {
        session_id: "s-1".into(),
        bytes: bytes::Bytes::from_static(bytes),
    }));
}

#[test]
fn open_type_unfocus_and_close() {
    let mut app = common::attached_app(false, 60, 16);
    ctrl_backtick(&mut app);
    // Body is 15 rows: the pane takes 6 (header + 5 screen rows).
    assert_eq!(
        app.cmds,
        [Cmd::ShellOpen {
            session_id: "s-1".into(),
            cols: 60,
            rows: 5
        }]
    );
    app.cmds.clear();
    let sh = app.attached.as_ref().unwrap().shell.as_ref().unwrap();
    assert!(sh.focused && !sh.connected);

    // Nothing is sent before the socket is up.
    key(&mut app, KeyCode::Char('l'), KeyModifiers::NONE);
    assert!(app.cmds.is_empty());
    opened(&mut app);
    key(&mut app, KeyCode::Char('l'), KeyModifiers::NONE);
    key(&mut app, KeyCode::Char('s'), KeyModifiers::NONE);
    key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(
        app.cmds,
        [
            Cmd::ShellInput(b"l".to_vec()),
            Cmd::ShellInput(b"s".to_vec()),
            Cmd::ShellInput(b"\r".to_vec()),
            Cmd::ShellInput(vec![3]),
        ]
    );
    app.cmds.clear();
    // The composer saw none of it.
    assert!(app.attached.as_ref().unwrap().composer.is_empty());

    app.update(Msg::Paste("echo hi\n".into()));
    assert_eq!(app.cmds, [Cmd::ShellInput(b"echo hi\n".to_vec())]);
    app.cmds.clear();

    // Ctrl+` hands the keys back to the composer; the pane stays.
    ctrl_backtick(&mut app);
    key(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
    assert!(app.cmds.is_empty());
    let a = app.attached.as_ref().unwrap();
    assert_eq!(a.composer.text(), "x");
    assert!(!a.shell.as_ref().unwrap().focused);

    // Esc with a draft clears the draft first; on an empty draft it
    // closes the pane.
    key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.attached.as_ref().unwrap().composer.is_empty());
    assert!(app.attached.as_ref().unwrap().shell.is_some());
    key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.attached.as_ref().unwrap().shell.is_none());
    assert_eq!(app.cmds, [Cmd::ShellClose]);
}

#[test]
fn the_pane_renders_the_emulated_screen_60x16() {
    let mut app = common::attached_app(false, 60, 16);
    ctrl_backtick(&mut app);
    opened(&mut app);
    app.update(Msg::Net(NetMsg::ShellTitle {
        session_id: "s-1".into(),
        title: "bash".into(),
    }));
    output(
        &mut app,
        b"$ ls\r\nCargo.toml  \x1b[1;34msrc\x1b[0m  tests\r\n$ ",
    );
    insta::assert_snapshot!(frame(&mut app, 60, 16));
}

#[test]
fn an_ended_shell_reopens_on_enter_and_closes_on_esc() {
    let mut app = common::attached_app(false, 60, 16);
    ctrl_backtick(&mut app);
    opened(&mut app);
    output(&mut app, b"bye");
    app.cmds.clear();
    app.update(Msg::Net(NetMsg::ShellEnded {
        session_id: "s-1".into(),
        error: None,
    }));
    let sh = app.attached.as_ref().unwrap().shell.as_ref().unwrap();
    assert_eq!(sh.ended.as_deref(), Some("the shell exited"));
    // Keys no longer go anywhere.
    key(&mut app, KeyCode::Char('l'), KeyModifiers::NONE);
    assert!(app.cmds.is_empty());
    key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        app.cmds,
        [Cmd::ShellOpen {
            session_id: "s-1".into(),
            cols: 60,
            rows: 5
        }]
    );
    app.cmds.clear();
    let sh = app.attached.as_ref().unwrap().shell.as_ref().unwrap();
    assert!(sh.ended.is_none(), "a reopen clears the end");
    assert!(sh.screen().contents().trim().is_empty(), "fresh screen");

    app.update(Msg::Net(NetMsg::ShellEnded {
        session_id: "s-1".into(),
        error: Some("Unavailable: connection refused".into()),
    }));
    key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.attached.as_ref().unwrap().shell.is_none());
    assert!(app.cmds.is_empty(), "nothing to close on the wire");
}

#[test]
fn the_screen_follows_a_resize() {
    let mut app = common::attached_app(false, 60, 16);
    ctrl_backtick(&mut app);
    opened(&mut app);
    app.cmds.clear();
    app.update(Msg::Resize(100, 40));
    // Body 39 rows: two fifths is 15, less the header.
    assert_eq!(
        app.cmds,
        [Cmd::ShellResize {
            cols: 100,
            rows: 14
        }]
    );
    app.cmds.clear();
    let sh = app.attached.as_ref().unwrap().shell.as_ref().unwrap();
    assert_eq!((sh.cols, sh.rows), (100, 14));
    app.update(Msg::Resize(100, 40));
    assert!(app.cmds.is_empty(), "same size, no frame");
}

#[test]
fn native_scrollback_mode_shows_the_shell_as_an_overlay_60x12() {
    let mut app = common::attached_app(true, 60, 12);
    ctrl_backtick(&mut app);
    // The overlay owns the whole terminal less the header and footer.
    assert_eq!(
        app.cmds,
        [Cmd::ShellOpen {
            session_id: "s-1".into(),
            cols: 60,
            rows: 10
        }]
    );
    opened(&mut app);
    output(&mut app, b"$ uname\r\nLinux\r\n$ ");
    let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
    let theme = Theme::new(false);
    terminal
        .draw(|f| {
            let area = f.area();
            draw_overlay(f.buffer_mut(), area, &app, &theme);
        })
        .unwrap();
    insta::assert_snapshot!(terminal.backend().to_string());
}
