//! Terminal lifecycle: raw mode, the alternate screen (fullscreen mode
//! only), bracketed paste, focus events, optional mouse capture, and the
//! kitty keyboard flags where the terminal supports them. Everything is
//! undone in reverse on drop and from the panic hook, so a crash never
//! leaves the shell in raw mode.

use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::{execute, queue};

static ALT_SCREEN: AtomicBool = AtomicBool::new(false);
static MOUSE: AtomicBool = AtomicBool::new(false);
static KITTY: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// The kitty flags pushed on every screen the TUI draws on.
const KITTY_FLAGS: KeyboardEnhancementFlags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
    .union(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS);

/// Restore the terminal. Idempotent; safe from the panic hook.
pub fn restore() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut out = io::stdout();
    if KITTY.swap(false, Ordering::SeqCst) {
        let _ = queue!(out, PopKeyboardEnhancementFlags);
    }
    if MOUSE.swap(false, Ordering::SeqCst) {
        let _ = queue!(out, DisableMouseCapture);
    }
    let _ = queue!(out, DisableFocusChange, DisableBracketedPaste);
    let _ = queue!(out, crossterm::cursor::Show);
    if ALT_SCREEN.swap(false, Ordering::SeqCst) {
        let _ = queue!(out, LeaveAlternateScreen);
    }
    let _ = out.flush();
    let _ = disable_raw_mode();
}

/// The live terminal session. Dropping it restores the terminal.
pub struct Guard {
    _private: (),
}

impl Guard {
    /// Enter raw mode and the requested modes. `alt_screen` is false in
    /// native scrollback mode. Kitty flags are pushed only when the
    /// terminal reports support; `event-types` is left out because several
    /// terminals mis-handle key releases with it.
    pub fn enter(alt_screen: bool, mouse: bool) -> io::Result<Guard> {
        enter_modes(alt_screen, mouse)?;
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            prev(info);
        }));
        Ok(Guard { _private: () })
    }

    /// Raw mode only, for `engrams session shell`: no alternate screen,
    /// no paste or focus reports, and no kitty flags, because every byte
    /// the local terminal sends goes to the remote shell as-is. Restored
    /// on drop and from the panic hook like the full guard.
    pub fn raw() -> io::Result<Guard> {
        enable_raw_mode()?;
        ACTIVE.store(true, Ordering::SeqCst);
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            prev(info);
        }));
        Ok(Guard { _private: () })
    }

    /// Hand the terminal to another program (`$EDITOR`). [`Guard::resume`]
    /// takes it back.
    pub fn suspend(&self) {
        restore();
    }

    pub fn resume(&self, alt_screen: bool, mouse: bool) -> io::Result<()> {
        enter_modes(alt_screen, mouse)
    }

    pub fn stdout(&self) -> Stdout {
        io::stdout()
    }
}

fn enter_modes(alt_screen: bool, mouse: bool) -> io::Result<()> {
    enable_raw_mode()?;
    ACTIVE.store(true, Ordering::SeqCst);
    let mut out = io::stdout();
    if alt_screen {
        queue!(out, EnterAlternateScreen)?;
        ALT_SCREEN.store(true, Ordering::SeqCst);
    }
    queue!(out, EnableBracketedPaste, EnableFocusChange)?;
    if mouse {
        queue!(out, EnableMouseCapture)?;
        MOUSE.store(true, Ordering::SeqCst);
    }
    if matches!(supports_keyboard_enhancement(), Ok(true)) {
        queue!(out, PushKeyboardEnhancementFlags(KITTY_FLAGS))?;
        KITTY.store(true, Ordering::SeqCst);
    }
    out.flush()?;
    Ok(())
}

impl Drop for Guard {
    fn drop(&mut self) {
        restore();
    }
}

/// Enter the alternate screen for an overlay (native scrollback mode's
/// shell pane). The flag keeps [`restore`] exact if the process dies
/// while the overlay is up.
pub fn enter_overlay() -> io::Result<()> {
    let mut out = io::stdout();
    queue!(out, EnterAlternateScreen)?;
    ALT_SCREEN.store(true, Ordering::SeqCst);
    // The kitty keyboard protocol keeps one flag stack per screen: the
    // flags pushed on the main screen do not apply on the alternate one.
    // Without them Ctrl+` reaches the shell as a plain backtick and never
    // hands the keys back.
    if KITTY.load(Ordering::SeqCst) {
        queue!(out, PushKeyboardEnhancementFlags(KITTY_FLAGS))?;
    }
    out.flush()
}

pub fn leave_overlay() {
    if ALT_SCREEN.swap(false, Ordering::SeqCst) {
        let mut out = io::stdout();
        if KITTY.load(Ordering::SeqCst) {
            let _ = queue!(out, PopKeyboardEnhancementFlags);
        }
        let _ = queue!(out, LeaveAlternateScreen);
        let _ = out.flush();
    }
}

/// Ring the bell and post a desktop notification (OSC 9, OSC 777) for an
/// unfocused terminal. Terminals that know neither ignore the sequences.
pub fn notify(text: &str) {
    let text: String = text.chars().filter(|c| !c.is_control()).collect();
    let mut out = io::stdout();
    let _ = write!(
        out,
        "\x07\x1b]9;{text}\x07\x1b]777;notify;engrams;{text}\x07"
    );
    let _ = out.flush();
}

/// Set the window title.
pub fn set_title(title: &str) {
    let _ = execute!(io::stdout(), crossterm::terminal::SetTitle(title));
}
