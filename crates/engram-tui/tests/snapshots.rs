//! Fullscreen frames pinned with `insta` on ratatui's `TestBackend`.

mod common;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use engram_tui::app::{App, Msg, NetMsg};
use engram_tui::render::{draw_fullscreen, Theme};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

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
fn session_list_80x12() {
    let mut app = App::new(false, 80, 12);
    app.update(Msg::Net(NetMsg::Sessions(Ok(common::rows()))));
    insta::assert_snapshot!(frame(&mut app, 80, 12));
}

#[test]
fn attached_transcript_80x24() {
    let mut app = common::attached_app(false, 80, 24);
    insta::assert_snapshot!(frame(&mut app, 80, 24));
}

#[test]
fn attached_streaming_with_a_draft_and_queue_60x16() {
    let mut app = common::attached_app(false, 60, 16);
    for e in [
        common::env(9, "run_started", r#"{"run_id":"r2","prompt_id":"p2"}"#),
        common::env(
            10,
            "agent_message",
            r#"{"run_id":"","message_id":"u2","role":"user","text":"Now add a usage section.","prompt_id":"p2"}"#,
        ),
        common::env(
            11,
            "prompt_queued",
            r#"{"prompt_id":"p3","summary":"And a license header."}"#,
        ),
    ] {
        app.update(Msg::Net(NetMsg::Live {
            session_id: "s-1".into(),
            envelope: e,
        }));
    }
    app.update(Msg::Net(NetMsg::Live {
        session_id: "s-1".into(),
        envelope: engram_tui::event::Envelope {
            idx: None,
            kind: "agent_message_chunk".into(),
            payload_json: r#"{"run_id":"r2","message_id":"m3","chunk":"Writing the usage sec"}"#
                .into(),
        },
    }));
    for c in "draft".chars() {
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )));
    }
    let shot = frame(&mut app, 60, 16);
    // The elapsed counter is wall-clock; blank it for the snapshot.
    let shot = shot
        .lines()
        .map(|l| {
            if l.contains("working") {
                "<footer: working>".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(shot);
}

#[test]
fn scrolled_up_shows_the_new_activity_jump_40x10() {
    let mut app = common::attached_app(false, 40, 10);
    frame(&mut app, 40, 10);
    app.update(Msg::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)));
    app.update(Msg::Net(NetMsg::Live {
        session_id: "s-1".into(),
        envelope: common::env(9, "resumed", "{}"),
    }));
    insta::assert_snapshot!(frame(&mut app, 40, 10));
}

#[test]
fn help_overlay_80x20() {
    let mut app = common::attached_app(false, 80, 20);
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('?'),
        KeyModifiers::NONE,
    )));
    insta::assert_snapshot!(frame(&mut app, 80, 20));
}
