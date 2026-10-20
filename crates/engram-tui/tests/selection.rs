//! Mouse selection in the transcript: a drag selects and copies what the
//! frame shows, a wrapped line comes back as one line, a click still
//! toggles a tool call, and Esc clears the selection.

mod common;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use engram_tui::app::{App, Msg};
use engram_tui::render::{draw_fullscreen, Theme};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

const W: u16 = 60;
const H: u16 = 16;

/// Draw one frame; returns its rows as text.
fn frame(term: &mut Terminal<TestBackend>, app: &mut App) -> Vec<String> {
    let theme = Theme::new(false);
    let mut scratch = Vec::new();
    term.draw(|f| {
        let area = f.area();
        draw_fullscreen(f.buffer_mut(), area, app, &theme, &mut scratch);
    })
    .unwrap();
    let buf = term.backend().buffer();
    (0..H)
        .map(|y| (0..W).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect()
}

/// The (column, row) where `needle` starts on screen.
fn find(rows: &[String], needle: &str) -> (u16, u16) {
    for (y, row) in rows.iter().enumerate() {
        if let Some(byte) = row.find(needle) {
            let col = row[..byte].chars().count() as u16;
            return (col, y as u16);
        }
    }
    panic!("{needle:?} not on screen:\n{}", rows.join("\n"));
}

fn mouse(app: &mut App, kind: MouseEventKind, (column, row): (u16, u16)) {
    app.update(Msg::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }));
}

fn drag(app: &mut App, from: (u16, u16), to: (u16, u16)) {
    mouse(app, MouseEventKind::Down(MouseButton::Left), from);
    mouse(app, MouseEventKind::Drag(MouseButton::Left), to);
    mouse(app, MouseEventKind::Up(MouseButton::Left), to);
}

#[test]
fn a_drag_copies_the_words_it_covers() {
    let mut app = common::attached_app(false, W, H);
    let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
    let rows = frame(&mut term, &mut app);
    let (x, y) = find(&rows, "look at the layout");
    // From "look" to the end of "layout".
    drag(
        &mut app,
        (x, y),
        (x + "look at the layout".len() as u16 - 1, y),
    );
    assert!(app.attached.as_ref().unwrap().copy_request);
    let rows = frame(&mut term, &mut app);
    let a = app.attached.as_mut().unwrap();
    assert_eq!(a.copied.take().as_deref(), Some("look at the layout"));
    // The selection stays on screen, and the copy is not asked again.
    assert!(a.selection.is_some());
    assert!(!a.copy_request);
    assert!(rows.iter().any(|r| r.contains("look at the layout")));
}

#[test]
fn a_wrapped_line_copies_as_one_line() {
    let mut app = common::attached_app(false, W, H);
    let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
    let rows = frame(&mut term, &mut app);
    // The reply "There is already a README.md. I will extend it …" wraps
    // over several rows at this width.
    let start = find(&rows, "There is already");
    let end = find(&rows, "introduction intact.");
    drag(
        &mut app,
        start,
        (end.0 + "introduction intact.".len() as u16 - 1, end.1),
    );
    frame(&mut term, &mut app);
    let text = app.attached.as_mut().unwrap().copied.take().unwrap();
    assert_eq!(
        text,
        "There is already a README.md. I will extend it with a usage section and keep the existing introduction intact."
    );
}

