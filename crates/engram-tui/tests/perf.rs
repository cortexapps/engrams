//! The budgets of ADR 0124 D5 as tests: a steady-state frame allocates
//! nothing, a frame over a 10k-event transcript stays inside the time
//! budget, and the live heap for that transcript stays inside the memory
//! budget. The counting allocator is the only instrumentation, and the
//! only `unsafe` in the crate: a `GlobalAlloc` cannot be written without
//! it, so the crate's `unsafe_code` lint is `deny` (not the workspace's
//! `forbid`) and this file alone allows it.

#![allow(unsafe_code)]

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use engram_tui::app::{App, Msg, NetMsg};
use engram_tui::event::Envelope;
use engram_tui::render::{draw_fullscreen, Theme};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// 10k events shaped like a real session: prompts, replies with a few
/// paragraphs, tool calls with multi-line results.
fn big_history() -> Vec<Envelope> {
    let mut out: Vec<Envelope> = Vec::with_capacity(10_000);
    fn push(out: &mut Vec<Envelope>, kind: &str, payload: String) {
        let idx = out.len() as i64;
        out.push(Envelope {
            idx: Some(idx),
            kind: kind.to_string(),
            payload_json: payload,
        });
    }
    let mut run = 0;
    while out.len() < 10_000 {
        run += 1;
        push(
            &mut out,
            "agent_message",
            format!(
                r#"{{"run_id":"","message_id":"u{run}","role":"user","text":"Prompt number {run}: please look into the module and report what you find, then fix it.","prompt_id":"p{run}"}}"#
            ),
        );
        push(
            &mut out,
            "run_started",
            format!(r#"{{"run_id":"r{run}","prompt_id":"p{run}"}}"#),
        );
        push(
            &mut out,
            "agent_message",
            format!(
                r#"{{"run_id":"r{run}","message_id":"m{run}","role":"assistant","text":"Looking at the module now. The function has three callers and one of them passes a stale handle.\n\nI will read the callers first, then the handle type, and then propose a fix that keeps the public API."}}"#
            ),
        );
        for t in 0..4 {
            push(
                &mut out,
                "tool_call_started",
                format!(
                    r#"{{"run_id":"r{run}","tool_call_id":"t{run}-{t}","tool_name":"Read","args_summary":"crates/example/src/module_{t}.rs"}}"#
                ),
            );
            push(
                &mut out,
                "tool_call_completed",
                format!(
                    r#"{{"run_id":"r{run}","tool_call_id":"t{run}-{t}","tool_name":"Read","ok":true,"duration_ms":7,"result_summary":"fn handle() -> Handle {{\n    let h = open();\n    h.touch();\n    h\n}}\n\nfn caller_a() {{ handle().use_it(); }}\nfn caller_b() {{ handle().use_it(); }}\nfn caller_c() {{ handle().use_it(); }}"}}"#
                ),
            );
        }
        push(
            &mut out,
            "agent_message",
            format!(
                r#"{{"run_id":"r{run}","message_id":"m{run}b","role":"assistant","text":"The stale handle comes from caller_b, which caches the result across a reconnect. I changed it to call handle() each time and added a test."}}"#
            ),
        );
        push(
            &mut out,
            "run_completed",
            format!(r#"{{"run_id":"r{run}","ok":true}}"#),
        );
    }
    out.truncate(10_000);
    out
}

fn frame(
    terminal: &mut Terminal<TestBackend>,
    app: &mut App,
    theme: &Theme,
    scratch: &mut Vec<(usize, u16)>,
) {
    terminal
        .draw(|f| {
            let area = f.area();
            draw_fullscreen(f.buffer_mut(), area, app, theme, scratch);
        })
        .unwrap();
}

#[test]
fn steady_state_frames_allocate_nothing_and_fit_the_time_budget() {
    let theme = Theme::new(true);
    let mut terminal = Terminal::new(TestBackend::new(200, 60)).unwrap();
    let mut scratch = Vec::with_capacity(256);

    let mut app = App::new(false, 200, 60);
    app.update(Msg::Net(NetMsg::Sessions(Ok(common::rows()))));
    app.attach("s-1");
    app.cmds.clear();
    let before = LIVE.load(Ordering::Relaxed);
    let events = big_history();
    let raw_bytes: usize = events
        .iter()
        .map(|e| e.payload_json.len() + e.kind.len())
        .sum();
    app.update(Msg::Net(NetMsg::History {
        session_id: "s-1".into(),
        events,
        older: false,
    }));
    // Lay out every cell once (the worst case: a jump to the top).
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Home,
        KeyModifiers::CONTROL,
    )));
    frame(&mut terminal, &mut app, &theme, &mut scratch);
    app.update(Msg::Key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL)));
    frame(&mut terminal, &mut app, &theme, &mut scratch);
    let live = LIVE.load(Ordering::Relaxed).saturating_sub(before);
    let cells = app.attached.as_ref().unwrap().reducer.transcript.len();
    // 13 events per scripted run fold to 7 cells.
    assert!(cells >= 5_000, "{cells} cells");
    eprintln!("perf: {cells} cells, {raw_bytes} B raw, {live} B live heap");
    // ADR 0124 D5: the transcript is stored once. Allow the wrap cache
    // and bookkeeping on top of the raw text, never a second copy.
    // Measured: 1.9 MB raw → 2.4 MB live (text once + wrap cache + maps).
    assert!(
        live < raw_bytes * 3 / 2 + 1024 * 1024,
        "live heap {live} B for {raw_bytes} B of events ({cells} cells)"
    );

    // Scrolled into the middle: every frame paints 60 rows of cells.
    app.update(Msg::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)));
    app.update(Msg::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)));
    frame(&mut terminal, &mut app, &theme, &mut scratch);
    frame(&mut terminal, &mut app, &theme, &mut scratch);

    let allocs_before = ALLOCS.load(Ordering::Relaxed);
    let mut times = Vec::with_capacity(200);
    for _ in 0..200 {
        let t = Instant::now();
        frame(&mut terminal, &mut app, &theme, &mut scratch);
        times.push(t.elapsed());
    }
    let allocs = ALLOCS.load(Ordering::Relaxed) - allocs_before;
    // ratatui's own diff/flush path is outside our control; our drawing
    // must add nothing. The TestBackend draw of an unchanged frame
    // allocates for its diff vector only.
    // Measured: 1 (ratatui's diff vector on the first changed frame).
    assert!(allocs <= 8, "{allocs} allocations over 200 steady frames");
    times.sort();
    let p99 = times[197];
    eprintln!(
        "perf: {allocs} allocations / 200 frames, p50 {:?}, p99 {p99:?}",
        times[100]
    );
    // Measured: p99 0.3 ms release, 2.5 ms debug. Debug gets headroom for
    // slow CI runners; release is the ADR 0124 D5 budget.
    let budget_ms = if cfg!(debug_assertions) { 25 } else { 2 };
    assert!(
        p99.as_millis() <= budget_ms,
        "p99 frame {p99:?} exceeds {budget_ms} ms"
    );
}
