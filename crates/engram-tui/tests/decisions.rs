//! Questions and plan decisions answered from the keyboard through
//! `CompleteToolCall` (ADR 0124 PR6): the number keys pick, Enter confirms,
//! Tab collects feedback; the cell locks optimistically and reopens when
//! the RPC fails.

mod common;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use engram_tui::app::{App, Cmd, Msg, NetMsg};
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

const QUESTION: &str = r#"{"run_id":"r2","tool_call_id":"q1","name":"ask_user_question","args_json":"{\"questions\":[{\"question\":\"Which database?\",\"header\":\"db\",\"options\":[{\"label\":\"Postgres\",\"description\":\"\"},{\"label\":\"SQLite\",\"description\":\"\"}]},{\"question\":\"Which features?\",\"multiSelect\":true,\"options\":[{\"label\":\"auth\"},{\"label\":\"search\"},{\"label\":\"export\"}]}]}"}"#;

const PLAN: &str = r###"{"run_id":"r2","tool_call_id":"pl1","name":"exit_plan_mode","args_json":"{\"plan\":\"## Plan\\n\\n1. add the section\\n2. run the tests\"}"}"###;

fn with_question() -> App {
    let mut app = common::attached_app(false, 60, 16);
    live(
        &mut app,
        9,
        "run_started",
        r#"{"run_id":"r2","prompt_id":"p2"}"#,
    );
    live(&mut app, 10, "tool_call_requested", QUESTION);
    app.cmds.clear();
    app
}

#[test]
fn number_keys_pick_and_enter_submits_the_canonical_answers() {
    let mut app = with_question();
    assert!(app.attached.as_ref().unwrap().reducer.awaiting_decision());
    key(&mut app, KeyCode::Char('2'));
    // Single select: a second pick replaces the first.
    key(&mut app, KeyCode::Char('1'));
    key(&mut app, KeyCode::Enter);
    assert!(app.cmds.is_empty(), "the second question still waits");
    key(&mut app, KeyCode::Enter);
    assert!(app.cmds.is_empty(), "no pick yet on a multi-select");
    key(&mut app, KeyCode::Char('1'));
    key(&mut app, KeyCode::Char('3'));
    key(&mut app, KeyCode::Char('1'));
    key(&mut app, KeyCode::Char('9'));
    key(&mut app, KeyCode::Enter);
    let Some(Cmd::Complete {
        session_id,
        tool_call_id,
        result_json,
    }) = app.cmds.pop()
    else {
        panic!("expected a completion, got {:?}", app.cmds);
    };
    assert_eq!((session_id.as_str(), tool_call_id.as_str()), ("s-1", "q1"));
    assert_eq!(
        result_json,
        r#"{"Which database?":["Postgres"],"Which features?":["export"]}"#
    );
    let a = app.attached.as_ref().unwrap();
    assert!(!a.reducer.awaiting_decision(), "locked optimistically");
    assert!(a.reducer.is_running());
    // Typing afterwards goes to the composer as usual.
    key(&mut app, KeyCode::Char('1'));
    assert_eq!(app.attached.as_ref().unwrap().composer.text(), "1");
}

#[test]
fn a_failed_completion_reopens_the_question() {
    let mut app = with_question();
    key(&mut app, KeyCode::Char('1'));
    key(&mut app, KeyCode::Enter);
    key(&mut app, KeyCode::Char('2'));
    key(&mut app, KeyCode::Enter);
    assert!(!app.attached.as_ref().unwrap().reducer.awaiting_decision());
    app.update(Msg::Net(NetMsg::CompleteFailed {
        session_id: "s-1".into(),
        tool_call_id: "q1".into(),
        error: "Unavailable: boom".into(),
    }));
    assert!(app.attached.as_ref().unwrap().reducer.awaiting_decision());
    assert!(app.note.as_ref().is_some_and(|(n, _)| n.contains("boom")));
}

