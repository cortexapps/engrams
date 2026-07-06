//! Integration test: noop harness ↔ HarnessHub via tokio::io::duplex.
//!
//! Pairs `engram-harness-noop` (the test harness, on one end of an
//! in-memory duplex stream) against `engram_host_agent::harness::HarnessHub`
//! (the hub, on the other end). Asserts that the events the noop
//! harness emits land in the configured `EventSink` in order, with
//! `result_summary` preserved verbatim.

use std::sync::Arc;
use std::time::Duration;

use engram_core::{SandboxId, SessionId};
use engram_harness_noop::{run as run_noop, NoopConfig};
use engram_harness_proto::HarnessEvent;
use engram_host_agent::harness::{EventSink, HarnessHub};
use parking_lot::Mutex;

#[tokio::test]
async fn noop_harness_events_land_in_event_sink_in_order() {
    let collected: Arc<Mutex<Vec<HarnessEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let collected_for_sink = collected.clone();
    let sink: EventSink = Arc::new(move |_session_id, _sandbox_id, ev| {
        let collected = collected_for_sink.clone();
        Box::new(Box::pin(async move {
            collected.lock().push(ev);
        }))
    });
    let hub = HarnessHub::new(
        sink,
        engram_host_agent::bindings::BindingStore::open(
            tempfile::tempdir().expect("tempdir").keep(),
        )
        .expect("binding store"),
    );

    let session_id = SessionId::new();
    let sandbox_id = SandboxId::new();
    // ADR 0073: bind so the attach token validates (the same
    // choreography the host-agent runs before spawning a harness).
    hub.bind_session(session_id, sandbox_id, 1).expect("bind");
    let (host_side, harness_side) = tokio::io::duplex(1 << 16);
    hub.accept_connection(sandbox_id, Some(session_id), host_side);

    let mut cfg = NoopConfig::for_session(session_id);
    cfg.sandbox_id = sandbox_id;
    cfg.binding_epoch = 1;
    cfg.tool_calls = 3;
    cfg.interval = Duration::from_millis(1);
    cfg.tool_call_duration_ms = 1;
    cfg.result_summary_template = "noop tool result".into();

    // Run noop in a task; once it emits Idle it stays connected
    // waiting for shutdown. Send Shutdown to release it.
    let run_handle = tokio::spawn(async move { run_noop(harness_side, cfg).await });
    // Wait for the hub to register the connection so shutdown
    // doesn't race the handshake.
    for _ in 0..100 {
        if hub.attached_count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Wait for the noop's events to arrive (RunStarted + 3
    // ToolCallStarted/Completed pairs + Idle = 8 events). Poll on
    // the *semantic* condition — all 3 ToolCallCompleted received —
    // rather than a raw len() threshold. The previous `len >= 8`
    // exit could fire after only 2 completes (with a 7th event
    // being ToolCallStarted #3), which then races the shutdown
    // below: shutdown short-circuits the harness before the 3rd
    // tool call finishes, the assertion downstream sees 2 instead
    // of 3. Bump the budget to 5 s — this is in-memory loopback,
    // even slow runners shouldn't take more than tens of ms.
    let mut tool_completes_seen = 0;
    for _ in 0..1000 {
        tool_completes_seen = collected
            .lock()
            .iter()
            .filter(|e| matches!(e, HarnessEvent::ToolCallCompleted { .. }))
            .count();
        if tool_completes_seen >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        tool_completes_seen >= 3,
        "timed out waiting for 3 ToolCallCompleted events; saw {tool_completes_seen}",
    );

    hub.shutdown(sandbox_id, 0).await.expect("shutdown");
    let outcome = run_handle.await.unwrap().unwrap();
    assert_eq!(
        outcome,
        engram_harness_noop::NoopOutcome::Shutdown,
        "noop should exit via Shutdown"
    );

    let events = collected.lock();
    // Tolerate either exact-count or trailing-Idle-yet-to-arrive
    // depending on scheduling, but the core structure must hold.
    assert!(
        events.len() >= 7,
        "expected at least 7 events, got {}",
        events.len()
    );
    assert!(matches!(events[0], HarnessEvent::RunStarted { .. }));
    let mut completed_count = 0usize;
    for ev in events.iter() {
        if let HarnessEvent::ToolCallCompleted { result_summary, .. } = ev {
            completed_count += 1;
            assert_eq!(result_summary.as_deref(), Some("noop tool result"));
        }
    }
    assert_eq!(completed_count, 3, "expected 3 ToolCallCompleted events");
}

/// ADR 0073 / #447 regression (the b9b28452 shape, end to end): a
/// host-agent restart rebuilds the hub with EMPTY memory, and the
/// surviving in-guest harness re-dials. Pre-0067 that re-dial bounced
/// "no sandbox bound to this session_id" until a coordinator rebind
/// pass repopulated an in-memory map; post-0067 the durable binding
/// record on disk IS the routing, so a FRESH hub over the same
/// bindings dir accepts the re-dial immediately — zero coordinator
/// involvement, zero rebuild pass — and events flow.
#[tokio::test]
async fn survivor_redial_attaches_against_a_fresh_hub_with_zero_rebuild() {
    let bindings_dir = tempfile::tempdir().expect("tempdir");
    let session_id = SessionId::new();
    let sandbox_id = SandboxId::new();

    // "Old" host-agent process: binds (epoch 1), then dies.
    {
        let sink: EventSink = Arc::new(|_, _, _| Box::new(Box::pin(async {})));
        let hub = HarnessHub::new(
            sink,
            engram_host_agent::bindings::BindingStore::open(bindings_dir.path())
                .expect("binding store"),
        );
        hub.bind_session(session_id, sandbox_id, 1).expect("bind");
        // Dropped: the process is gone; only the dir survives.
    }

    // "New" host-agent process: fresh hub, same dir, nothing rebinds.
    let collected: Arc<Mutex<Vec<HarnessEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let collected_for_sink = collected.clone();
    let sink: EventSink = Arc::new(move |_session_id, _sandbox_id, ev| {
        let collected = collected_for_sink.clone();
        Box::new(Box::pin(async move {
            collected.lock().push(ev);
        }))
    });
    let hub = HarnessHub::new(
        sink,
        engram_host_agent::bindings::BindingStore::open(bindings_dir.path())
            .expect("binding store"),
    );

    // The survivor harness re-dials with its spawn-time token — the
    // session-lookup path (the TCP listener / vsock sink shape).
    let (host_side, harness_side) = tokio::io::duplex(1 << 16);
    hub.accept_via_session_lookup(host_side);

    let mut cfg = NoopConfig::for_session(session_id);
    cfg.sandbox_id = sandbox_id;
    cfg.binding_epoch = 1;
    cfg.tool_calls = 1;
    cfg.interval = Duration::from_millis(1);
    cfg.tool_call_duration_ms = 1;
    let run_handle = tokio::spawn(async move { run_noop(harness_side, cfg).await });

    for _ in 0..200 {
        if hub.attached_count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        hub.attached_count(),
        1,
        "survivor re-dial must attach against the fresh hub with no rebind pass",
    );

    // Prompt delivery works through the fresh registration.
    for _ in 0..200 {
        if !collected.lock().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        !collected.lock().is_empty(),
        "survivor's events must flow through the fresh hub",
    );

    hub.shutdown(sandbox_id, 0).await.expect("shutdown");
    let outcome = run_handle.await.expect("noop task").expect("noop run");
    assert!(matches!(
        outcome,
        engram_harness_noop::NoopOutcome::Shutdown
            | engram_harness_noop::NoopOutcome::EmittedAndPeerClosed
    ));
}

/// ADR 0073 fencing (the fbd3794c shape): a harness whose session was
/// re-bound to a NEWER generation presents a stale epoch and is
/// rejected `Superseded` — deterministically, on the first dial. The
/// noop harness surfaces that as AttachRejected (the claude harness
/// exits on the typed variant).
#[tokio::test]
async fn stale_epoch_redial_is_rejected_superseded() {
    let bindings_dir = tempfile::tempdir().expect("tempdir");
    let session_id = SessionId::new();
    let sink: EventSink = Arc::new(|_, _, _| Box::new(Box::pin(async {})));
    let hub = HarnessHub::new(
        sink,
        engram_host_agent::bindings::BindingStore::open(bindings_dir.path())
            .expect("binding store"),
    );
    // The session moved on: a newer generation owns the record.
    let new_sandbox = SandboxId::new();
    hub.bind_session(session_id, new_sandbox, 2)
        .expect("bind@2");

    let (host_side, harness_side) = tokio::io::duplex(1 << 16);
    hub.accept_via_session_lookup(host_side);

    // The old generation's harness re-dials with its frozen token.
    let mut cfg = NoopConfig::for_session(session_id);
    cfg.sandbox_id = SandboxId::new(); // the OLD sandbox
    cfg.binding_epoch = 1;
    let outcome = run_noop(harness_side, cfg).await.expect("noop run");
    assert!(
        matches!(outcome, engram_harness_noop::NoopOutcome::AttachRejected),
        "a stale-epoch dial must be rejected, got {outcome:?}",
    );
    assert_eq!(hub.attached_count(), 0);
}
