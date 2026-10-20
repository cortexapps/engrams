//! De-emphasis is the terminal's DIM attribute on the user's own foreground,
//! never a fixed grey: a grey is close to the background in many themes, and
//! the content of a tool call is not de-emphasised at all.

mod common;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use engram_tui::app::{App, Msg};
use engram_tui::render::{draw_fullscreen, Theme};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};
use ratatui::Terminal;

#[test]
fn dim_styles_set_no_colour() {
    for color in [true, false] {
        let th = Theme::new(color);
        for (name, st) in [("dim", th.dim), ("pending", th.pending)] {
            assert_eq!(st.fg, None, "{name} sets a foreground (color={color})");
            assert_eq!(st.bg, None, "{name} sets a background (color={color})");
            assert!(
                st.add_modifier.contains(Modifier::DIM),
                "{name} is not DIM (color={color})"
            );
        }
    }
}

#[test]
fn without_dim_drops_the_attribute_everywhere() {
    let th = Theme::new(true).without_dim();
    assert!(!th.dim.add_modifier.contains(Modifier::DIM));
    assert!(!th.pending.add_modifier.contains(Modifier::DIM));
    assert_eq!(th.dim.fg, None);
}

fn frame(app: &mut App, theme: &Theme) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
    let mut scratch = Vec::new();
    terminal
        .draw(|f| {
            let area = f.area();
            draw_fullscreen(f.buffer_mut(), area, app, theme, &mut scratch);
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

/// The style of the first cell of `needle` in the buffer.
fn style_of(buf: &Buffer, needle: &str) -> Style {
    for y in 0..buf.area.height {
        let line: String = (0..buf.area.width)
            .map(|x| buf.cell((x, y)).unwrap().symbol())
            .collect();
        if let Some(col) = line.find(needle) {
            let x = line[..col].chars().count() as u16;
            return buf.cell((x, y)).unwrap().style();
        }
    }
    let screen: Vec<String> = (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf.cell((x, y)).unwrap().symbol())
                .collect()
        })
        .collect();
    panic!("{needle:?} is not on screen:\n{}", screen.join("\n"));
}

/// A painted cell reports `Reset` where a style set nothing.
fn plain_fg(st: Style) -> bool {
    matches!(st.fg, None | Some(Color::Reset))
}

#[test]
fn tool_content_is_plain_and_only_affordances_are_dim() {
    let theme = Theme::new(true);
    let mut app = common::attached_app(false, 60, 20);
    // Open the folded turn; the tool call keeps its collapsed preview.
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('o'),
        KeyModifiers::CONTROL,
    )));
    let buf = frame(&mut app, &theme);

    // The command and its result: the user's foreground, not dim.
    for content in ["ls -la", "Cargo.toml"] {
        let st = style_of(&buf, content);
        assert!(plain_fg(st), "{content:?} has a foreground: {:?}", st.fg);
        assert!(
            !st.add_modifier.contains(Modifier::DIM),
            "{content:?} is dim"
        );
    }
    // The gutter and the tail are dim, with no colour.
    for affordance in ["⎿", "more lines"] {
        let st = style_of(&buf, affordance);
        assert!(plain_fg(st), "{affordance:?} has a foreground: {:?}", st.fg);
        assert!(
            st.add_modifier.contains(Modifier::DIM),
            "{affordance:?} is not dim"
        );
    }
}
