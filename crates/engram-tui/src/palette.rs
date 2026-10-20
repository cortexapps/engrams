//! The colours the TUI picks itself, resolved against the terminal's own
//! background.
//!
//! Without a background the theme uses the terminal's ANSI palette, which
//! the user's theme defines. When the start-up probe gets an `OSC 11`
//! reply, the roles that the TUI colours (inline code, links, headings,
//! the accent, diff and status tones) take the engrams brand tokens for a
//! light or a dark ground, and every one of them is checked for WCAG AA
//! text contrast (4.5:1) against the real background. A token that falls
//! short is walked toward black or white, keeping as much of its hue as
//! the surface allows, as Codex does.

/// An sRGB colour, 8 bits a channel.
pub type Rgb = (u8, u8, u8);

/// WCAG AA contrast for normal text.
pub const MIN_TEXT_CONTRAST: f32 = 4.5;

/// The brand tokens for one ground. Values are the sRGB form of the
/// `oklch` tokens in `web/src/index.css`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Brand {
    /// `--primary`: the accent, headings.
    pub primary: Rgb,
    /// `--mark-terminal`: inline code, tool names, links.
    pub terminal: Rgb,
    /// `--instrument-nominal-ink`: added lines.
    pub nominal: Rgb,
    /// `--instrument-caution-ink`: warnings.
    pub caution: Rgb,
    /// `--instrument-critical-ink`: errors, removed lines.
    pub critical: Rgb,
}

/// Tokens for a light ground (`:root` in `index.css`).
pub const LIGHT: Brand = Brand {
    primary: (0x00, 0x69, 0x58),
    terminal: (0x3a, 0x6b, 0x5c),
    nominal: (0x0a, 0x7e, 0x3a),
    caution: (0x90, 0x53, 0x00),
    critical: (0xc2, 0x18, 0x1d),
};

/// Tokens for a dark ground (`.dark` in `index.css`).
pub const DARK: Brand = Brand {
    primary: (0xbd, 0xdf, 0x42),
    terminal: (0x5e, 0xb0, 0x9e),
    nominal: (0x5f, 0xd3, 0x7f),
    caution: (0xfc, 0xb9, 0x53),
    critical: (0xff, 0x71, 0x6b),
};

/// The resolved colours for one terminal background: brand tokens for
/// its ground, each readable on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub primary: Rgb,
    pub terminal: Rgb,
    pub nominal: Rgb,
    pub caution: Rgb,
    pub critical: Rgb,
    /// The fill behind a prompt: the background moved a little toward
    /// the foreground's side.
    pub band: Rgb,
}

impl Palette {
    /// Resolve the brand against `bg`.
    pub fn resolve(bg: Rgb) -> Self {
        let light = is_light(bg);
        let brand = if light { LIGHT } else { DARK };
        let band = if light {
            blend(bg, (0, 0, 0), 0.06)
        } else {
            blend(bg, (255, 255, 255), 0.10)
        };
        Palette {
            primary: readable(brand.primary, bg),
            terminal: readable(brand.terminal, bg),
            nominal: readable(brand.nominal, bg),
            caution: readable(brand.caution, bg),
            critical: readable(brand.critical, bg),
            band,
        }
    }

    /// Every colour a role can take, for tests.
    pub fn roles(&self) -> [Rgb; 5] {
        [
            self.primary,
            self.terminal,
            self.nominal,
            self.caution,
            self.critical,
        ]
    }
}

/// A background is light when its Rec. 601 luma is at least half scale,
/// the rule Neovim and Codex use.
pub fn is_light((r, g, b): Rgb) -> bool {
    (299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b)) / 1000 >= 128
}

/// WCAG relative luminance of an sRGB colour.
pub fn relative_luminance((r, g, b): Rgb) -> f32 {
    let lin = |c: u8| {
        let c = f32::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

/// WCAG contrast ratio between two colours, 1 to 21.
pub fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// `from` moved `t` of the way to `to` (`t` in 0..=1).
pub fn blend(from: Rgb, to: Rgb, t: f32) -> Rgb {
    let mix = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
    (mix(from.0, to.0), mix(from.1, to.1), mix(from.2, to.2))
}

/// `preferred` when it reads on `bg`; otherwise the nearest step of it
/// toward black or white (whichever contrasts more with `bg`) that does.
/// One of the two endpoints always reads on any background, so the
/// result never falls below [`MIN_TEXT_CONTRAST`].
pub fn readable(preferred: Rgb, bg: Rgb) -> Rgb {
    if contrast(preferred, bg) >= MIN_TEXT_CONTRAST {
        return preferred;
    }
    let (black, white) = ((0, 0, 0), (255, 255, 255));
    let endpoint = if contrast(black, bg) >= contrast(white, bg) {
        black
    } else {
        white
    };
    (1..=255u8)
        .map(|step| blend(preferred, endpoint, f32::from(step) / 255.0))
        .find(|c| contrast(*c, bg) >= MIN_TEXT_CONTRAST)
        .unwrap_or(endpoint)
}
