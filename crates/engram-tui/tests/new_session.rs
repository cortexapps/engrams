//! The new-session flow (ADR 0124 PR8): Ctrl+N lists the profiles, Enter
//! picks one, the first prompt creates the task, and the TUI attaches to
//! its primary session.

mod common;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use engram_tui::app::{App, Cmd, Msg, NetMsg, NewStage, Screen};
use engram_tui::render::{draw_fullscreen, Theme};
use engram_tui::sessions::ProfileRow;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn key(app: &mut App, code: KeyCode, m: KeyModifiers) {
    app.update(Msg::Key(KeyEvent::new(code, m)));
}

fn type_str(app: &mut App, s: &str) {
    for c in s.chars() {
        key(app, KeyCode::Char(c), KeyModifiers::NONE);
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

fn profiles() -> Vec<ProfileRow> {
    vec![
        ProfileRow {
            id: "pr-1".into(),
            name: "default".into(),
            description: "the standard image".into(),
        },
        ProfileRow {
            id: "pr-2".into(),
            name: "research".into(),
            description: "read-only, no git push".into(),
        },
    ]
}

#[test]
fn ctrl_n_lists_profiles_and_enter_creates_the_task() {
    let mut app = App::new(false, 60, 16);
    app.update(Msg::Net(NetMsg::Sessions(Ok(common::rows()))));
    app.cmds.clear();
    key(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    assert_eq!(app.cmds, vec![Cmd::LoadProfiles]);
    app.cmds.clear();
    app.update(Msg::Net(NetMsg::Profiles(Ok(profiles()))));
    insta::assert_snapshot!("picker_60x16", frame(&mut app, 60, 16));

    // Filter to the second profile and pick it.
    type_str(&mut app, "res");
    key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.new_session.as_ref().unwrap().stage, NewStage::Prompt);
    type_str(&mut app, "Write the release notes.");
    insta::assert_snapshot!("prompt_60x16", frame(&mut app, 60, 16));
    key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        app.cmds,
        vec![Cmd::CreateTask {
            profile_id: "pr-2".into(),
            prompt: "Write the release notes.".into(),
        }]
    );
    assert!(app.new_session.as_ref().unwrap().creating);
    // A second Enter while the RPC is in flight does nothing.
    app.cmds.clear();
    key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.cmds.is_empty());

    app.update(Msg::Net(NetMsg::Created(Ok((
        "s-new".into(),
        "Write the release notes.".into(),
    )))));
    assert!(app.new_session.is_none());
    assert_eq!(app.screen, Screen::Attached);
    let a = app.attached.as_ref().unwrap();
    assert_eq!(a.session_id, "s-new");
    assert_eq!(a.title, "Write the release notes.");
    assert!(app.cmds.contains(&Cmd::Attach {
        session_id: "s-new".into()
    }));
    assert!(app.cmds.contains(&Cmd::LoadSessions));
}

#[test]
fn a_failed_create_reopens_the_prompt_with_the_error() {
    let mut app = App::new(false, 60, 16);
    key(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    app.update(Msg::Net(NetMsg::Profiles(Ok(profiles()))));
    key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    type_str(&mut app, "hello");
    key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    app.update(Msg::Net(NetMsg::Created(Err(
        "PermissionDenied: profile archived".into(),
    ))));
    let ns = app.new_session.as_ref().unwrap();
    assert!(!ns.creating);
    assert_eq!(
        ns.error.as_deref(),
        Some("PermissionDenied: profile archived")
    );
    assert_eq!(ns.stage, NewStage::Prompt);
    assert!(frame(&mut app, 60, 16).contains("profile archived"));
    // Esc goes back to the list; Esc again closes the overlay.
    key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.new_session.as_ref().unwrap().stage, NewStage::Pick);
    key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.new_session.is_none());
    assert_eq!(app.screen, Screen::Sessions);
}

#[test]
fn the_picker_opens_over_an_attached_session_too() {
    let mut app = common::attached_app(false, 60, 16);
    app.cmds.clear();
    key(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    assert_eq!(app.cmds, vec![Cmd::LoadProfiles]);
    app.update(Msg::Net(NetMsg::Profiles(Err("Unavailable: boom".into()))));
    assert!(frame(&mut app, 60, 16).contains("Unavailable: boom"));
    // Ctrl+C closes it and the draft underneath is untouched.
    key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.new_session.is_none());
    assert_eq!(app.screen, Screen::Attached);
}
