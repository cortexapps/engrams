//! `engrams tui`: a terminal client for engrams sessions (ADR 0124).
//!
//! The crate is split so each part is testable without a terminal:
//! `app` (state + update), `reducer` (events → cells), `transcript` (cells +
//! wrap cache), `render` (cells → buffer), `composer`, `keymap`, and the
//! runtime in this file that owns the terminal, the input task and the
//! network task.

pub mod app;
pub mod commands;
pub mod composer;
pub mod event;
pub mod keymap;
pub mod markdown;
pub mod net;
pub mod reducer;
pub mod render;
pub mod sessions;
pub mod shell;
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
use engram_app_client::shell::{Shell, ShellMsg};
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
    /// Screen-reader mode: word gutters, no glyph-only state.
    pub ax: bool,
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
    let mut theme = Theme::new(opts.color);
    theme.ax = opts.ax;

    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let mut input = spawn_input(tx.clone());

    let size = terminal.size().map_err(|e| e.to_string())?;
    let mut app = App::new(opts.inline, size.width, size.height);
    app.ax = opts.ax;
    let mut net = Net::new(clients, tx.clone());
    if let Some(id) = &opts.session_id {
        app.attach(id);
    }

    let mut scratch: Vec<(usize, u16)> = Vec::with_capacity(256);
    let mut last_draw = Instant::now() - FRAME;
    let mut dirty = true;
    let mut title = String::new();
    // Native scrollback mode shows a focused shell on the alternate
    // screen, through a second terminal; the inline one keeps its state
    // and resumes when the overlay leaves.
    let mut overlay: Option<Term> = None;
    let result = loop {
        // Run the commands the last update emitted.
        let mut failed: Option<String> = None;
        let mut edited: Option<Option<String>> = None;
        let cmds: Vec<Cmd> = std::mem::take(&mut app.cmds);
        for cmd in cmds {
            match cmd {
                Cmd::LoadSessions => net.load_sessions(),
                Cmd::LoadProfiles => net.load_profiles(),
                Cmd::CreateTask { profile_id, prompt } => net.create_task(profile_id, prompt),
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
                Cmd::Resume { session_id } => net.resume(session_id),
                Cmd::ViewTranscript { text } => {
                    input.abort();
                    let _ = (&mut input).await;
                    guard.suspend();
                    let _ = edit_in_editor(&text);
                    if let Err(e) = guard.resume(!opts.inline, opts.mouse) {
                        failed = Some(e.to_string());
                        break;
                    }
                    input = spawn_input(tx.clone());
                    if let Err(e) = terminal.clear() {
                        failed = Some(e.to_string());
                        break;
                    }
                    dirty = true;
                }
                Cmd::Dump { text } => {
                    // Leave the alternate screen, write to the main one
                    // (the terminal keeps it as scrollback), come back.
                    guard.suspend();
                    {
                        let mut out = std::io::stdout();
                        let _ = out.write_all(text.as_bytes());
                        let _ = out.flush();
                    }
                    if let Err(e) = guard.resume(!opts.inline, opts.mouse) {
                        failed = Some(e.to_string());
                        break;
                    }
                    if let Err(e) = terminal.clear() {
                        failed = Some(e.to_string());
                        break;
                    }
                    dirty = true;
                }
                Cmd::Dequeue {
                    session_id,
                    prompt_id,
                } => net.dequeue(session_id, prompt_id),
                Cmd::Complete {
                    session_id,
                    tool_call_id,
                    result_json,
                } => net.complete_tool_call(session_id, tool_call_id, result_json),
                Cmd::ShellOpen {
                    session_id,
                    cols,
                    rows,
                } => net.shell_open(session_id, cols, rows),
                Cmd::ShellInput(bytes) => net.shell_input(bytes),
                Cmd::ShellResize { cols, rows } => net.shell_resize(cols, rows),
                Cmd::ShellClose => net.shell_close(),
                Cmd::EditDraft { text } => {
                    // The editor owns the tty: stop reading input (the
                    // stream's reader thread would steal keystrokes), give
                    // the terminal back, run, and take everything back.
                    input.abort();
                    let _ = (&mut input).await;
                    guard.suspend();
                    let result = edit_in_editor(&text);
                    if let Err(e) = guard.resume(!opts.inline, opts.mouse) {
                        failed = Some(e.to_string());
                        break;
                    }
                    input = spawn_input(tx.clone());
                    if let Err(e) = terminal.clear() {
                        failed = Some(e.to_string());
                        break;
                    }
                    edited = Some(result);
                    dirty = true;
                }
                Cmd::Quit => {}
            }
        }
        if let Some(e) = failed {
            break Err(e);
        }
        if let Some(result) = edited {
            app.draft_edited(result);
        }
        if app.quit {
            break Ok(());
        }

        if dirty && last_draw.elapsed() >= FRAME {
            let want_overlay = opts.inline
                && app
                    .attached
                    .as_ref()
                    .is_some_and(|a| a.shell.as_ref().is_some_and(|s| s.focused));
            if want_overlay != overlay.is_some() {
                if want_overlay {
                    let ov = terminal::enter_overlay().and_then(|()| {
                        Terminal::with_options(
                            CrosstermBackend::new(guard.stdout()),
                            TerminalOptions {
                                viewport: Viewport::Fullscreen,
                            },
                        )
                    });
                    match ov {
                        Ok(t) => overlay = Some(t),
                        Err(e) => break Err(e.to_string()),
                    }
                } else {
                    overlay = None;
                    terminal::leave_overlay();
                }
            }
            let drawn = match &mut overlay {
                Some(ov) => draw_overlay(ov, &app, &theme),
                None => draw(&mut terminal, &mut app, &theme, &mut scratch),
            };
            if let Err(e) = drawn {
                break Err(e.to_string());
            }
            last_draw = Instant::now();
            dirty = false;
            let want = app
                .attached
                .as_ref()
                .map(|a| {
                    let mark = if a.reducer.awaiting_decision() {
                        "⚠ needs you · "
                    } else if a.reducer.is_running() {
                        "⟳ "
                    } else {
                        ""
                    };
                    format!("{mark}{} · engrams", a.title)
                })
                .unwrap_or_else(|| "engrams".to_string());
            if want != title {
                terminal::set_title(&want);
                title = want;
            }
            for alert in app.alerts.drain(..) {
                terminal::notify(&alert);
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
    if overlay.take().is_some() {
        terminal::leave_overlay();
    }
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

/// The input task: crossterm events → messages. Restartable, because the
/// editor hand-off drops the stream.
fn spawn_input(tx: mpsc::UnboundedSender<Msg>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
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
            if tx.send(msg).is_err() {
                break;
            }
        }
    })
}

