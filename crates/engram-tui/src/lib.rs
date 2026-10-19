//! `engrams tui`: a terminal client for engrams sessions (ADR 0124).
//!
//! The crate is split so each part is testable without a terminal:
//! `app` (state + update), `reducer` (events → cells), `transcript` (cells +
//! wrap cache), `render` (cells → buffer), `composer`, `keymap`, and the
//! runtime in this file that owns the terminal, the input task and the
//! network task.

pub mod app;
pub mod composer;
pub mod event;
pub mod keymap;
pub mod net;
pub mod reducer;
pub mod render;
pub mod sessions;
pub mod terminal;
pub mod transcript;
pub mod wrap;

use std::io::Write;
use std::time::{Duration, Instant};

use crossterm::event::Event as CtEvent;
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc;

use crate::app::{App, Cmd, Msg};
use crate::net::Net;
use crate::render::Theme;
use engram_app_client::Clients;

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Attach to this session at start; otherwise open the session list.
    pub session_id: Option<String>,
    /// Native scrollback mode (`--no-alt-screen`).
    pub inline: bool,
    /// Capture the mouse for wheel scrolling.
    pub mouse: bool,
    /// Use color (false under `NO_COLOR`).
    pub color: bool,
}

/// Minimum interval between frames (~120 Hz).
const FRAME: Duration = Duration::from_millis(8);
/// Rows the inline viewport owns at the bottom of the terminal.
const INLINE_ROWS: u16 = 14;

/// Run the TUI to completion on a current-thread runtime.
pub fn run(clients: Clients, opts: Options) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(run_async(clients, opts))
}

async fn run_async(clients: Clients, opts: Options) -> Result<(), String> {
    let guard = terminal::Guard::enter(!opts.inline, opts.mouse).map_err(|e| e.to_string())?;
    let backend = CrosstermBackend::new(guard.stdout());
    let viewport = if opts.inline {
        Viewport::Inline(INLINE_ROWS)
    } else {
        Viewport::Fullscreen
    };
    let mut terminal =
        Terminal::with_options(backend, TerminalOptions { viewport }).map_err(|e| e.to_string())?;
    let theme = Theme::new(opts.color);

    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let input_tx = tx.clone();
    let input = tokio::spawn(async move {
        let mut events = crossterm::event::EventStream::new();
        while let Some(Ok(ev)) = events.next().await {
            let msg = match ev {
                CtEvent::Key(k) => Msg::Key(k),
                CtEvent::Paste(p) => Msg::Paste(p),
                CtEvent::Mouse(m) => Msg::Mouse(m),
                CtEvent::Resize(w, h) => Msg::Resize(w, h),
                CtEvent::FocusGained => Msg::Focus(true),
                CtEvent::FocusLost => Msg::Focus(false),
            };
            if input_tx.send(msg).is_err() {
                break;
            }
        }
    });

    let size = terminal.size().map_err(|e| e.to_string())?;
    let mut app = App::new(opts.inline, size.width, size.height);
    let mut net = Net::new(clients, tx);
    if let Some(id) = &opts.session_id {
        app.attach(id);
    }

    let mut scratch: Vec<(usize, u16)> = Vec::with_capacity(256);
    let mut last_draw = Instant::now() - FRAME;
    let mut dirty = true;
    let mut title = String::new();
    let result = loop {
        // Run the commands the last update emitted.
        for cmd in app.cmds.drain(..) {
            match cmd {
                Cmd::LoadSessions => net.load_sessions(),
                Cmd::Attach { session_id } => net.attach(session_id),
                Cmd::Detach => net.detach(),
                Cmd::LoadOlder {
                    session_id,
                    before_idx,
                } => net.load_older(session_id, before_idx),
                Cmd::Send {
                    session_id,
                    prompt_id,
                    text,
                } => net.send_prompt(session_id, prompt_id, text),
                Cmd::Interrupt { session_id } => net.interrupt(session_id),
                Cmd::Dequeue {
                    session_id,
                    prompt_id,
                } => net.dequeue(session_id, prompt_id),
                Cmd::Quit => {}
            }
        }
        if app.quit {
            break Ok(());
        }

        if dirty && last_draw.elapsed() >= FRAME {
            if let Err(e) = draw(&mut terminal, &mut app, &theme, &mut scratch) {
                break Err(e.to_string());
            }
            last_draw = Instant::now();
            dirty = false;
            let want = app
                .attached
                .as_ref()
                .map(|a| format!("{} · engrams", a.title))
                .unwrap_or_else(|| "engrams".to_string());
            if want != title {
                terminal::set_title(&want);
                title = want;
            }
            // The walk hit the top of memory: page history in.
            if app.attached.as_ref().is_some_and(|a| a.want_older) {
                app.request_older();
                continue;
            }
        }

        let frame_wait = if dirty {
            FRAME.saturating_sub(last_draw.elapsed())
        } else if app.wants_tick() {
            Duration::from_secs(1)
        } else {
            Duration::from_secs(3600)
        };
        tokio::select! {
            msg = rx.recv() => {
                let Some(msg) = msg else { break Ok(()); };
                app.update(msg);
                // Coalesce whatever else is already queued into this frame.
                while let Ok(msg) = rx.try_recv() {
                    app.update(msg);
                }
                dirty = true;
            }
            () = tokio::time::sleep(frame_wait) => {
                if !dirty {
                    app.update(Msg::Tick);
                }
                dirty = true;
            }
        }
    };

    input.abort();
    net.detach();
    if opts.inline {
        // Leave the shell prompt below the viewport.
        let _ = terminal.clear();
        let mut out = std::io::stdout();
        let _ = writeln!(out);
    }
    drop(terminal);
    drop(guard);
    result
}

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

fn draw(
    terminal: &mut Term,
    app: &mut App,
    theme: &Theme,
    scratch: &mut Vec<(usize, u16)>,
) -> std::io::Result<()> {
    if app.inline {
        // Finalized cells go to native scrollback once, then leave memory.
        if let Some(a) = &mut app.attached {
            loop {
                let t = &mut a.reducer.transcript;
                let width = terminal.size()?.width;
                let Some((_, e)) = t.next_committable() else {
                    break;
                };
                let h = e.height(width.saturating_sub(2).max(1));
                if h > 0 {
                    terminal.insert_before(h, |buf| render::paint_cell(buf, e, theme))?;
                }
                t.release();
            }
        }
    }
    // ADR 0124 D4: one frame, one synchronized update, so a terminal
    // never paints a half-diffed screen.
    let mut out = std::io::stdout();
    crossterm::queue!(out, crossterm::terminal::BeginSynchronizedUpdate)?;
    let mut cursor = None;
    let mut want_older = false;
    terminal.draw(|f| {
        let area = f.area();
        let buf = f.buffer_mut();
        if app.inline {
            cursor = render::draw_inline(buf, area, app, theme, scratch);
        } else {
            let (c, w) = render::draw_fullscreen(buf, area, app, theme, scratch);
            cursor = c;
            want_older = w;
        }
        if let Some(p) = cursor {
            f.set_cursor_position(p);
        }
    })?;
    crossterm::queue!(out, crossterm::terminal::EndSynchronizedUpdate)?;
    out.flush()?;
    if let Some(a) = &mut app.attached {
        a.want_older = want_older;
    }
    Ok(())
}
