//! The program status (OSC 7501), the title and the alerts, driven
//! through the app as the runtime drives them: one `Reporter::frame` per
//! drawn frame, the bytes it writes asserted.

mod common;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use engram_tui::app::{App, Msg, NetMsg};
use engram_tui::status::{attention, Kind, Note, Reporter, State};
use engram_tui::terminal::{scan, Probe};

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

/// A run's end as the harness reports it: the run event, then idle.
fn end_run(app: &mut App, kind: &str, payload: &str) {
    live(app, 10, kind, payload);
    live(app, 11, "harness_idle", "{}");
}

fn key(app: &mut App, code: KeyCode) {
    app.update(Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

/// One frame's output as text.
fn frame(r: &mut Reporter, app: &App) -> String {
    let mut out = Vec::new();
    r.frame(app, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

fn b64(s: &str) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in s.as_bytes().chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let v = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(A[((v >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn report(state: &str, extra: &str) -> String {
    format!(
        "\x1b]7501;state={state}:app=engrams{extra}:title={}",
        b64("Add a README")
    )
}

#[test]
fn a_replayed_history_is_idle_not_an_unseen_result() {
    // The scripted history ends with a completed run.
    let app = common::attached_app(false, 60, 16);
    assert_eq!(attention(&app).state, State::Idle);
}

#[test]
fn a_run_reports_working_then_done_until_a_key() {
    let mut app = common::attached_app(false, 60, 16);
    let mut r = Reporter::new(true);
    let first = frame(&mut r, &app);
    assert!(first.starts_with(&report("idle", "")), "{first:?}");
    assert!(first.contains("\x1b]2;Add a README · engrams\x1b\\"));

    start_run(&mut app);
    assert!(frame(&mut r, &app).starts_with(&report("working", "")));
    // Nothing changed: a steady frame writes nothing.
    assert_eq!(frame(&mut r, &app), "");

    end_run(&mut app, "run_completed", r#"{"run_id":"r2","ok":true}"#);
    let done = frame(&mut r, &app);
    assert!(
        done.starts_with(&format!("{}:msg={}", report("done", ""), b64("Finished"))),
        "{done:?}"
    );
    // Focused: no bell.
    assert!(!done.contains('\x07'));

    // Focus alone is not acknowledgment; a key is.
    app.update(Msg::Focus(false));
    app.update(Msg::Focus(true));
    assert_eq!(attention(&app).state, State::Done);
    key(&mut app, KeyCode::Char('x'));
    assert_eq!(attention(&app).state, State::Idle);
}

#[test]
fn a_waiting_question_blocks_and_alerts_an_unfocused_terminal() {
    let mut app = common::attached_app(false, 60, 16);
    let mut r = Reporter::new(true);
    frame(&mut r, &app);
    app.update(Msg::Focus(false));
    start_run(&mut app);
    frame(&mut r, &app);
    live(&mut app, 10, "tool_call_requested", QUESTION);
    let out = frame(&mut r, &app);
    assert!(
        out.contains(&format!(
            "{}:msg={}",
            report("blocked", ":kind=question"),
            b64("Which database?")
        )),
        "{out:?}"
    );
    assert!(out.contains(
        "\x07\x1b]9;Add a README: needs you\x07\x1b]777;notify;engrams;Add a README: needs you\x07"
    ));
    // The same state on the next frame: no second alert.
    assert_eq!(frame(&mut r, &app), "");
}

#[test]
fn a_plan_blocks_for_permission() {
    let mut app = common::attached_app(false, 60, 16);
    start_run(&mut app);
    live(&mut app, 10, "tool_call_requested", PLAN);
    let at = attention(&app);
    assert_eq!(at.state, State::Blocked(Kind::Permission));
    assert_eq!(at.note, Note::Text("Approve the plan?"));
}

#[test]
fn a_failed_run_is_an_error_and_alerts() {
    let mut app = common::attached_app(false, 60, 16);
    let mut r = Reporter::new(true);
    start_run(&mut app);
    frame(&mut r, &app);
    app.update(Msg::Focus(false));
    end_run(&mut app, "run_completed", r#"{"run_id":"r2","ok":false}"#);
    let out = frame(&mut r, &app);
    assert!(out.contains(&report("error", "")), "{out:?}");
    assert!(out.contains("\x1b]9;Add a README: failed\x07"));
}

#[test]
fn an_interrupt_is_idle() {
    let mut app = common::attached_app(false, 60, 16);
    start_run(&mut app);
    end_run(&mut app, "run_interrupted", r#"{"run_id":"r2"}"#);
    assert_eq!(attention(&app).state, State::Idle);
}

#[test]
fn a_session_that_fails_live_is_an_error() {
    let mut app = common::attached_app(false, 60, 16);
    live(
        &mut app,
        10,
        "status_changed",
        r#"{"from":"active","to":"host_lost"}"#,
    );
    let at = attention(&app);
    assert_eq!(at.state, State::Error);
    assert_eq!(at.note, Note::Text("The session failed"));
}

#[test]
fn the_task_list_is_the_progress() {
    let mut app = common::attached_app(false, 60, 16);
    start_run(&mut app);
    live(
        &mut app,
        10,
        "tool_call_started",
        r#"{"run_id":"r2","tool_call_id":"t9","tool_name":"TodoWrite","args_summary":"{\"todos\":[{\"content\":\"a\",\"status\":\"completed\"},{\"content\":\"b\",\"status\":\"completed\"},{\"content\":\"c\",\"status\":\"in_progress\"},{\"content\":\"d\",\"status\":\"pending\"},{\"content\":\"e\",\"status\":\"pending\"}]}"}"#,
    );
    let at = attention(&app);
    assert_eq!(at.progress, Some(40));
    assert_eq!(at.note, Note::Tasks(2, 5));
    let mut r = Reporter::new(true);
    assert!(frame(&mut r, &app).contains(":progress=40:"));
}

/// Claude Code's task tools, in the shape a production session sent them:
/// the id comes back in the create result, the update names it.
#[test]
fn the_task_tools_are_the_progress() {
    let mut app = common::attached_app(false, 60, 16);
    start_run(&mut app);
    for (n, id) in ["1", "2", "3"].iter().enumerate() {
        let n = n as i64 * 2;
        live(
            &mut app,
            10 + n,
            "tool_call_started",
            &format!(
                r#"{{"run_id":"r2","tool_call_id":"c{id}","tool_name":"TaskCreate","args_summary":"{{\"description\":\"Task\",\"subject\":\"{id}\"}}"}}"#
            ),
        );
        live(
            &mut app,
            11 + n,
            "tool_call_completed",
            &format!(
                r#"{{"run_id":"r2","tool_call_id":"c{id}","tool_name":"TaskCreate","ok":true,"result_summary":"Task #{id} created successfully: {id}"}}"#
            ),
        );
    }
    assert_eq!(attention(&app).note, Note::Tasks(0, 3));
    live(
        &mut app,
        20,
        "tool_call_started",
        r#"{"run_id":"r2","tool_call_id":"u1","tool_name":"TaskUpdate","args_summary":"{\"status\":\"completed\",\"taskId\":\"1\"}"}"#,
    );
    let at = attention(&app);
    assert_eq!(at.note, Note::Tasks(1, 3));
    assert_eq!(at.progress, Some(33));
    live(
        &mut app,
        21,
        "tool_call_started",
        r#"{"run_id":"r2","tool_call_id":"u2","tool_name":"TaskUpdate","args_summary":"{\"status\":\"deleted\",\"taskId\":\"3\"}"}"#,
    );
    assert_eq!(attention(&app).note, Note::Tasks(1, 2));
}

/// A production session's order after an answer: the result, a harness
/// idle, then the run it resumes. The gap is working, never idle.
#[test]
fn an_answered_question_is_working_until_its_run_starts() {
    let mut app = common::attached_app(false, 60, 16);
    start_run(&mut app);
    live(&mut app, 10, "tool_call_requested", QUESTION);
    live(
        &mut app,
        11,
        "run_completed",
        r#"{"run_id":"r2","ok":true}"#,
    );
    assert_eq!(attention(&app).state, State::Blocked(Kind::Question));
    live(
        &mut app,
        12,
        "tool_result_submitted",
        r#"{"tool_call_id":"q1","result_json":"{\"Which database?\":[\"SQLite\"]}"}"#,
    );
    live(&mut app, 13, "harness_idle", "{}");
    assert_eq!(attention(&app).state, State::Working);
    live(&mut app, 14, "run_started", r#"{"run_id":"r3"}"#);
    assert_eq!(attention(&app).state, State::Working);
    live(
        &mut app,
        15,
        "run_completed",
        r#"{"run_id":"r3","ok":true}"#,
    );
    live(&mut app, 16, "harness_idle", "{}");
    assert_eq!(attention(&app).state, State::Done);
}

#[test]
fn a_refused_key_blocks_for_auth_until_a_success() {
    let mut app = common::attached_app(false, 60, 16);
    app.update(Msg::Net(NetMsg::AuthFailed));
    let at = attention(&app);
    assert_eq!(at.state, State::Blocked(Kind::Auth));
    assert_eq!(at.note, Note::Text("Run engrams auth login"));
    app.update(Msg::Net(NetMsg::Sessions(Ok(common::rows()))));
    assert_eq!(attention(&app).state, State::Idle);
}

#[test]
fn the_session_list_says_the_key_was_refused() {
    let mut app = App::new(false, 70, 8);
    app.update(Msg::Net(NetMsg::AuthFailed));
    app.update(Msg::Net(NetMsg::Sessions(Err(
        "Unauthenticated: bad key".into()
    ))));
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(70, 8)).unwrap();
    let theme = engram_tui::render::Theme::new(false);
    let mut scratch = Vec::new();
    terminal
        .draw(|f| {
            let area = f.area();
            engram_tui::render::draw_fullscreen(
                f.buffer_mut(),
                area,
                &mut app,
                &theme,
                &mut scratch,
            );
        })
        .unwrap();
    let screen: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(screen.contains("the API key was refused"), "{screen}");
    assert!(!screen.contains("no sessions yet"));
}

#[test]
fn attaching_another_session_is_not_an_alert() {
    let mut app = common::attached_app(false, 60, 16);
    let mut r = Reporter::new(true);
    start_run(&mut app);
    live(&mut app, 10, "tool_call_requested", QUESTION);
    app.update(Msg::Focus(false));
    // Prime the reporter with the blocked state of s-1, then attach s-2
    // and give it a pending question of its own from history.
    frame(&mut r, &app);
    app.attach("s-2");
    app.update(Msg::Net(NetMsg::History {
        session_id: "s-2".into(),
        events: vec![
            common::env(0, "run_started", r#"{"run_id":"r2","prompt_id":"p2"}"#),
            common::env(1, "tool_call_requested", QUESTION),
        ],
        older: false,
    }));
    let out = frame(&mut r, &app);
    assert!(out.contains("state=blocked"), "{out:?}");
    assert!(!out.contains('\x07'), "a new session is not an alert");
}

#[test]
fn without_support_the_title_carries_the_state_and_no_report_is_sent() {
    let mut app = common::attached_app(false, 60, 16);
    let mut r = Reporter::new(false);
    start_run(&mut app);
    let out = frame(&mut r, &app);
    assert!(!out.contains("7501"));
    assert_eq!(out, "\x1b]2;⟳ Add a README · engrams\x1b\\");
}

#[test]
fn a_title_never_carries_control_characters() {
    let mut app = common::attached_app(false, 60, 16);
    app.attached.as_mut().unwrap().title = "x\x07\x1b]0;evil\x1b\\".into();
    let mut r = Reporter::new(false);
    assert_eq!(frame(&mut r, &app), "\x1b]2;x]0;evil\\ · engrams\x1b\\");
}

#[test]
fn the_probe_reads_both_replies_and_stops_at_the_sentinel() {
    let both = b"\x1b[?1u\x1b]7501;?:states=idle,working,done,blocked,error\x1b\\\x1b[?62;22c";
    assert_eq!(
        scan(both),
        (
            Probe {
                kitty: true,
                program_status: true
            },
            true
        )
    );
    // A terminal that knows neither answers DA1 only.
    assert_eq!(scan(b"\x1b[?62;c"), (Probe::default(), true));
    // The sentinel has not arrived yet: keep reading.
    assert_eq!(
        scan(b"\x1b]7501;?\x07"),
        (
            Probe {
                kitty: false,
                program_status: true
            },
            false
        )
    );
}
