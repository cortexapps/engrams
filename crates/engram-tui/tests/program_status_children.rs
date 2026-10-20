//! Child records beside the TUI's own: running subagents, the sandbox's
//! lifecycle, and what programs in the shell pane report. Driven through
//! the app, one `Reporter::frame` per drawn frame.

mod common;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use engram_tui::app::{App, Cmd, Msg, NetMsg};
use engram_tui::sessions::RunState;
use engram_tui::status::Reporter;

fn live(app: &mut App, idx: i64, kind: &str, payload: &str) {
    app.update(Msg::Net(NetMsg::Live {
        session_id: "s-1".into(),
        envelope: common::env(idx, kind, payload),
    }));
}

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

fn shell_output(app: &mut App, bytes: &'static [u8]) {
    app.update(Msg::Net(NetMsg::ShellOutput {
        session_id: "s-1".into(),
        bytes: bytes::Bytes::from_static(bytes),
    }));
}

/// An app with the shell pane open and connected, on a terminal that
/// takes reports.
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
fn a_subagent_is_a_child_while_it_runs() {
    let mut app = common::attached_app(false, 60, 16);
    let mut r = Reporter::new(true);
    live(&mut app, 9, "run_started", r#"{"run_id":"r2"}"#);
    frame(&mut r, &app);
    live(
        &mut app,
        10,
        "tool_call_started",
        r#"{"run_id":"r2","tool_call_id":"a1","tool_name":"Agent","args_summary":"{\"description\":\"Explore the auth module\",\"prompt\":\"look\"}"}"#,
    );
    let out = frame(&mut r, &app);
    assert_eq!(
        out,
        format!(
            "\x1b]7501;state=working:id=agent-1:title={}:msg={}\x1b\\",
            b64("Explore the auth module"),
            b64("Subagent")
        )
    );
    assert_eq!(frame(&mut r, &app), "", "a steady frame writes nothing");
    live(
        &mut app,
        11,
        "tool_call_completed",
        r#"{"run_id":"r2","tool_call_id":"a1","tool_name":"Agent","ok":true}"#,
    );
    assert_eq!(
        frame(&mut r, &app),
        "\x1b]7501;state=clear:id=agent-1\x1b\\"
    );
}

#[test]
fn the_sandbox_is_a_child_while_it_moves() {
    let mut app = common::attached_app(false, 60, 16);
    let mut r = Reporter::new(true);
    frame(&mut r, &app);
    live(
        &mut app,
        9,
        "status_changed",
        r#"{"from":"active","to":"evacuating"}"#,
    );
    let out = frame(&mut r, &app);
    assert!(
        out.contains(&format!(
            "\x1b]7501;state=working:id=vm:title={}:msg={}\x1b\\",
            b64("Sandbox"),
            b64("Moving to another host")
        )),
        "{out:?}"
    );
    live(
        &mut app,
        10,
        "status_changed",
        r#"{"from":"evacuating","to":"active"}"#,
    );
    assert!(frame(&mut r, &app).contains("\x1b]7501;state=clear:id=vm\x1b\\"));
}

#[test]
fn a_program_in_the_shell_reports_through_the_pane() {
    let mut app = with_shell();
    let mut r = Reporter::new(true);
    frame(&mut r, &app);
    // "cargo build" from a program in the remote shell, BEL-terminated.
    shell_output(
        &mut app,
        b"\x1b]7501;state=working:app=cargo:progress=40:msg=Y2FyZ28gYnVpbGQ=\x07",
    );
    let out = frame(&mut r, &app);
    assert_eq!(
        out,
        format!(
            "\x1b]7501;state=working:id=shell:app=cargo:progress=40:msg={}\x1b\\",
            b64("cargo build")
        )
    );
    // The shell ends: working is no longer true.
    app.update(Msg::Net(NetMsg::ShellEnded {
        session_id: "s-1".into(),
        error: None,
    }));
    assert_eq!(frame(&mut r, &app), "\x1b]7501;state=clear:id=shell\x1b\\");
}

#[test]
fn the_pane_answers_a_programs_queries() {
    let mut app = with_shell();
    shell_output(&mut app, b"\x1b]7501;?\x1b\\\x1b[c");
    assert_eq!(
        app.cmds,
        [Cmd::ShellInput(b"\x1b]7501;?\x1b\\\x1b[?62;22c".to_vec())]
    );
}

#[test]
fn a_child_beneath_a_cleared_record_is_sent_again() {
    let mut app = with_shell();
    let mut r = Reporter::new(true);
    shell_output(&mut app, b"\x1b]7501;state=working\x07");
    shell_output(&mut app, b"\x1b]7501;state=done:id=test\x07");
    frame(&mut r, &app);
    // The program's root goes; its child stays. Clearing `shell` on the
    // terminal clears `shell/test` too, so it is sent again.
    shell_output(&mut app, b"\x1b]133;A\x07");
    assert_eq!(
        frame(&mut r, &app),
        "\x1b]7501;state=clear:id=shell\x1b\\\x1b]7501;state=done:id=shell/test\x1b\\"
    );
}

#[test]
fn leaving_the_session_clears_its_children() {
    let mut app = common::attached_app(false, 60, 16);
    let mut r = Reporter::new(true);
    live(&mut app, 9, "run_started", r#"{"run_id":"r2"}"#);
    live(
        &mut app,
        10,
        "tool_call_started",
        r#"{"run_id":"r2","tool_call_id":"a1","tool_name":"Task","args_summary":"{\"description\":\"Find callers\"}"}"#,
    );
    frame(&mut r, &app);
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('d'),
        KeyModifiers::CONTROL,
    )));
    assert!(frame(&mut r, &app).contains("\x1b]7501;state=clear:id=agent-1\x1b\\"));
}

