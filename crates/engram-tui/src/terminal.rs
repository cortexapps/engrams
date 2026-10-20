//! Terminal lifecycle: raw mode, the alternate screen (fullscreen mode
//! only), bracketed paste, focus events, optional mouse capture, and the
//! kitty keyboard flags where the terminal supports them. Everything is
//! undone in reverse on drop and from the panic hook, so a crash never
//! leaves the shell in raw mode, nor a program status record behind.
//!
//! One probe at start asks for the kitty flags and for Program Status
//! Protocol support (OSC 7501) in a single round trip, with primary
//! device attributes as the sentinel every terminal answers.

use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::queue;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};

static ALT_SCREEN: AtomicBool = AtomicBool::new(false);
static MOUSE: AtomicBool = AtomicBool::new(false);
static KITTY: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static PROBE: OnceLock<Probe> = OnceLock::new();

/// What the terminal answered to the start-up probe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Probe {
    /// The kitty keyboard protocol (a `CSI ? flags u` reply).
    pub kitty: bool,
    /// The Program Status Protocol (an `OSC 7501 ; ?` reply).
    pub program_status: bool,
}

/// The probe's answer; all false before [`Guard::enter`].
pub fn probe() -> Probe {
    PROBE.get().copied().unwrap_or_default()
}

/// The kitty flags pushed on every screen the TUI draws on.
const KITTY_FLAGS: KeyboardEnhancementFlags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
    .union(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS);

/// Restore the terminal. Idempotent; safe from the panic hook.
pub fn restore() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut out = io::stdout();
    crate::status::clear_on_exit(&mut out);
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
        if let Err(e) = enter_modes(alt_screen, mouse) {
            // Raw mode may be on already: leave the terminal as found.
            restore();
            return Err(e);
        }
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
    // Probed once (a resume after `$EDITOR` reuses the answer), and
    // before focus and mouse reports are on, so none of theirs crowd the
    // replies.
    if PROBE.get().is_none() {
        let _ = PROBE.set(run_probe());
    }
    queue!(out, EnableBracketedPaste, EnableFocusChange)?;
    if mouse {
        queue!(out, EnableMouseCapture)?;
        MOUSE.store(true, Ordering::SeqCst);
    }
    if probe().kitty {
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

/// How long the probe waits for the device attributes reply. A terminal
/// that never answers costs this once, at start.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Bytes kept when the probe's buffer fills: more than the longest prefix
/// `scan` needs to see whole.
#[cfg(unix)]
const PROBE_TAIL: usize = 32;

/// `ENGRAMS_PROGRAM_STATUS=1` or `0` sets OSC 7501 support instead of
/// asking the terminal.
fn program_status_override() -> Option<bool> {
    match std::env::var("ENGRAMS_PROGRAM_STATUS").ok()?.as_str() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

/// Ask for the kitty flags (and OSC 7501 unless overridden), then for
/// the primary device attributes, and read the replies up to DA1. Raw
/// mode must be on. Runs before the input stream exists, so no reader
/// competes for the replies. Bytes the user types in that one round trip
/// are dropped: crossterm's reader cannot be handed them back.
#[cfg(unix)]
fn run_probe() -> Probe {
    use std::io::IsTerminal;
    use std::os::fd::AsFd;

    let forced = program_status_override();
    let mut out = io::stdout();
    let query: &[u8] = if forced.is_some() {
        b"\x1b[?u\x1b[c"
    } else {
        b"\x1b[?u\x1b]7501;?\x1b\\\x1b[c"
    };
    if out.write_all(query).and_then(|()| out.flush()).is_err() {
        return Probe::default();
    }
    // The replies come on the terminal the keys come from: stdin, or the
    // controlling terminal when stdin is redirected (as crossterm reads).
    let tty;
    let stdin = io::stdin();
    let fd = if stdin.is_terminal() {
        stdin.as_fd()
    } else {
        match std::fs::File::open("/dev/tty") {
            Ok(f) => {
                tty = f;
                tty.as_fd()
            }
            Err(_) => return Probe::default(),
        }
    };
    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    let mut buf = [0u8; 256];
    let mut len = 0;
    let mut found = Probe::default();
    loop {
        let (probe, done) = scan(&buf[..len]);
        found.kitty |= probe.kitty;
        found.program_status |= probe.program_status;
        if done {
            return Probe {
                program_status: forced.unwrap_or(found.program_status),
                ..found
            };
        }
        if len == buf.len() {
            // Typed or pasted bytes filled the buffer: keep what was
            // found and the tail (a reply split across reads), read on.
            buf.copy_within(len - PROBE_TAIL..len, 0);
            len = PROBE_TAIL;
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Probe {
                program_status: forced.unwrap_or(false),
                ..found
            };
        }
        let mut fds = [rustix::event::PollFd::new(
            &fd,
            rustix::event::PollFlags::IN,
        )];
        let timeout = rustix::event::Timespec {
            tv_sec: left.as_secs() as _,
            tv_nsec: left.subsec_nanos() as _,
        };
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(0) => continue,
            Ok(_) => match rustix::io::read(fd, &mut buf[len..]) {
                Ok(0) | Err(_) => return Probe::default(),
                Ok(n) => len += n,
            },
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => return Probe::default(),
        }
    }
}

/// Windows consoles answer neither query through this path: no kitty
/// flags, and OSC 7501 only when forced.
#[cfg(not(unix))]
fn run_probe() -> Probe {
    Probe {
        kitty: false,
        program_status: program_status_override().unwrap_or(false),
    }
}

/// Read the replies in `buf`: the kitty flags (`CSI ? <n> u`), the OSC
/// 7501 detection reply (`OSC 7501 ; ? …`), and whether the DA1 reply
/// (`CSI ? … c`), which always comes last, has arrived.
pub fn scan(buf: &[u8]) -> (Probe, bool) {
    let mut p = Probe::default();
    let mut i = 0;
    while i < buf.len() {
        const STATUS_REPLY: &[u8] = b"\x1b]7501;?";
        if buf[i..].starts_with(STATUS_REPLY) {
            p.program_status = true;
            i += STATUS_REPLY.len();
            continue;
        }
        if buf[i..].starts_with(b"\x1b[?") {
            let rest = &buf[i + 3..];
            let n = rest
                .iter()
                .take_while(|b| b.is_ascii_digit() || **b == b';')
                .count();
            match rest.get(n) {
                Some(b'u') => p.kitty = true,
                Some(b'c') => return (p, true),
                _ => {}
            }
            i += 3 + n;
            continue;
        }
        i += 1;
    }
    (p, false)
}
