//! Regressions for defects the adversarial reviews of the program status
//! work found: each test is the event sequence that went wrong.

mod common;

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use engram_tui::app::{App, Msg, NetMsg};
use engram_tui::sessions::SessionRow;
use engram_tui::status::{attention, Alert, Reporter, State};

const QUESTION: &str = r#"{"run_id":"r2","tool_call_id":"q1","name":"ask_user_question","args_json":"{\"questions\":[{\"question\":\"Which database?\",\"header\":\"db\",\"options\":[{\"label\":\"Postgres\"},{\"label\":\"SQLite\"}]}]}"}"#;

const PLAN: &str = r###"{"run_id":"r2","tool_call_id":"pl1","name":"exit_plan_mode","args_json":"{\"plan\":\"## Plan\\n\\n1. add the section\"}"}"###;

fn live(app: &mut App, idx: i64, kind: &str, payload: &str) {
    app.update(Msg::Net(NetMsg::Live {
        session_id: "s-1".into(),
        envelope: common::env(idx, kind, payload),
    }));
}

fn start_run(app: &mut App) {
    live(app, 9, "run_started", r#"{"run_id":"r2","prompt_id":"p2"}"#);
}

fn frame(r: &mut Reporter, app: &App) -> String {
    let mut out = Vec::new();
    r.frame(app, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

fn shell_output(app: &mut App, bytes: &'static [u8]) {
    app.update(Msg::Net(NetMsg::ShellOutput {
        session_id: "s-1".into(),
        bytes: bytes::Bytes::from_static(bytes),
    }));
}

fn with_shell() -> App {
    let mut app = common::attached_app(false, 60, 16);
    app.program_status = true;
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('`'),
        KeyModifiers::CONTROL,
    )));
    app.update(Msg::Net(NetMsg::ShellOpened {
        session_id: "s-1".into(),
    }));
    app.cmds.clear();
    app
}

#[test]
fn a_session_lost_mid_run_is_an_error_not_working() {
    let mut app = common::attached_app(false, 60, 16);
    start_run(&mut app);
    live(
        &mut app,
        10,
        "status_changed",
        r#"{"from":"active","to":"host_lost"}"#,
    );
    assert_eq!(attention(&app).state, State::Error);
}

#[test]
fn a_question_in_the_first_history_is_not_an_alert() {
    let mut app = App::new(false, 60, 16);
    app.update(Msg::Net(NetMsg::Sessions(Ok(common::rows()))));
    let mut r = Reporter::with_alert(true, Alert::Osc9);
    frame(&mut r, &app);
    app.attach("s-1");
    // The loading frame comes before the history.
    frame(&mut r, &app);
    app.update(Msg::Focus(false));
    app.update(Msg::Net(NetMsg::History {
        session_id: "s-1".into(),
        events: vec![
            common::env(0, "run_started", r#"{"run_id":"r2","prompt_id":"p2"}"#),
            common::env(1, "tool_call_requested", QUESTION),
        ],
        older: false,
    }));
    let out = frame(&mut r, &app);
    assert!(out.contains("state=blocked"), "{out:?}");
    assert!(!out.contains('\x07'), "history is not a change");
}

#[test]
fn an_approved_plan_works_through_its_handoff() {
    let mut app = common::attached_app(false, 60, 16);
    let mut r = Reporter::with_alert(true, Alert::Osc9);
    start_run(&mut app);
    live(&mut app, 10, "tool_call_requested", PLAN);
    frame(&mut r, &app);
    app.update(Msg::Focus(false));
    live(
        &mut app,
        11,
        "tool_result_submitted",
        r#"{"tool_call_id":"pl1","result_json":"{\"decision\":\"approve\"}"}"#,
    );
    // The handoff interrupts the plan run; the implementation run follows.
    live(&mut app, 12, "run_interrupted", r#"{"run_id":"r2"}"#);
    live(&mut app, 13, "harness_idle", "{}");
    assert_eq!(attention(&app).state, State::Working);
    assert!(!frame(&mut r, &app).contains('\x07'), "no done alert");
    live(&mut app, 14, "run_started", r#"{"run_id":"r3"}"#);
    assert_eq!(attention(&app).state, State::Working);
}

#[test]
fn a_session_that_goes_unreachable_mid_run_has_no_result() {
    let mut app = common::attached_app(false, 60, 16);
    start_run(&mut app);
    live(
        &mut app,
        10,
        "status_changed",
        r#"{"from":"active","to":"unreachable"}"#,
    );
    assert_eq!(attention(&app).state, State::Idle);
}

#[test]
fn a_full_reset_in_the_shell_clears_its_records() {
    let mut app = with_shell();
    let mut r = Reporter::with_alert(true, Alert::Osc9);
    shell_output(&mut app, b"\x1b]7501;state=error:id=job\x07");
    frame(&mut r, &app);
    // RIS split across two chunks.
    shell_output(&mut app, b"\x1b");
    shell_output(&mut app, b"c");
    assert_eq!(
        frame(&mut r, &app),
        "\x1b]7501;state=clear:id=shell/job\x1b\\"
    );
    // A report after the reset in the same chunk survives it.
    shell_output(&mut app, b"\x1bc\x1b]7501;state=done:id=next\x07");
    assert!(frame(&mut r, &app).contains("state=done:id=shell/next"));
}

#[test]
fn the_attached_sessions_children_come_before_other_sessions() {
    let mut app = with_shell();
    shell_output(&mut app, b"\x1b]7501;state=done:id=job\x07");
    let rows: Vec<SessionRow> = (0..30)
        .map(|i| SessionRow {
            session_id: format!("x{i}"),
            title: format!("t{i}"),
            status: "active".into(),
            age_secs: None,
            needs_you: true,
            running: None,
        })
        .collect();
    app.update(Msg::Net(NetMsg::Sessions(Ok(rows))));
    app.update(Msg::Net(NetMsg::SessionsRunning {
        running: Vec::new(),
        known: true,
    }));
    let mut r = Reporter::with_alert(true, Alert::Osc9);
    let out = frame(&mut r, &app);
    assert!(
        out.contains("id=shell/job"),
        "the pane's record survives the cap"
    );
    // The root, the pane's record, and 24 other sessions.
    assert_eq!(out.matches("\x1b]7501;state=").count(), 1 + 1 + 24);
}

#[test]
fn the_poll_slows_in_the_background_and_backs_off_on_errors() {
    let mut app = common::attached_app(false, 60, 16);
    assert_eq!(app.poll_interval(), Duration::from_secs(15));
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('d'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!(app.poll_interval(), Duration::from_secs(5));
    for _ in 0..6 {
        app.update(Msg::Net(NetMsg::Sessions(Err("down".into()))));
    }
    assert_eq!(app.poll_interval(), Duration::from_secs(60));
    app.update(Msg::Net(NetMsg::Sessions(Ok(common::rows()))));
    assert_eq!(app.poll_interval(), Duration::from_secs(5));
}
