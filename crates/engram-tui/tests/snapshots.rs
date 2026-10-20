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

#[test]
fn markdown_reply_60x18() {
    let mut app = common::attached_app(false, 60, 18);
    app.update(Msg::Net(NetMsg::Live {
        session_id: "s-1".into(),
        envelope: common::env(9, "run_started", r#"{"run_id":"r2","prompt_id":"p2"}"#),
    }));
    app.update(Msg::Net(NetMsg::Live {
        session_id: "s-1".into(),
        envelope: common::env(
            10,
            "agent_message",
            r###"{"run_id":"r1","message_id":"m4","role":"assistant","text":"## Plan\n\nI will add a **usage** section with `engrams tui`:\n\n1. read the README\n2. append the section\n\n```sh\nengrams tui\n```\n\n> keep the intro\n\nSee [the docs](https://example.com/docs)."}"###,
        ),
    }));
    app.update(Msg::Net(NetMsg::Live {
        session_id: "s-1".into(),
        envelope: common::env(11, "run_completed", r#"{"run_id":"r2","ok":true}"#),
    }));
    insta::assert_snapshot!(frame(&mut app, 60, 18));
}

#[test]
fn file_change_diff_60x14() {
    let mut app = common::attached_app(false, 60, 14);
    for e in [
        common::env(9, "run_started", r#"{"run_id":"r2","prompt_id":"p2"}"#),
        common::env(
            10,
            "tool_call_started",
            r#"{"run_id":"r2","tool_call_id":"t2","tool_name":"Edit","args_summary":"README.md"}"#,
        ),
        common::env(
            11,
            "file_changed",
            r###"{"run_id":"r2","tool_call_id":"t2","path":"README.md","change":{"edit":{"hunks":[{"old":"# engrams\n","new":"# engrams\n\n## Usage\n\nengrams tui\n"}]}}}"###,
        ),
        common::env(
            12,
            "tool_call_completed",
            r#"{"run_id":"r2","tool_call_id":"t2","tool_name":"Edit","ok":true,"duration_ms":3}"#,
        ),
    ] {
        app.update(Msg::Net(NetMsg::Live {
            session_id: "s-1".into(),
            envelope: e,
        }));
    }
    let shot = frame(&mut app, 60, 14);
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
fn search_jumps_to_the_hit_60x12() {
    let mut app = common::attached_app(false, 60, 12);
    frame(&mut app, 60, 12);
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('t'),
        KeyModifiers::CONTROL,
    )));
    for c in "layout".chars() {
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )));
    }
    app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    let a = app.attached.as_ref().unwrap();
    assert_eq!(a.search.as_ref().unwrap().cursor, Some(1));
    insta::assert_snapshot!(frame(&mut app, 60, 12));
    // Esc closes the search and leaves the scroll where it is.
    app.update(Msg::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    assert!(app.attached.as_ref().unwrap().search.is_none());
}

