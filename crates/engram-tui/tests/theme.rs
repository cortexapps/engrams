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

// ---- Adaptive colours (an `OSC 11` reply) -------------------------------

use engram_tui::palette::{contrast, is_light, Palette, Rgb, DARK, LIGHT, MIN_TEXT_CONTRAST};
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;

/// The brand grounds from `web/src/index.css`: lab paper and green-black.
const PAPER: Rgb = (0xf0, 0xf5, 0xec);
const GREEN_BLACK: Rgb = (0x0b, 0x1d, 0x1e);

#[test]
fn brand_tokens_read_on_their_own_ground_unchanged() {
    let light = Palette::resolve(PAPER);
    assert_eq!(light.primary, LIGHT.primary);
    assert_eq!(light.terminal, LIGHT.terminal);
    assert_eq!(light.nominal, LIGHT.nominal);
    assert_eq!(light.caution, LIGHT.caution);
    assert_eq!(light.critical, LIGHT.critical);
    let dark = Palette::resolve(GREEN_BLACK);
    assert_eq!(dark.primary, DARK.primary);
    assert_eq!(dark.terminal, DARK.terminal);
    assert_eq!(dark.nominal, DARK.nominal);
    assert_eq!(dark.caution, DARK.caution);
    assert_eq!(dark.critical, DARK.critical);
    // Pure white and black, the common terminal defaults, keep them too.
    assert_eq!(Palette::resolve((255, 255, 255)).roles(), light.roles());
    assert_eq!(Palette::resolve((0, 0, 0)).roles(), dark.roles());
}

#[test]
fn ground_picks_the_token_set() {
    assert!(is_light((0xfd, 0xf6, 0xe3)), "Solarized light");
    assert!(!is_light((0x00, 0x2b, 0x36)), "Solarized dark");
    assert!(!is_light((0x28, 0x2c, 0x34)), "One dark");
    // Lime is a dark-ground token; verdigris is the light primary.
    assert_eq!(Palette::resolve((0xfd, 0xf6, 0xe3)).primary, LIGHT.primary);
    assert_eq!(Palette::resolve((0x28, 0x2c, 0x34)).primary, DARK.primary);
}

#[test]
fn a_token_short_of_contrast_is_walked_not_dropped() {
    // Lime on a light grey: the dark-ground token would be unreadable
    // on a ground that is only just dark.
    let bg = (0x70, 0x70, 0x70);
    assert!(!is_light(bg));
    let p = Palette::resolve(bg);
    assert_ne!(p.primary, DARK.primary);
    assert!(contrast(p.primary, bg) >= MIN_TEXT_CONTRAST);
    // It moved toward white, the better endpoint, and kept its hue order
    // (green stays the strongest channel).
    assert!(p.primary.1 >= p.primary.0 && p.primary.1 >= p.primary.2);
}

#[test]
fn adapt_sets_every_picked_colour_and_keeps_dim_plain() {
    let th = Theme::new(true).adapt(PAPER);
    let p = Palette::resolve(PAPER);
    let rgb = |(r, g, b): Rgb| Some(Color::Rgb(r, g, b));
    assert_eq!(th.code.fg, rgb(p.terminal));
    assert_eq!(th.tool.fg, rgb(p.terminal));
    assert_eq!(th.link.fg, rgb(p.terminal));
    assert_eq!(th.heading.fg, rgb(p.primary));
    assert_eq!(th.accent.fg, rgb(p.primary));
    assert_eq!(th.add.fg, rgb(p.nominal));
    assert_eq!(th.del.fg, rgb(p.critical));
    assert_eq!(th.err.fg, rgb(p.critical));
    assert_eq!(th.warn.fg, rgb(p.caution));
    assert_eq!(th.band.bg, rgb(p.band));
    assert_eq!(th.band.fg, None, "the band keeps the user's foreground");
    // Modifiers survive the recolour.
    assert!(th.link.add_modifier.contains(Modifier::UNDERLINED));
    assert!(th.heading.add_modifier.contains(Modifier::BOLD));
    assert!(th.accent.add_modifier.contains(Modifier::BOLD));
    // De-emphasis is still the attribute alone.
    assert_eq!(th.dim.fg, None);
    assert_eq!(th.pending.fg, None);
    // No colour at all under `NO_COLOR`, whatever the terminal said.
    let plain = Theme::new(false).adapt(PAPER);
    for st in [
        plain.code,
        plain.tool,
        plain.link,
        plain.heading,
        plain.accent,
        plain.band,
    ] {
        assert_eq!(st.fg, None);
        assert_eq!(st.bg, None);
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: Some(Box::new(FileFailurePersistence::WithSource("proptest-regressions"))),
        cases: 1024,
        ..ProptestConfig::default()
    })]

    /// Whatever the terminal's background, every colour the TUI picks is
    /// AA text on it, and the band stays close to the background.
    #[test]
    fn every_role_reads_on_any_background(r in any::<u8>(), g in any::<u8>(), b in any::<u8>()) {
        let bg = (r, g, b);
        let p = Palette::resolve(bg);
        for c in p.roles() {
            let ratio = contrast(c, bg);
            prop_assert!(ratio >= MIN_TEXT_CONTRAST, "{c:?} on {bg:?} is {ratio:.2}:1");
        }
        prop_assert!(contrast(p.band, bg) < 1.6, "the band {:?} is far from {bg:?}", p.band);
    }
}