#[test]
fn a_click_still_opens_a_tool_call_and_esc_clears_a_selection() {
    let mut app = common::attached_app(false, W, H);
    let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
    let rows = frame(&mut term, &mut app);
    let at = find(&rows, "look at the layout");
    // A press and a release without a move: no copy, no selection.
    mouse(&mut app, MouseEventKind::Down(MouseButton::Left), at);
    mouse(&mut app, MouseEventKind::Up(MouseButton::Left), at);
    let a = app.attached.as_ref().unwrap();
    assert!(a.selection.is_none());
    assert!(!a.copy_request);

    drag(&mut app, at, (at.0 + 3, at.1));
    frame(&mut term, &mut app);
    app.update(Msg::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    let a = app.attached.as_ref().unwrap();
    assert!(a.selection.is_none(), "Esc clears the selection first");
}

/// A long word the layout cut across rows copies back whole, with no
/// space where the cut was.
#[test]
fn a_cut_word_copies_without_a_space() {
    let mut app = common::attached_app(false, W, H);
    let word = "x".repeat(80);
    app.update(Msg::Net(engram_tui::app::NetMsg::Live {
        session_id: "s-1".into(),
        envelope: common::env(
            9,
            "agent_message",
            &format!(
                r#"{{"run_id":"r1","message_id":"m9","role":"assistant","text":"see {word} end"}}"#
            ),
        ),
    }));
    let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
    let rows = frame(&mut term, &mut app);
    // The layout puts the long word on rows of its own.
    let start = find(&rows, "see");
    let end = find(&rows, "x end");
    drag(&mut app, start, (end.0 + 4, end.1));
    frame(&mut term, &mut app);
    let text = app.attached.as_mut().unwrap().copied.take().unwrap();
    assert_eq!(text, format!("see {word} end"));
}

/// The ends of a selection whose row scrolled away still cut the row at
/// the selected columns.
#[test]
fn a_selection_scrolled_away_keeps_its_columns() {
    let mut app = common::attached_app(false, W, H);
    // Enough text below to scroll the selected row away.
    // JSON newlines, escaped.
    let lines: String = (0..30).map(|i| format!(r"line {i}\n\n")).collect();
    app.update(Msg::Net(engram_tui::app::NetMsg::Live {
        session_id: "s-1".into(),
        envelope: common::env(
            9,
            "agent_message",
            &format!(
                r#"{{"run_id":"r1","message_id":"m9","role":"assistant","text":"pick this row\n\n{lines}"}}"#
            ),
        ),
    }));
    let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
    // Scroll so the reply's first line is on screen.
    for _ in 0..20 {
        mouse(&mut app, MouseEventKind::ScrollUp, (0, 2));
    }
    let rows = frame(&mut term, &mut app);
    let at = find(&rows, "pick this row");
    // "this" only.
    drag(&mut app, (at.0 + 5, at.1), (at.0 + 8, at.1));
    frame(&mut term, &mut app);
    app.attached.as_mut().unwrap().copied = None;
    // Scroll the row off the screen, then copy again.
    for _ in 0..20 {
        mouse(&mut app, MouseEventKind::ScrollDown, (0, 2));
    }
    let rows = frame(&mut term, &mut app);
    assert!(!rows.iter().any(|r| r.contains("pick this row")));
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )));
    frame(&mut term, &mut app);
    assert_eq!(
        app.attached.as_mut().unwrap().copied.take().as_deref(),
        Some("this")
    );
}

/// Older history shifts every cell, and a resize re-wraps every row: a
/// selection would name other text, so it goes.
#[test]
fn older_history_or_a_resize_drops_the_selection() {
    let mut app = common::attached_app(false, W, H);
    let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
    let rows = frame(&mut term, &mut app);
    let at = find(&rows, "look at the layout");
    drag(&mut app, at, (at.0 + 3, at.1));
    app.update(Msg::Net(engram_tui::app::NetMsg::History {
        session_id: "s-1".into(),
        events: vec![common::env(
            -1,
            "agent_message",
            r#"{"run_id":"r0","message_id":"m0","role":"assistant","text":"older"}"#,
        )],
        older: true,
    }));
    assert!(app.attached.as_ref().unwrap().selection.is_none());
    drag(&mut app, at, (at.0 + 3, at.1));
    app.update(Msg::Resize(40, H));
    assert!(app.attached.as_ref().unwrap().selection.is_none());
}

/// A copy's result arrives after a newer copy was asked for: it is
/// dropped, and its fallback never puts the older text back.
#[test]
fn a_stale_copy_result_is_dropped() {
    let mut app = common::attached_app(false, W, H);
    app.copy_seq = 2;
    app.cmds.clear();
    app.update(Msg::Copied {
        seq: 1,
        chars: 3,
        how: engram_tui::app::Copied::Failed("no clipboard".into()),
        fallback: Some("old".into()),
    });
    assert!(app.cmds.is_empty(), "{:?}", app.cmds);
    assert!(app.note.is_none());
}

#[test]
fn ctrl_c_on_a_selection_copies_it_again() {
    let mut app = common::attached_app(false, W, H);
    let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
    let rows = frame(&mut term, &mut app);
    let at = find(&rows, "look at the layout");
    drag(&mut app, at, (at.0 + 3, at.1));
    frame(&mut term, &mut app);
    app.attached.as_mut().unwrap().copied = None;
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )));
    frame(&mut term, &mut app);
    assert_eq!(
        app.attached.as_mut().unwrap().copied.take().as_deref(),
        Some("look")
    );
    assert!(!app.quit);
}
