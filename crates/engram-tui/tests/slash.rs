//! Slash commands (ADR 0124 PR8): `/` in an empty composer lists the
//! commands, Tab completes, Enter runs the one the draft names.

mod common;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use engram_tui::app::{App, Cmd, Msg, NetMsg, Screen};
use engram_tui::render::{draw_fullscreen, Theme};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn key(app: &mut App, code: KeyCode) {
    app.update(Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

fn live(app: &mut App, idx: i64, kind: &str, payload: &str) {
    app.update(Msg::Net(NetMsg::Live {
        session_id: "s-1".into(),
        envelope: common::env(idx, kind, payload),
    }));
}

fn type_str(app: &mut App, s: &str) {
    for c in s.chars() {
        key(app, KeyCode::Char(c));
    }
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

#[test]
fn the_popup_lists_matches_and_tab_completes() {
    let mut app = common::attached_app(false, 60, 18);
    type_str(&mut app, "/s");
    insta::assert_snapshot!("popup_60x18", frame(&mut app, 60, 18));
    key(&mut app, KeyCode::Tab);
    assert_eq!(app.attached.as_ref().unwrap().composer.text(), "/sessions");
    // A prompt that merely starts with a slash and a space is not a command.
    app.attached.as_mut().unwrap().composer.clear();
    type_str(&mut app, "/s x");
    assert_eq!(engram_tui::render::slash_rows("/s x"), 0);
}

#[test]
fn enter_runs_the_named_command() {
    let mut app = common::attached_app(false, 60, 18);
    app.cmds.clear();

    // `/s` is ambiguous: nothing runs, the draft stays.
    type_str(&mut app, "/s");
    key(&mut app, KeyCode::Enter);
    assert!(app.cmds.is_empty());
    assert_eq!(app.attached.as_ref().unwrap().composer.text(), "/s");
    app.attached.as_mut().unwrap().composer.clear();

    // `/resume` only on an idle session.
    type_str(&mut app, "/resume");
    key(&mut app, KeyCode::Enter);
    assert!(app.cmds.is_empty());
    app.update(Msg::Net(NetMsg::Status {
        session_id: "s-1".into(),
        status: "idle".into(),
        title: None,
    }));
    type_str(&mut app, "/resume");
    key(&mut app, KeyCode::Enter);
    assert_eq!(
        app.cmds,
        vec![Cmd::Resume {
            session_id: "s-1".into()
        }]
    );
    app.cmds.clear();

    // `/dump` carries the loaded transcript as text.
    type_str(&mut app, "/dump");
    key(&mut app, KeyCode::Enter);
    let [Cmd::Dump { text }] = app.cmds.as_slice() else {
        panic!("expected a dump, got {:?}", app.cmds);
    };
    assert!(text.starts_with("› Add a README to the repo.\n\n"));
    assert!(text.contains("✓ Bash ls -la\n    Cargo.toml\n"));
    assert!(text.contains("· resumed\n"));
    app.cmds.clear();

    type_str(&mut app, "/transcript");
    key(&mut app, KeyCode::Enter);
    assert!(matches!(app.cmds.as_slice(), [Cmd::ViewTranscript { .. }]));
    app.cmds.clear();

    type_str(&mut app, "/new");
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.cmds, vec![Cmd::LoadProfiles]);
    assert!(app.new_session.is_some());
    key(&mut app, KeyCode::Esc);
    app.cmds.clear();

    // `/shell` opens the pane and hands it the keys, so it goes last.
    type_str(&mut app, "/sh");
    key(&mut app, KeyCode::Enter);
    assert!(matches!(app.cmds.as_slice(), [Cmd::ShellOpen { .. }]));
    assert!(app.attached.as_ref().unwrap().composer.is_empty());
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('`'),
        KeyModifiers::CONTROL,
    )));
    app.cmds.clear();

    type_str(&mut app, "/sessions");
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.screen, Screen::Sessions);
}

#[test]
fn dump_is_a_note_in_native_scrollback_mode() {
    let mut app = common::attached_app(true, 60, 14);
    app.cmds.clear();
    type_str(&mut app, "/dump");
    key(&mut app, KeyCode::Enter);
    assert!(app.cmds.is_empty());
    assert!(app.note.is_some());
}

#[test]
fn plan_toggles_the_mode_the_next_prompt_asks_for() {
    let mut app = common::attached_app(false, 60, 18);
    app.cmds.clear();
    type_str(&mut app, "/plan");
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.attached.as_ref().unwrap().composer_mode(), "plan");
    app.note = None;
    assert!(frame(&mut app, 60, 18).contains("· plan"));
    type_str(&mut app, "draft a plan");
    key(&mut app, KeyCode::Enter);
    let [Cmd::Send { harness_mode, .. }] = app.cmds.as_slice() else {
        panic!("expected a send, got {:?}", app.cmds);
    };
    assert_eq!(harness_mode.as_deref(), Some("plan"));
    app.cmds.clear();
    // The override is spent; the next prompt carries no mode. Once the
    // session reports plan mode, `/plan` again asks for default.
    type_str(&mut app, "more");
    key(&mut app, KeyCode::Enter);
    let [Cmd::Send { harness_mode, .. }] = app.cmds.as_slice() else {
        panic!("expected a send, got {:?}", app.cmds);
    };
    assert_eq!(harness_mode, &None);
    app.cmds.clear();
    live(&mut app, 20, "harness_mode_changed", r#"{"mode":"plan"}"#);
    assert_eq!(app.attached.as_ref().unwrap().composer_mode(), "plan");
    type_str(&mut app, "/plan");
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.attached.as_ref().unwrap().composer_mode(), "default");
    type_str(&mut app, "go");
    key(&mut app, KeyCode::Enter);
    let [Cmd::Send { harness_mode, .. }] = app.cmds.as_slice() else {
        panic!("expected a send, got {:?}", app.cmds);
    };
    assert_eq!(harness_mode.as_deref(), Some("default"));
}