#[test]
fn a_draft_in_progress_keeps_the_number_keys() {
    let mut app = with_question();
    key(&mut app, KeyCode::Char('x'));
    key(&mut app, KeyCode::Char('1'));
    assert_eq!(app.attached.as_ref().unwrap().composer.text(), "x1");
    key(&mut app, KeyCode::Enter);
    // Enter with a draft sends it (the harness queues it); the question
    // stays pending.
    assert!(matches!(app.cmds.last(), Some(Cmd::Send { text, .. }) if text == "x1"));
    assert!(app.attached.as_ref().unwrap().reducer.awaiting_decision());
}

#[test]
fn plan_choice_then_enter_and_tab_feedback() {
    let mut app = common::attached_app(false, 60, 16);
    live(
        &mut app,
        9,
        "run_started",
        r#"{"run_id":"r2","prompt_id":"p2"}"#,
    );
    live(&mut app, 10, "tool_call_requested", PLAN);
    app.cmds.clear();
    key(&mut app, KeyCode::Enter);
    assert!(app.cmds.is_empty(), "Enter alone decides nothing");
    key(&mut app, KeyCode::Char('1'));
    key(&mut app, KeyCode::Enter);
    let Some(Cmd::Complete { result_json, .. }) = app.cmds.pop() else {
        panic!("expected a completion");
    };
    assert_eq!(result_json, r#"{"decision":"approve"}"#);

    // A second plan: reject with feedback typed in the composer.
    live(
        &mut app,
        11,
        "tool_result_submitted",
        r#"{"tool_call_id":"pl1","result_json":"{\"decision\":\"approve\"}"}"#,
    );
    live(
        &mut app,
        12,
        "tool_call_requested",
        &PLAN.replace("pl1", "pl2"),
    );
    app.cmds.clear();
    key(&mut app, KeyCode::Tab);
    assert!(app.attached.as_ref().unwrap().feedback);
    for c in "add tests first".chars() {
        key(&mut app, KeyCode::Char(c));
    }
    key(&mut app, KeyCode::Enter);
    let Some(Cmd::Complete {
        tool_call_id,
        result_json,
        ..
    }) = app.cmds.pop()
    else {
        panic!("expected a completion");
    };
    assert_eq!(tool_call_id, "pl2");
    assert_eq!(
        result_json,
        r#"{"decision":"reject","feedback":"add tests first"}"#
    );
    let a = app.attached.as_ref().unwrap();
    assert!(!a.feedback);
    assert!(a.composer.is_empty());
}

#[test]
fn unfocused_terminal_gets_an_alert_when_a_decision_waits() {
    let mut app = common::attached_app(false, 60, 16);
    app.update(Msg::Focus(false));
    live(
        &mut app,
        9,
        "run_started",
        r#"{"run_id":"r2","prompt_id":"p2"}"#,
    );
    live(&mut app, 10, "tool_call_requested", QUESTION);
    assert_eq!(app.alerts, vec!["Add a README: needs you".to_string()]);
    app.alerts.clear();
    app.update(Msg::Focus(true));
    live(
        &mut app,
        11,
        "tool_result_submitted",
        r#"{"tool_call_id":"q1","result_json":"{}"}"#,
    );
    live(
        &mut app,
        12,
        "run_completed",
        r#"{"run_id":"r2","ok":true}"#,
    );
    assert!(app.alerts.is_empty(), "focused: no alert");
}

#[test]
fn pending_question_with_picks_60x16() {
    let mut app = with_question();
    key(&mut app, KeyCode::Char('2'));
    key(&mut app, KeyCode::Enter);
    key(&mut app, KeyCode::Char('1'));
    key(&mut app, KeyCode::Char('3'));
    let shot = frame(&mut app, 60, 16)
        .lines()
        .map(|l| {
            if l.contains("needs you") {
                "<footer: needs you>".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(shot);
}

#[test]
fn pending_plan_with_feedback_60x14() {
    let mut app = common::attached_app(false, 60, 14);
    live(
        &mut app,
        9,
        "run_started",
        r#"{"run_id":"r2","prompt_id":"p2"}"#,
    );
    live(&mut app, 10, "tool_call_requested", PLAN);
    key(&mut app, KeyCode::Tab);
    for c in "tests".chars() {
        key(&mut app, KeyCode::Char(c));
    }
    let shot = frame(&mut app, 60, 14)
        .lines()
        .map(|l| {
            if l.contains("needs you") {
                "<footer: needs you>".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(shot);
}