/// One poll as the network side delivers it: the list, then which active
/// sessions are running.
fn poll(app: &mut App, rows: Vec<engram_tui::sessions::SessionRow>, running: &[(&str, bool)]) {
    app.update(Msg::Net(NetMsg::Sessions(Ok(rows))));
    app.update(Msg::Net(NetMsg::SessionsRunning {
        running: running
            .iter()
            .map(|&(id, on)| {
                let run = if on {
                    RunState::Running
                } else {
                    RunState::Ended { ok: true }
                };
                (id.to_string(), run)
            })
            .collect(),
        known: true,
    }));
}

#[test]
fn another_session_that_needs_you_is_a_child() {
    let mut app = common::attached_app(false, 60, 16);
    let mut r = Reporter::new(true);
    frame(&mut r, &app);
    let mut rows = common::rows();
    rows[1].needs_you = true;
    poll(&mut app, rows, &[]);
    let out = frame(&mut r, &app);
    assert!(
        out.contains(&format!(
            "\x1b]7501;state=blocked:id=sessions/s-2:kind=question:title={}:msg={}\x1b\\",
            b64("Fix the flaky test"),
            b64("Waiting for you")
        )),
        "{out:?}"
    );
    // The first poll is not a change: no alert.
    assert!(!out.contains('\x07'));
}

#[test]
fn another_session_that_finishes_is_done_and_alerts_until_opened() {
    let mut app = common::attached_app(false, 60, 16);
    let mut r = Reporter::new(true);
    let mut rows = common::rows();
    rows[1].status = "active".into();
    poll(&mut app, rows.clone(), &[("s-2", true)]);
    assert!(frame(&mut r, &app).contains("state=working:id=sessions/s-2"));
    app.update(Msg::Focus(false));
    poll(&mut app, rows, &[("s-2", false)]);
    let out = frame(&mut r, &app);
    assert!(out.contains("state=done:id=sessions/s-2"), "{out:?}");
    assert!(out.contains("\x1b]9;Fix the flaky test: done\x07"));
    // Opening it is seeing it.
    app.attach("s-2");
    assert!(frame(&mut r, &app).contains("state=clear:id=sessions/s-2"));
}

#[test]
fn a_poll_without_session_state_changes_nothing() {
    let mut app = common::attached_app(false, 60, 16);
    let mut r = Reporter::new(true);
    let mut rows = common::rows();
    rows[1].status = "active".into();
    poll(&mut app, rows.clone(), &[("s-2", true)]);
    frame(&mut r, &app);
    // The control plane did not answer: the session reads as gone.
    rows[1].status = String::new();
    app.update(Msg::Net(NetMsg::Sessions(Ok(rows))));
    app.update(Msg::Net(NetMsg::SessionsRunning {
        running: Vec::new(),
        known: false,
    }));
    assert_eq!(frame(&mut r, &app), "", "no done, no error, no alert");
}

#[test]
fn a_poll_runs_only_when_something_shows_it() {
    let mut app = common::attached_app(false, 60, 16);
    app.poll_sessions();
    assert!(
        app.cmds.is_empty(),
        "attached, no terminal records: no poll"
    );
    app.program_status = true;
    app.poll_sessions();
    assert_eq!(app.cmds, [Cmd::LoadSessions]);
    app.cmds.clear();
    // One in flight at a time.
    app.poll_sessions();
    assert!(app.cmds.is_empty());
    poll(&mut app, common::rows(), &[]);
    app.poll_sessions();
    assert_eq!(app.cmds, [Cmd::LoadSessions]);
}

#[test]
fn a_refresh_keeps_the_selected_session() {
    let mut app = App::new(false, 60, 16);
    poll(&mut app, common::rows(), &[]);
    app.update(Msg::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
    assert_eq!(app.sessions.selected, 1);
    // s-2 moves to the top: the cursor follows it.
    let mut rows = common::rows();
    rows.swap(0, 1);
    poll(&mut app, rows, &[]);
    assert_eq!(app.sessions.selected, 0);
}

#[test]
fn without_support_no_child_is_sent() {
    let mut app = common::attached_app(false, 60, 16);
    let mut r = Reporter::new(false);
    live(
        &mut app,
        9,
        "status_changed",
        r#"{"from":"active","to":"evacuating"}"#,
    );
    assert!(!frame(&mut r, &app).contains("7501"));
}
