//! Native scrollback mode (ADR 0124 D1): finalized cells are inserted
//! above the inline viewport exactly once and land in the terminal's
//! scrollback; the viewport keeps only the live cells, the composer and
//! the footer. `TestBackend` models the scrollback so the assertion is
//! exact.

mod common;

use engram_tui::app::{App, Msg, NetMsg};
use engram_tui::render::{draw_inline, paint_cell, Theme};
use engram_tui::transcript::Cell;
use ratatui::backend::TestBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};

const VIEW: u16 = 6;

/// Commit what is committable, then draw. Mirrors the runtime's `draw`.
fn draw(terminal: &mut Terminal<TestBackend>, app: &mut App, theme: &Theme) -> usize {
    let mut committed = 0;
    if let Some(a) = &mut app.attached {
        loop {
            let t = &mut a.reducer.transcript;
            let width = terminal.size().unwrap().width;
            let Some((_, e)) = t.next_committable() else {
                break;
            };
            let h = e.height(width.saturating_sub(2).max(1));
            if h > 0 {
                terminal
                    .insert_before(h, |buf| paint_cell(buf, e, theme))
                    .unwrap();
            }
            t.release();
            committed += 1;
        }
    }
    let mut scratch = Vec::new();
    terminal
        .draw(|f| {
            let area = f.area();
            draw_inline(f.buffer_mut(), area, app, theme, &mut scratch);
        })
        .unwrap();
    committed
}

fn text_of(buf: &ratatui::buffer::Buffer) -> Vec<String> {
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

#[test]
fn finalized_cells_land_in_scrollback_once_and_leave_memory() {
    let backend = TestBackend::new(40, 12);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(VIEW),
        },
    )
    .unwrap();
    let theme = Theme::new(false);
    let mut app = common::attached_app(true, 40, 12);

    let committed = draw(&mut terminal, &mut app, &theme);
    // Every scripted cell is final after `run_completed` + the snapshot
    // marker: the prompt, two replies, the tool row and the marker.
    assert_eq!(committed, 5);
    let a = app.attached.as_ref().unwrap();
    assert!(
        a.reducer.transcript.is_empty(),
        "committed cells are released"
    );
    assert_eq!(a.reducer.transcript.len(), 5, "indices stay absolute");

    // The screen above the viewport plus the scrollback hold the cells in
    // order; nothing is duplicated.
    let screen = text_of(terminal.backend().buffer());
    let mut all = text_of(terminal.backend().scrollback());
    all.extend(screen.iter().cloned());
    let joined = all.join("\n");
    assert_eq!(joined.matches("Add a README to the repo.").count(), 1);
    assert_eq!(joined.matches("✓ Bash ls -la").count(), 1);
    assert_eq!(joined.matches("· snapshot taken").count(), 1);
    let prompt_at = all.iter().position(|l| l.contains("Add a README")).unwrap();
    let tool_at = all.iter().position(|l| l.contains("Bash ls -la")).unwrap();
    let marker_at = all
        .iter()
        .position(|l| l.contains("snapshot taken"))
        .unwrap();
    assert!(prompt_at < tool_at && tool_at < marker_at);

    // A second draw with nothing new inserts nothing.
    assert_eq!(draw(&mut terminal, &mut app, &theme), 0);
    let again = text_of(terminal.backend().scrollback());
    assert_eq!(again, text_of(terminal.backend().scrollback()));

    // The viewport holds the composer and footer, not the history.
    let view = &screen[screen.len() - VIEW as usize..];
    let view = view.join("\n");
    assert!(view.contains("message the agent"), "{view}");
    assert!(!view.contains("Add a README"), "{view}");
}

#[test]
fn a_streaming_cell_stays_in_the_viewport_until_final() {
    let backend = TestBackend::new(40, 12);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(VIEW),
        },
    )
    .unwrap();
    let theme = Theme::new(false);
    let mut app = common::attached_app(true, 40, 12);
    draw(&mut terminal, &mut app, &theme);
    let live = |kind: &str, payload: &str| {
        Msg::Net(NetMsg::Live {
            session_id: "s-1".into(),
            envelope: engram_tui::event::Envelope {
                idx: if kind == "agent_message_chunk" {
                    None
                } else {
                    Some(99)
                },
                kind: kind.into(),
                payload_json: payload.into(),
            },
        })
    };
    app.update(live("run_started", r#"{"run_id":"r2"}"#));
    app.update(live(
        "agent_message_chunk",
        r#"{"run_id":"r2","message_id":"m9","chunk":"partial words"}"#,
    ));
    assert_eq!(
        draw(&mut terminal, &mut app, &theme),
        0,
        "a streaming cell is not final"
    );
    let screen = text_of(terminal.backend().buffer());
    let view = screen[screen.len() - VIEW as usize..].join("\n");
    assert!(view.contains("partial words"), "{view}");
    let a = app.attached.as_ref().unwrap();
    assert!(matches!(
        a.reducer.transcript.get(5).map(|e| &e.cell),
        Some(Cell::Assistant {
            streaming: true,
            ..
        })
    ));

    app.update(Msg::Net(NetMsg::Live {
        session_id: "s-1".into(),
        envelope: common::env(
            100,
            "agent_message",
            r#"{"run_id":"r2","message_id":"m9","role":"assistant","text":"partial words, finished."}"#,
        ),
    }));
    app.update(Msg::Net(NetMsg::Live {
        session_id: "s-1".into(),
        envelope: common::env(101, "run_completed", r#"{"run_id":"r2","ok":true}"#),
    }));
    assert_eq!(draw(&mut terminal, &mut app, &theme), 1);
    let screen = text_of(terminal.backend().buffer());
    let mut all = text_of(terminal.backend().scrollback());
    all.extend(screen);
    let joined = all.join("\n");
    assert_eq!(joined.matches("partial words, finished.").count(), 1);
}

#[test]
fn a_resize_relayouts_without_duplicating_committed_cells() {
    let backend = TestBackend::new(40, 12);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(VIEW),
        },
    )
    .unwrap();
    let theme = Theme::new(false);
    let mut app = common::attached_app(true, 40, 12);
    draw(&mut terminal, &mut app, &theme);
    terminal.backend_mut().resize(30, 12);
    app.update(Msg::Resize(30, 12));
    assert_eq!(draw(&mut terminal, &mut app, &theme), 0);
    // Wherever ratatui re-anchored the viewport, the composer is drawn
    // once and no committed cell was written again.
    let screen = text_of(terminal.backend().buffer());
    let mut all = text_of(terminal.backend().scrollback());
    all.extend(screen);
    let joined = all.join("\n");
    assert_eq!(joined.matches("message the agent").count(), 1, "{joined}");
    assert_eq!(joined.matches("Add a README to the").count(), 1, "{joined}");
}
