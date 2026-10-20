//! Vim keys in lists: the session list has a normal mode (j/k, g/G, `/`
//! to filter, `n` new session) and every list or popup moves with
//! Ctrl+N/P and Ctrl+J/K beside the arrows.

mod common;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use engram_tui::app::{App, Cmd, Msg, NetMsg};

fn key(app: &mut App, code: KeyCode, m: KeyModifiers) {
    app.update(Msg::Key(KeyEvent::new(code, m)));
}

fn ch(app: &mut App, c: char) {
    key(app, KeyCode::Char(c), KeyModifiers::NONE);
}

fn ctrl(app: &mut App, c: char) {
    key(app, KeyCode::Char(c), KeyModifiers::CONTROL);
}

fn list() -> App {
    let mut app = App::new(false, 80, 12);
    app.update(Msg::Net(NetMsg::Sessions(Ok(common::rows()))));
    app.cmds.clear();
    app
}

#[test]
fn the_session_list_moves_with_vim_keys() {
    let mut app = list();
    ch(&mut app, 'j');
    assert_eq!(app.sessions.selected, 1);
    ch(&mut app, 'G');
    assert_eq!(app.sessions.selected, 2);
    ch(&mut app, 'k');
    assert_eq!(app.sessions.selected, 1);
    ch(&mut app, 'g');
    assert_eq!(app.sessions.selected, 0);
    ctrl(&mut app, 'n');
    ctrl(&mut app, 'j');
    assert_eq!(app.sessions.selected, 2);
    ctrl(&mut app, 'p');
    ctrl(&mut app, 'k');
    assert_eq!(app.sessions.selected, 0);
    // Letters are keys, not a filter.
    assert!(app.sessions.filter.is_empty());
    assert!(app.cmds.is_empty());
}

#[test]
fn slash_filters_and_esc_returns_to_the_keys() {
    let mut app = list();
    ch(&mut app, '/');
    assert!(app.sessions.filtering);
    for c in "flaky".chars() {
        ch(&mut app, c);
    }
    assert_eq!(app.sessions.filter, "flaky");
    assert_eq!(app.sessions.visible().count(), 1);
    // Esc keeps the filter and gives the keys back; j moves again.
    key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!app.sessions.filtering);
    assert_eq!(app.sessions.filter, "flaky");
    ch(&mut app, 'j');
    assert_eq!(app.sessions.filter, "flaky");
    // Esc again clears it; Enter attaches the selected session.
    key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.sessions.filter.is_empty());
    key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app
        .cmds
        .iter()
        .any(|c| matches!(c, Cmd::Attach { session_id } if session_id == "s-1")));
}

/// As from Vim's `/` prompt: Backspace on an empty filter gives the keys
/// back.
#[test]
fn backspace_out_of_an_empty_filter() {
    let mut app = list();
    ch(&mut app, '/');
    ch(&mut app, 'f');
    key(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    assert!(app.sessions.filtering, "it erased the letter");
    key(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    assert!(!app.sessions.filtering, "the empty filter is left");
    ch(&mut app, 'j');
    assert_eq!(app.sessions.selected, 1);
}

#[test]
fn n_opens_a_new_session_and_its_picker_moves_with_ctrl_n() {
    let mut app = list();
    ch(&mut app, 'n');
    assert_eq!(app.cmds, [Cmd::LoadProfiles]);
    app.update(Msg::Net(NetMsg::Profiles(Ok(vec![
        engram_tui::sessions::ProfileRow {
            id: "a".into(),
            name: "a".into(),
            description: String::new(),
        },
        engram_tui::sessions::ProfileRow {
            id: "b".into(),
            name: "b".into(),
            description: String::new(),
        },
    ]))));
    ctrl(&mut app, 'n');
    let ns = app.new_session.as_ref().expect("the picker stays open");
    assert_eq!(ns.selected, 1);
    ctrl(&mut app, 'p');
    assert_eq!(app.new_session.as_ref().unwrap().selected, 0);
}

#[test]
fn the_palette_filters_with_letters_and_moves_with_ctrl_n() {
    let mut app = common::attached_app(false, 80, 16);
    ctrl(&mut app, 'p');
    app.update(Msg::Net(NetMsg::Sessions(Ok(common::rows()))));
    assert!(app.palette);
    ch(&mut app, 'j');
    assert_eq!(app.sessions.filter, "j", "letters filter in the palette");
    key(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    ctrl(&mut app, 'n');
    assert_eq!(app.sessions.selected, 1);
    ctrl(&mut app, 'p');
    assert_eq!(app.sessions.selected, 0);
    assert!(app.palette, "Ctrl+P moves; it does not close the palette");
}
