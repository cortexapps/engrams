//! Heartbeat loop. Phase 1 keeps this in-process (the coordinator and
//! host run in the same process when started with --mode=all). Phase 3
//! switches the transport to tonic+gRPC; the wire format is in
//! `engram-protocol`.

use chrono::Utc;
use engram_core::SandboxId;
use engram_protocol::{Heartbeat, HostCapacityReport};

use crate::resource::CapacitySnapshot;

// ADR 0098 D1 carve-out: this is the in-process (`--mode=all`) heartbeat
// builder, exercised only by this module's tests; the production HTTP
// heartbeat is assembled in `lib.rs`. `sent_at` is display-only (the coord
// records its own authoritative `server_time`). Threading a clock here would
// only touch test call sites for no simulation benefit.
#[allow(clippy::disallowed_methods)]
pub fn build_heartbeat(
    host_id: engram_core::HostId,
    cap: &CapacitySnapshot,
    running_sandboxes: Vec<SandboxId>,
    draining: bool,
) -> Heartbeat {
    Heartbeat {
        host_id,
        sent_at: Utc::now(),
        capacity: HostCapacityReport {
            total_mib: cap.total_mib,
            used_mib: cap.used_mib,
            running_sandboxes: cap.running_sandboxes,
        },
        running_sandboxes,
        // In-process path: the caller already resolved the list
        // successfully, so the set is authoritative.
        running_sandboxes_known: true,
        draining,
        ready_images: Vec::new(),
        checkpoints: Vec::new(),
        // In-process (`--mode=all`) path doesn't probe observed
        // utilization; the HTTP heartbeat loop in `lib.rs` is the
        // real producer.
        utilization: Default::default(),
        // ADR 0084 (wire v15): no capture-job executor exists yet
        // (dormant P1a) — always empty until a later commit wires the
        // durable-record source.
        capture_job_reports: Vec::new(),
    }
}

/// Waits until the next heartbeat is due: the next `tick`, or earlier when
/// a checkpoint record lands. An early beat restarts the period. A signal
/// sent while no one waits is kept, so a record that lands during a send
/// gets the next beat at once.
pub async fn next_beat(
    tick: &mut tokio::time::Interval,
    checkpoint_recorded: &tokio::sync::Notify,
) {
    tokio::select! {
        biased;
        () = checkpoint_recorded.notified() => tick.reset(),
        _ = tick.tick() => {}
    }
}

#[cfg(test)]
mod tests {
    // tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use engram_core::HostId;

    #[test]
    fn build_heartbeat_copies_capacity_fields() {
        let host = HostId::new();
        let cap = CapacitySnapshot {
            total_mib: 65_536,
            used_mib: 12_288,
            running_sandboxes: 3,
        };
        let hb = build_heartbeat(host, &cap, Vec::new(), false);

        assert_eq!(hb.host_id, host);
        assert_eq!(hb.capacity.total_mib, 65_536);
        assert_eq!(hb.capacity.used_mib, 12_288);
        assert_eq!(hb.capacity.running_sandboxes, 3);
        assert!(!hb.draining);
        // Phase 1: the local snapshot field isn't populated by the
        // helper. It gets filled by the host agent before send.
        assert!(hb.running_sandboxes.is_empty());
    }

    #[test]
    fn build_heartbeat_propagates_drain_flag() {
        let cap = CapacitySnapshot::default();
        let hb = build_heartbeat(HostId::new(), &cap, Vec::new(), true);
        assert!(hb.draining);
    }

    #[test]
    fn build_heartbeat_preserves_running_sandboxes() {
        // ADR 0009 §2: this field is the input to the coord's reconcile
        // pass. The builder must propagate it through without mutation.
        let sandboxes = vec![SandboxId::new(), SandboxId::new(), SandboxId::new()];
        let hb = build_heartbeat(
            HostId::new(),
            &CapacitySnapshot::default(),
            sandboxes.clone(),
            false,
        );
        assert_eq!(hb.running_sandboxes, sandboxes);
    }

    #[test]
    fn sent_at_is_recent() {
        // We don't pin sent_at, but it must be roughly "now" — verifying
        // catches a bug where someone replaces Utc::now() with epoch 0.
        let before = chrono::Utc::now();
        let hb = build_heartbeat(
            HostId::new(),
            &CapacitySnapshot::default(),
            Vec::new(),
            false,
        );
        let after = chrono::Utc::now();
        assert!(hb.sent_at >= before);
        assert!(hb.sent_at <= after);
    }

    const PERIOD: std::time::Duration = std::time::Duration::from_secs(5);

    async fn started_interval() -> tokio::time::Interval {
        let mut tick = tokio::time::interval(PERIOD);
        tick.tick().await;
        tick
    }

    #[tokio::test(start_paused = true)]
    async fn next_beat_waits_one_period_without_a_record() {
        let mut tick = started_interval().await;
        let recorded = tokio::sync::Notify::new();
        let start = tokio::time::Instant::now();
        next_beat(&mut tick, &recorded).await;
        assert_eq!(start.elapsed(), PERIOD);
    }

    #[tokio::test(start_paused = true)]
    async fn next_beat_fires_when_a_record_lands_mid_period() {
        let mut tick = started_interval().await;
        let recorded = std::sync::Arc::new(tokio::sync::Notify::new());
        let signal = recorded.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            signal.notify_one();
        });
        let start = tokio::time::Instant::now();
        next_beat(&mut tick, &recorded).await;
        assert_eq!(start.elapsed(), std::time::Duration::from_secs(1));

        let start = tokio::time::Instant::now();
        next_beat(&mut tick, &recorded).await;
        assert_eq!(
            start.elapsed(),
            PERIOD,
            "the early beat restarts the period"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn next_beat_keeps_a_record_signalled_between_beats() {
        let mut tick = started_interval().await;
        let recorded = tokio::sync::Notify::new();
        recorded.notify_one();
        let start = tokio::time::Instant::now();
        next_beat(&mut tick, &recorded).await;
        assert_eq!(start.elapsed(), std::time::Duration::ZERO);
    }
}
