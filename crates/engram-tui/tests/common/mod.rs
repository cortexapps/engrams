//! Shared fixtures: a scripted session and the frame helpers.

#![allow(dead_code)]

use engram_tui::app::{App, Msg, NetMsg};
use engram_tui::event::Envelope;
use engram_tui::sessions::SessionRow;

pub fn env(idx: i64, kind: &str, payload: &str) -> Envelope {
    Envelope {
        idx: Some(idx),
        kind: kind.to_string(),
        payload_json: payload.to_string(),
    }
}

/// A short conversation: a prompt, a reply with a tool call, a resume.
pub fn scripted() -> Vec<Envelope> {
    vec![
        env(0, "status_changed", r#"{"from":"created","to":"active"}"#),
        env(
            1,
            "agent_message",
            r#"{"run_id":"","message_id":"u1","role":"user","text":"Add a README to the repo.","prompt_id":"p1"}"#,
        ),
        env(
            2,
            "run_started",
            r#"{"run_id":"r1","prompt_id":"p1","at":"2026-01-01T10:00:00Z"}"#,
        ),
        env(
            3,
            "agent_message",
            r#"{"run_id":"r1","message_id":"m1","role":"assistant","text":"I will look at the layout first."}"#,
        ),
        env(
            4,
            "tool_call_started",
            r#"{"run_id":"r1","tool_call_id":"t1","tool_name":"Bash","args_summary":"ls -la"}"#,
        ),
        env(
            5,
            "tool_call_completed",
            r#"{"run_id":"r1","tool_call_id":"t1","tool_name":"Bash","ok":true,"duration_ms":12,"result_summary":"Cargo.toml\nsrc\ntests\ndocs\nREADME.md\nLICENSE\njustfile"}"#,
        ),
        env(
            6,
            "agent_message",
            r#"{"run_id":"r1","message_id":"m2","role":"assistant","text":"There is already a README.md. I will extend it with a usage section and keep the existing introduction intact."}"#,
        ),
        env(
            7,
            "run_completed",
            r#"{"run_id":"r1","ok":true,"at":"2026-01-01T10:01:05Z"}"#,
        ),
        env(8, "resumed", r#"{}"#),
    ]
}

pub fn rows() -> Vec<SessionRow> {
    vec![
        SessionRow {
            session_id: "s-1".into(),
            title: "Add a README".into(),
            status: "active".into(),
            age_secs: Some(90),
            needs_you: false,
            running: None,
        },
        SessionRow {
            session_id: "s-2".into(),
            title: "Fix the flaky test".into(),
            status: "idle".into(),
            age_secs: Some(7200),
            needs_you: false,
            running: None,
        },
        SessionRow {
            session_id: "s-3".into(),
            title: "Migrate the CI".into(),
            status: "parked".into(),
            age_secs: Some(200_000),
            needs_you: false,
            running: None,
        },
    ]
}

/// An app attached to `s-1` with the scripted history loaded.
pub fn attached_app(inline: bool, w: u16, h: u16) -> App {
    let mut app = App::new(inline, w, h);
    app.update(Msg::Net(NetMsg::Sessions(Ok(rows()))));
    app.attach("s-1");
    app.cmds.clear();
    app.update(Msg::Net(NetMsg::Status {
        session_id: "s-1".into(),
        status: "active".into(),
        title: None,
    }));
    app.update(Msg::Net(NetMsg::History {
        session_id: "s-1".into(),
        events: scripted(),
        older: false,
    }));
    app
}