/// Run `$VISUAL` (else `$EDITOR`, else `vi`) on the draft and return the
/// edited text; `None` when the editor fails or exits non-zero.
fn edit_in_editor(text: &str) -> Option<String> {
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_else(|| "vi".to_string());
    let mut words = editor.split_whitespace();
    let program = words.next()?;
    let path = std::env::temp_dir().join(format!("engrams-draft-{}.md", std::process::id()));
    std::fs::write(&path, text).ok()?;
    let status = std::process::Command::new(program)
        .args(words)
        .arg(&path)
        .status();
    let edited = match status {
        Ok(st) if st.success() => std::fs::read_to_string(&path)
            .ok()
            .map(|s| s.trim_end_matches('\n').to_string()),
        _ => None,
    };
    let _ = std::fs::remove_file(&path);
    edited
}

/// One overlay frame: the shell pane over the whole alternate screen.
fn draw_overlay(terminal: &mut Term, app: &App, theme: &Theme) -> std::io::Result<()> {
    let mut out = std::io::stdout();
    crossterm::queue!(out, crossterm::terminal::BeginSynchronizedUpdate)?;
    terminal.draw(|f| {
        let area = f.area();
        if let Some(p) = render::draw_overlay(f.buffer_mut(), area, app, theme) {
            f.set_cursor_position(p);
        }
    })?;
    crossterm::queue!(out, crossterm::terminal::EndSynchronizedUpdate)?;
    out.flush()
}

/// `engrams session shell <id>`: the local terminal becomes the session
/// shell until the remote shell exits. Raw mode, every byte through, the
/// window size follows `SIGWINCH`.
pub fn shell_passthrough(clients: Clients, session_id: &str) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let result = rt.block_on(async {
        let (cols, rows) = crossterm::terminal::size().map_err(|e| e.to_string())?;
        let mut shell = Shell::open(&clients.transport, session_id, cols, rows)
            .await
            .map_err(|e| e.to_string())?;
        let raw = terminal::Guard::raw().map_err(|e| e.to_string())?;
        let result = passthrough_loop(&mut shell).await;
        drop(raw);
        result
    });
    // A blocked stdin read must not hold the process open.
    rt.shutdown_background();
    result
}

async fn passthrough_loop(shell: &mut Shell) -> Result<(), String> {
    use tokio::io::AsyncReadExt;
    let mut stdin = tokio::io::stdin();
    let mut out = std::io::stdout();
    let mut buf = [0u8; 4096];
    #[cfg(unix)]
    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
        .map_err(|e| e.to_string())?;
    #[cfg(not(unix))]
    let mut winch = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            n = stdin.read(&mut buf) => match n {
                Ok(0) | Err(_) => return Ok(()),
                Ok(n) => shell.input(&buf[..n]).await.map_err(|e| e.to_string())?,
            },
            msg = shell.next() => match msg {
                Ok(Some(ShellMsg::Output(bytes))) => {
                    out.write_all(&bytes).and_then(|()| out.flush()).map_err(|e| e.to_string())?;
                }
                Ok(Some(ShellMsg::Title(_))) => {}
                Ok(None) => return Ok(()),
                Err(e) => return Err(e.to_string()),
            },
            _ = winch_wait(&mut winch) => {
                let (cols, rows) = crossterm::terminal::size().map_err(|e| e.to_string())?;
                shell.resize(cols, rows).await.map_err(|e| e.to_string())?;
            }
        }
    }
}

#[cfg(unix)]
async fn winch_wait(sig: &mut tokio::signal::unix::Signal) {
    let _ = sig.recv().await;
}

#[cfg(not(unix))]
async fn winch_wait(tick: &mut tokio::time::Interval) {
    tick.tick().await;
}

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