#[test]
fn tool_rows_show_the_subject_of_each_call_70x16() {
    let mut app = common::attached_app(false, 70, 16);
    for e in [
        common::env(9, "run_started", r#"{"run_id":"r2","prompt_id":"p2"}"#),
        common::env(
            10,
            "tool_call_started",
            r#"{"run_id":"r2","tool_call_id":"t2","tool_name":"Grep","args_summary":"{\"pattern\":\"fn main\",\"path\":\"src\",\"output_mode\":\"content\"}"}"#,
        ),
        common::env(
            11,
            "tool_call_completed",
            r#"{"run_id":"r2","tool_call_id":"t2","tool_name":"Grep","ok":true,"duration_ms":3,"result_summary":"src/main.rs:1:fn main() {"}"#,
        ),
        common::env(
            12,
            "tool_call_started",
            r#"{"run_id":"r2","tool_call_id":"t3","tool_name":"Bash","args_summary":"{\"command\":\"cd /work && for f in a b c d e f g h i j k l m n o p; do\\n  echo \\\"=== $f\\\"\\n  cat $f.txt\\ndone\\necho done\\necho again\\necho more\",\"description\":\"Print the files\"}"}"#,
        ),
        common::env(
            13,
            "tool_call_completed",
            r#"{"run_id":"r2","tool_call_id":"t3","tool_name":"Bash","ok":false,"duration_ms":30,"result_summary":"cat: a.txt: No such file or directory"}"#,
        ),
    ] {
        app.update(Msg::Net(NetMsg::Live {
            session_id: "s-1".into(),
            envelope: e,
        }));
    }
    let shot = frame(&mut app, 70, 16);
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
fn a_table_refits_when_the_terminal_narrows() {
    let mut app = common::attached_app(false, 60, 14);
    for e in [
        common::env(9, "run_started", r#"{"run_id":"r2","prompt_id":"p2"}"#),
        common::env(
            10,
            "agent_message",
            r#"{"run_id":"r2","message_id":"m3","role":"assistant","text":"Budgets:\n\n| Metric | Budget | Note |\n|---|---|---|\n| RSS attached | 40 MB | with ten thousand events loaded |\n| binary | 8 MB | stripped, linux-musl |"}"#,
        ),
        common::env(11, "run_completed", r#"{"run_id":"r2","ok":true}"#),
    ] {
        app.update(Msg::Net(NetMsg::Live {
            session_id: "s-1".into(),
            envelope: e,
        }));
    }
    let wide = frame(&mut app, 60, 14);
    app.update(Msg::Resize(34, 14));
    let narrow = frame(&mut app, 34, 14);
    insta::assert_snapshot!(format!("{wide}\n{narrow}"));
}

#[test]
fn read_only_calls_fold_into_an_explored_row_60x12() {
    let mut app = common::attached_app(false, 60, 12);
    for e in [
        common::env(9, "run_started", r#"{"run_id":"r2","prompt_id":"p2"}"#),
        common::env(
            10,
            "tool_call_started",
            r#"{"run_id":"r2","tool_call_id":"t2","tool_name":"Read","args_summary":"{\"file_path\":\"src/main.rs\"}"}"#,
        ),
        common::env(
            11,
            "tool_call_completed",
            r#"{"run_id":"r2","tool_call_id":"t2","tool_name":"Read","ok":true}"#,
        ),
        common::env(
            12,
            "tool_call_started",
            r#"{"run_id":"r2","tool_call_id":"t3","tool_name":"Read","args_summary":"{\"file_path\":\"src/lib.rs\"}"}"#,
        ),
        common::env(
            13,
            "tool_call_completed",
            r#"{"run_id":"r2","tool_call_id":"t3","tool_name":"Read","ok":true}"#,
        ),
        common::env(
            14,
            "tool_call_started",
            r#"{"run_id":"r2","tool_call_id":"t4","tool_name":"Grep","args_summary":"{\"pattern\":\"fn main\",\"path\":\"src\"}"}"#,
        ),
    ] {
        app.update(Msg::Net(NetMsg::Live {
            session_id: "s-1".into(),
            envelope: e,
        }));
    }
    let shot = frame(&mut app, 60, 12);
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

/// The events of a turn with three calls, then its end.
fn folded_turn(app: &mut App) {
    for e in [
        common::env(
            9,
            "run_started",
            r#"{"run_id":"r2","prompt_id":"p2","at":"2026-01-01T00:00:00Z"}"#,
        ),
        common::env(
            10,
            "tool_call_started",
            r#"{"run_id":"r2","tool_call_id":"t2","tool_name":"Bash","args_summary":"{\"command\":\"cargo build\"}"}"#,
        ),
        common::env(
            11,
            "tool_call_completed",
            r#"{"run_id":"r2","tool_call_id":"t2","tool_name":"Bash","ok":true,"result_summary":"Finished"}"#,
        ),
        common::env(
            12,
            "tool_call_started",
            r#"{"run_id":"r2","tool_call_id":"t3","tool_name":"Bash","args_summary":"{\"command\":\"cargo test\"}"}"#,
        ),
        common::env(
            13,
            "tool_call_completed",
            r#"{"run_id":"r2","tool_call_id":"t3","tool_name":"Bash","ok":true,"result_summary":"ok"}"#,
        ),
        common::env(
            14,
            "tool_call_started",
            r#"{"run_id":"r2","tool_call_id":"t4","tool_name":"Read","args_summary":"{\"file_path\":\"src/lib.rs\"}"}"#,
        ),
        common::env(
            15,
            "tool_call_completed",
            r#"{"run_id":"r2","tool_call_id":"t4","tool_name":"Read","ok":true}"#,
        ),
        common::env(
            16,
            "agent_message",
            r#"{"run_id":"r2","message_id":"m3","role":"assistant","text":"Built and tested."}"#,
        ),
        common::env(
            17,
            "run_completed",
            r#"{"run_id":"r2","ok":true,"at":"2026-01-01T00:00:09Z"}"#,
        ),
    ] {
        app.update(Msg::Net(NetMsg::Live {
            session_id: "s-1".into(),
            envelope: e,
        }));
    }
}

#[test]
fn a_finished_turn_folds_and_opens_60x14() {
    let mut app = common::attached_app(false, 60, 14);
    folded_turn(&mut app);
    insta::assert_snapshot!("folded_60x14", frame(&mut app, 60, 14));
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('o'),
        KeyModifiers::CONTROL,
    )));
    insta::assert_snapshot!("fold_open_60x14", frame(&mut app, 60, 14));
}

#[test]
fn a_click_on_the_fold_row_opens_it() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let mut app = common::attached_app(false, 60, 14);
    folded_turn(&mut app);
    let shot = frame(&mut app, 60, 14);
    let y = shot
        .lines()
        .position(|l| l.contains("Ran 2 shell commands"))
        .expect("the fold row") as u16;
    app.update(Msg::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 3,
        row: y,
        modifiers: KeyModifiers::NONE,
    }));
    let shot = frame(&mut app, 60, 14);
    assert!(shot.contains("▾ Ran 2 shell commands"), "{shot}");
    assert!(shot.contains("cargo build"), "{shot}");
}

