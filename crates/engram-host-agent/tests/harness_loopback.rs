//! Integration test: noop harness ↔ HarnessHub via tokio::io::duplex.
//!
//! Pairs `engram-harness-noop` (the test harness, on one end of an
//! in-memory duplex stream) against `engram_host_agent::harness::HarnessHub`
//! (the hub, on the other end). Asserts that the events the noop
//! harness emits land in the configured `EventSink` in order, with
//! `transcript_delta` bytes preserved verbatim.

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
    let hub = HarnessHub::new(sink);

    let session_id = SessionId::new();
    let sandbox_id = SandboxId::new();
    let (host_side, harness_side) = tokio::io::duplex(1 << 16);
    hub.accept_connection(sandbox_id, Some(session_id), host_side);

    let mut cfg = NoopConfig::for_session(session_id);
    cfg.tool_calls = 3;
    cfg.interval = Duration::from_millis(1);
    cfg.tool_call_duration_ms = 1;
    cfg.transcript_delta_template = b"{\"step\":\"noop\"}\n".to_vec();

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
    // ToolCallStarted/Completed pairs + Idle = 8 events).
    for _ in 0..200 {
        if collected.lock().len() >= 8 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Hub's last_event_at is updated on every event. Snapshot it
    // BEFORE shutdown — disconnect cleanup wipes the entry by design
    // (the idle evictor reads `attached_count` to know if the
    // sandbox is still around).
    let last_event = hub.last_event_at(sandbox_id);
    assert!(
        last_event.is_some(),
        "last_event_at should be set while attached"
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
        if let HarnessEvent::ToolCallCompleted {
            transcript_delta, ..
        } = ev
        {
            completed_count += 1;
            assert_eq!(transcript_delta.as_slice(), b"{\"step\":\"noop\"}\n");
        }
    }
    assert_eq!(completed_count, 3, "expected 3 ToolCallCompleted events");
}
