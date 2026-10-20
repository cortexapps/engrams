//! Syntax highlighting as drawn: a fenced code block in a reply and a
//! file-change diff get colors from the terminal palette on their first
//! layout, and nothing without color.

mod common;

use engram_tui::app::{App, Msg, NetMsg};
use engram_tui::markdown::Markdown;
use engram_tui::render::{draw_fullscreen, Theme};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;
use ratatui::Terminal;

const W: u16 = 70;
const H: u16 = 20;

fn draw(app: &mut App, theme: &Theme) -> Buffer {
    let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
    let mut scratch = Vec::new();
    term.draw(|f| {
        let area = f.area();
        draw_fullscreen(f.buffer_mut(), area, app, theme, &mut scratch);
    })
    .unwrap();
    term.backend().buffer().clone()
}

/// The foreground of the first cell of `needle` on screen.
fn fg_of(buf: &Buffer, needle: &str) -> Color {
    for y in 0..H {
        let row: String = (0..W).map(|x| buf[(x, y)].symbol()).collect();
        if let Some(byte) = row.find(needle) {
            let x = row[..byte].chars().count() as u16;
            return buf[(x, y)].fg;
        }
    }
    panic!("{needle:?} not on screen");
}

fn live(app: &mut App, idx: i64, kind: &str, payload: &str) {
    app.update(Msg::Net(NetMsg::Live {
        session_id: "s-1".into(),
        envelope: common::env(idx, kind, payload),
    }));
}

#[test]
fn a_code_block_is_highlighted_on_first_layout() {
    let mut md = Markdown::finalized("Try this:\n\n```rust\nlet n = 42; // the answer\n```\n");
    // Plain until laid out: no tone yet.
    assert!(md.runs().iter().all(|r| r.tone == 0));
    md.highlight();
    let text = md.text().to_string();
    let toned: Vec<(&str, u8)> = md
        .runs()
        .iter()
        .filter(|r| r.tone != 0)
        .map(|r| (&text[r.start as usize..r.end as usize], r.tone))
        .collect();
    assert!(toned.iter().any(|(s, _)| *s == "let"), "{toned:?}");
    assert!(toned.iter().any(|(s, _)| s.contains("42")), "{toned:?}");
    assert!(
        toned.iter().any(|(s, _)| s.contains("the answer")),
        "{toned:?}"
    );
    // Runs stay sorted and apart.
    for w in md.runs().windows(2) {
        assert!(w[0].end <= w[1].start, "{:?}", md.runs());
    }
    // Again: nothing changes.
    let before = md.runs().to_vec();
    md.highlight();
    assert_eq!(md.runs(), &before[..]);
}

#[test]
fn a_reply_draws_its_code_in_palette_colors() {
    let mut app = common::attached_app(false, W, H);
    live(
        &mut app,
        9,
        "run_started",
        r#"{"run_id":"r2","prompt_id":"p2"}"#,
    );
    live(
        &mut app,
        10,
        "agent_message",
        r#"{"run_id":"r2","message_id":"m9","role":"assistant","text":"```python\ndef greet():\n    return \"hi\"\n```"}"#,
    );
    let buf = draw(&mut app, &Theme::new(true));
    assert_eq!(fg_of(&buf, "def"), Color::Magenta);
    assert_eq!(fg_of(&buf, "\"hi\""), Color::Green);
    // Without color, nothing is colored.
    let plain = draw(&mut app, &Theme::new(false));
    assert_eq!(fg_of(&plain, "def"), Color::Reset);
}

#[test]
fn a_diff_is_highlighted_with_its_sign_kept() {
    let mut app = common::attached_app(false, W, H);
    live(
        &mut app,
        9,
        "run_started",
        r#"{"run_id":"r2","prompt_id":"p2"}"#,
    );
    live(
        &mut app,
        10,
        "tool_call_started",
        r#"{"run_id":"r2","tool_call_id":"e1","tool_name":"Edit","args_summary":"{\"file_path\":\"src/lib.rs\"}"}"#,
    );
    live(
        &mut app,
        11,
        "file_changed",
        r#"{"run_id":"r2","tool_call_id":"e1","path":"src/lib.rs","change":{"edit":{"hunks":[{"old":"let a = 1;","new":"let a = 2;"}]}},"at":"2026-01-01T00:00:00Z"}"#,
    );
    let mut theme = Theme::new(true);
    theme.diff_bg = Some(engram_tui::render::diff_backgrounds((0x1a, 0x1b, 0x26)));
    let buf = draw(&mut app, &theme);
    // The keyword after the sign is toned; the sign keeps its color.
    assert_eq!(fg_of(&buf, "+let"), Color::Green);
    assert_eq!(fg_of(&buf, "let a = 2"), Color::Magenta);
    // An added line sits on the faint green background.
    let (x, y) = (0..H)
        .find_map(|y| {
            let row: String = (0..W).map(|x| buf[(x, y)].symbol()).collect();
            row.find("+let")
                .map(|b| (row[..b].chars().count() as u16, y))
        })
        .expect("the added line");
    let bg = format!("{:?}", buf[(x + 1, y)].bg);
    assert!(bg.starts_with("Rgb("), "{bg}");
}