/// Draw once at `w`×`h`; returns whether the draw asked for older history.
fn draw_wants_older(app: &mut App, w: u16, h: u16) -> bool {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    let theme = Theme::new(false);
    let mut scratch = Vec::new();
    let mut want = false;
    terminal
        .draw(|f| {
            let area = f.area();
            want = draw_fullscreen(f.buffer_mut(), area, app, &theme, &mut scratch).1;
        })
        .unwrap();
    want
}

#[test]
fn a_scroll_past_the_top_paints_the_top() {
    let mut app = common::attached_app(false, 60, 12);
    app.attached.as_mut().unwrap().exhausted = true;
    // Far past the top of a short transcript: the frame shows the first
    // cell, the scroll is clamped so that page fills the area, and the
    // draw asks for older history once.
    for _ in 0..40 {
        app.update(Msg::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }));
    }
    assert!(draw_wants_older(&mut app, 60, 12));
    let shot = frame(&mut app, 60, 12);
    assert!(shot.contains("Add a README to the repo."), "{shot}");
    assert!(shot.contains("✓ Bash ls -la"), "{shot}");
    let up = app.attached.as_ref().unwrap().scroll_up;
    assert!(up > 0 && up < 12, "{up}");
    // A second frame at the clamped scroll paints the same rows and asks
    // for nothing; the next scroll up asks again.
    assert_eq!(frame(&mut app, 60, 12), shot);
    assert!(!draw_wants_older(&mut app, 60, 12));
    app.update(Msg::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)));
    assert!(draw_wants_older(&mut app, 60, 12));
    // A transcript that fits the screen clamps to zero, and a scroll up
    // still asks for older history.
    let mut app = common::attached_app(false, 60, 40);
    app.update(Msg::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)));
    assert!(draw_wants_older(&mut app, 60, 40));
    assert_eq!(app.attached.as_ref().unwrap().scroll_up, 0);
    assert!(!draw_wants_older(&mut app, 60, 40), "no scroll, no page");
}

#[test]
fn a_prompt_row_sits_on_a_band() {
    use ratatui::style::Color;
    let mut app = common::attached_app(false, 60, 30);
    let mut terminal = Terminal::new(TestBackend::new(60, 30)).unwrap();
    let theme = Theme::new(true);
    let mut scratch = Vec::new();
    terminal
        .draw(|f| {
            let area = f.area();
            draw_fullscreen(f.buffer_mut(), area, &mut app, &theme, &mut scratch);
        })
        .unwrap();
    let buf = terminal.backend().buffer();
    let row_of = |needle: &str| {
        (0..buf.area.height)
            .find(|&y| {
                let line: String = (0..buf.area.width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol())
                    .collect();
                line.contains(needle)
            })
            .expect(needle)
    };
    let y = row_of("Add a README to the repo.");
    // Edge to edge, past the end of the text.
    for x in [0u16, 10, 59] {
        assert_eq!(buf.cell((x, y)).unwrap().bg, Color::DarkGray, "x={x}");
    }
    let y = row_of("There is already");
    assert_eq!(buf.cell((2, y)).unwrap().bg, Color::Reset);
}
