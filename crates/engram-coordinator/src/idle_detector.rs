//! ADR 0073 phase 4: THE idle detector — the coordinator's PG-derived
//! scanner, promoted from "L3 backstop" to the only detection plane.
//!
//! History: idle detection used to run twice — the host hub's
//! in-memory TTL scan (soft `Idle` announcements + hard any-event
//! silence, POSTed up as candidates) and this module's slow hard-TTL
//! backstop, which existed precisely because the hub copy is lossy
//! (detach/restart amnesia — prod 0782bea5 sat Active 8+ hours). The
//! durable event log answers the same question without the amnesia
//! class, and at the host tick's own cadence it matches the host
//! detector's latency. So the hub scan, the candidates POST, and the
//! backstop-vs-primary split are all deleted; what remains is this
//! one scanner with the host detector's EXACT semantics:
//!
//! - **soft TTL** (default 300s, `ENGRAM_IDLE_TTL_SECS`): newest event
//!   is `harness_idle` and older than the TTL — the agent finished a
//!   turn and nobody followed up. (An interleaving event replaces
//!   `harness_idle` as newest, which is the hub's clear-on-any-event
//!   rule by construction.)
//! - **hard TTL** (default 28800s = 8h, `ENGRAM_IDLE_HARD_TTL_SECS`):
//!   any-event silence — the backstop for adapters that never emit
//!   `Idle`.
//! - **shell pin**: sessions with `shell_pinned_until > now()` are
//!   never nominated — the human has a live browser shell open. The
//!   pin is a PG column stamped by the coordinator's own WS bridge
//!   (api/shell.rs) on its keepalive, which deletes the host-side
//!   refcount + renew RPCs + stale sweep (issue #219 class) outright:
//!   a dead bridge simply stops stamping and the pin expires.
//! - **disk-pressure brake** (ADR 0014 issue #4): a host whose
//!   reported free disk is under the floor has ALL its nominations
//!   held (an eviction writes a multi-GiB memory dump).
//! - **pressure-aware soft gating** (default OFF,
//!   `ENGRAM_IDLE_EVICT_PRESSURE_AWARE`): when on, soft candidates on
//!   hosts NOT under memory pressure stay resident; hard candidates
//!   always proceed. Reads the heartbeat-persisted
//!   `hosts.utilization` instead of host-local `/proc/meminfo` —
//!   same signal, one hop later. **These env knobs move to the
//!   COORDINATOR deployment with this change.**
//!
//! Mid-move sessions can't be nominated by construction: a session
//! mid-teleport/evac is not `Active`, and `transition_session`'s
//! legality CAS resolves any nomination race (the loser's Conflict is
//! a no-op). Epic #545 note: rung interlocks ("skip sessions already
//! descending") land HERE — this module exposes the single nomination
//! choke point (`nominate`).

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use engram_core::types::BindingDisposition;
use engram_core::types::SessionState;
use engram_core::HostId;

use crate::state::{SessionEvent, SharedState};

/// Soft TTL default — mirrors the retired host-side
/// `DEFAULT_IDLE_TTL_SECS` (ADR 0039 follow-up #20 rationale).
pub const DEFAULT_SOFT_TTL_SECS: u64 = 300;
/// Hard TTL default — the never-emits-Idle backstop.
pub const DEFAULT_HARD_TTL_SECS: u64 = 28_800;
/// Scan cadence — the retired host tick's cadence, so detection
/// latency is unchanged.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(10);
/// ADR 0014 issue #4 disk floor (bytes) under which a host's
/// nominations are held.
pub const DEFAULT_DISK_FLOOR_BYTES: u64 = 20 * 1024 * 1024 * 1024;
/// Free-RAM floor (percent) for pressure-aware soft gating.
pub const DEFAULT_MEM_FLOOR_PCT: u8 = 15;

#[derive(Clone, Debug)]
pub struct IdleDetectorConfig {
    pub poll_interval: Duration,
    pub soft_ttl: Duration,
    pub hard_ttl: Duration,
    pub disk_floor_bytes: u64,
    /// `ENGRAM_IDLE_EVICT_PRESSURE_AWARE` — default OFF (ships dark;
    /// epic #545 flips pressure-driven to the only mode).
    pub pressure_aware: bool,
    pub mem_floor_pct: u8,
}

impl Default for IdleDetectorConfig {
    fn default() -> Self {
        Self {
            poll_interval: DEFAULT_POLL_INTERVAL,
            soft_ttl: Duration::from_secs(DEFAULT_SOFT_TTL_SECS),
            hard_ttl: Duration::from_secs(DEFAULT_HARD_TTL_SECS),
            disk_floor_bytes: DEFAULT_DISK_FLOOR_BYTES,
            pressure_aware: false,
            mem_floor_pct: DEFAULT_MEM_FLOOR_PCT,
        }
    }
}

impl IdleDetectorConfig {
    /// Same env names the host detector used — the knobs move from the
    /// host DaemonSet to the coordinator deployment, values unchanged.
    pub fn from_env() -> Self {
        fn secs(var: &str, default: u64) -> Duration {
            Duration::from_secs(
                std::env::var(var)
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(default),
            )
        }
        Self {
            poll_interval: DEFAULT_POLL_INTERVAL,
            soft_ttl: secs("ENGRAM_IDLE_TTL_SECS", DEFAULT_SOFT_TTL_SECS),
            hard_ttl: secs("ENGRAM_IDLE_HARD_TTL_SECS", DEFAULT_HARD_TTL_SECS),
            disk_floor_bytes: std::env::var("ENGRAM_IDLE_EVICT_DISK_FLOOR_BYTES")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(DEFAULT_DISK_FLOOR_BYTES),
            pressure_aware: std::env::var("ENGRAM_IDLE_EVICT_PRESSURE_AWARE")
                .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
            mem_floor_pct: std::env::var("ENGRAM_IDLE_EVICT_MEM_FLOOR_PCT")
                .ok()
                .and_then(|s| s.parse::<u8>().ok())
                .unwrap_or(DEFAULT_MEM_FLOOR_PCT),
        }
    }
}

/// Why a candidate crossed the line. Mirrors the retired hub
/// `IdleKind` — hard candidates survive every gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdleKind {
    Soft,
    Hard,
}

pub fn spawn(cfg: IdleDetectorConfig, state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cfg.poll_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Skip the first immediate tick — right after a deploy the
        // event-append path may still be settling, and the shortest
        // TTL is minutes.
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = run_once(&cfg, &state).await {
                tracing::warn!(error = %e, "idle detector tick failed; will retry");
            }
        }
    })
}

/// One scan. `pub` so tests (and a future admin trigger) drive it
/// deterministically.
pub async fn run_once(
    cfg: &IdleDetectorConfig,
    state: &SharedState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let candidates = state
        .services
        .meta
        .list_idle_scan_candidates(cfg.soft_ttl.as_secs() as i64, cfg.hard_ttl.as_secs() as i64)
        .await?;
    if candidates.is_empty() {
        return Ok(());
    }

    // Per-host gates from the heartbeat-persisted utilization. Fail-open
    // directions mirror the retired host helpers exactly:
    // - disk brake: unmeasured → NO hold (never silently block all
    //   evictions on missing telemetry — `disk_pressure_check`);
    // - memory gate: unmeasured → PRESSURED (pressure-aware mode
    //   degrades to TTL-only eviction rather than silently pinning
    //   soft-idle sessions resident forever — `mem_pressure_from`,
    //   issue #540; its unit coverage moved into this module's tests).
    let mut disk_hold: HashMap<HostId, bool> = HashMap::new();
    let mut mem_pressured: HashMap<HostId, bool> = HashMap::new();
    if let Ok(hosts) = state.services.meta.list_active_hosts().await {
        for h in hosts {
            let u = &h.utilization;
            if u.disk_total_mib > 0 {
                let free_bytes = u.disk_total_mib.saturating_sub(u.disk_used_mib) * 1024 * 1024;
                disk_hold.insert(h.id, free_bytes < cfg.disk_floor_bytes);
            }
            if let Some(free_pct) =
                (u.mem_total_mib.saturating_sub(u.mem_used_mib) * 100).checked_div(u.mem_total_mib)
            {
                mem_pressured.insert(h.id, (free_pct as u8) < cfg.mem_floor_pct);
            }
        }
    }

    let now = state.services.clock.now_utc();
    for c in candidates {
        let kind = match classify(cfg, &c, now) {
            Some(k) => k,
            None => continue,
        };
        if let Some(host) = c.host_id {
            if *disk_hold.get(&host).unwrap_or(&false) {
                ::metrics::counter!(crate::metrics::IDLE_EVICT_DISK_PRESSURE_HOLDS_TOTAL)
                    .increment(1);
                tracing::warn!(
                    session_id = %c.session_id,
                    host_id = %host,
                    "idle-evict held: host disk under floor",
                );
                continue;
            }
            if cfg.pressure_aware
                && kind == IdleKind::Soft
                // A host absent from the map never heartbeated
                // utilization at all — fail open toward eviction.
                && !*mem_pressured.get(&host).unwrap_or(&true)
            {
                ::metrics::counter!(crate::metrics::IDLE_EVICT_KEPT_RESIDENT_TOTAL).increment(1);
                continue;
            }
        }
        nominate(state, &c, kind).await;
    }
    Ok(())
}

fn classify(
    cfg: &IdleDetectorConfig,
    c: &engram_core::traits::metadata::IdleScanCandidate,
    now: DateTime<Utc>,
) -> Option<IdleKind> {
    if c.shell_pinned_until.is_some_and(|t| t > now) {
        return None;
    }
    let last = c.last_event_at;
    let age = now.signed_duration_since(last).to_std().ok()?;
    if age >= cfg.hard_ttl {
        return Some(IdleKind::Hard);
    }
    if matches!(
        c.last_event_kind.as_deref(),
        Some("harness_idle" | "harness_parked")
    ) && age >= cfg.soft_ttl
    {
        return Some(IdleKind::Soft);
    }
    None
}

/// The single nomination choke point (`Active → Evicting` via the
/// legality CAS; the eviction scanner runs the pipeline).
async fn nominate(
    state: &SharedState,
    c: &engram_core::traits::metadata::IdleScanCandidate,
    kind: IdleKind,
) {
    match state
        .services
        .meta
        .transition_session(
            c.session_id,
            SessionState::Evicting,
            BindingDisposition::Retain,
        )
        .await
    {
        Ok(prev) => {
            let now = state.services.clock.now_utc();
            // ADR 0074 rung 1: the nomination is rung 1 — VM untouched,
            // cancellable (queued-op cancel + one CAS) until the evict
            // op's pipeline claims the session.
            if let Err(e) = state
                .services
                .meta
                .set_session_park_rung(c.session_id, 1, Some(now))
                .await
            {
                tracing::warn!(session_id = %c.session_id, error = %e, "park_rung stamp failed");
            }
            // ADR 0079: the nomination ENQUEUES the evict op; the op
            // executor drives the pipeline. Plain enqueue (no
            // idempotency key) — the eviction scanner's keyed backstop
            // dedups any later re-enqueue for this nomination. Failure
            // is benign: the scanner re-enqueues on its next sweep.
            if let Err(e) = crate::session_ops::enqueue(
                state,
                c.session_id,
                engram_core::types::session_op::OpKind::Evict,
                serde_json::json!({ "target": "idle", "allow_park": true, "nominated": true }),
                None,
            )
            .await
            {
                tracing::warn!(session_id = %c.session_id, error = %e,
                    "idle detector: evict op enqueue failed; the eviction scanner will re-enqueue");
            }
            ::metrics::counter!(
                crate::metrics::EVICTION_NOMINATED_TOTAL,
                "source" => "detector"
            )
            .increment(1);
            tracing::info!(
                session_id = %c.session_id,
                sandbox_id = ?c.sandbox_id,
                kind = ?kind,
                last_event_at = %c.last_event_at,
                "idle detector nominated session for eviction",
            );
            let _ = state
                .emit(
                    c.session_id,
                    SessionEvent::StatusChanged {
                        from: prev,
                        to: SessionState::Evicting,
                        at: now,
                    },
                )
                .await;
        }
        // Raced a concurrent transition (delete, resume-triggered
        // activity, a competing replica's scan) — benign.
        Err(engram_core::MetaError::Conflict(_)) => {}
        Err(e) => {
            tracing::warn!(
                session_id = %c.session_id,
                error = %e,
                "idle detector: transition to Evicting failed",
            );
        }
    }
}

#[cfg(test)]
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use engram_core::traits::metadata::IdleScanCandidate;
    use engram_core::{SandboxId, SessionId};

    /// Ported nomination coverage (the retired backstop + candidates-
    /// handler tests): a hard-idle Active session is flipped to
    /// Evicting via the legality CAS and the StatusChanged event lands;
    /// a session that raced to non-Active is a benign no-op.
    #[tokio::test]
    async fn run_once_nominates_hard_idle_active_session() {
        use crate::state::tests::build_state_for_session;
        let id = SessionId::new();
        let session = engram_core::types::Session {
            id,
            status: engram_core::types::SessionState::Active,
            host_id: None,
            sandbox_id: Some(SandboxId::new()),
            image: "test/repo:idle-detector".into(),
            mode: engram_core::types::session::SessionMode::Agent,
            created_at: Utc::now() - chrono::Duration::seconds(30_000),
            last_active_at: Utc::now() - chrono::Duration::seconds(30_000),
            last_event_at: None,
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        };
        let (state, mini, _local) = build_state_for_session(session);
        // No events at all → last_event_at falls back to created_at
        // (8h20m ago) → hard TTL (8h) crossed.
        run_once(&IdleDetectorConfig::default(), &state)
            .await
            .expect("run_once");
        assert_eq!(
            mini.session.lock().status,
            engram_core::types::SessionState::Evicting,
            "hard-idle Active session must be nominated",
        );
        let events = mini.events.lock();
        assert!(
            events.iter().any(|e| e.kind == "status_changed"),
            "nomination must emit StatusChanged",
        );
    }

    fn cand(
        now: DateTime<Utc>,
        age_secs: i64,
        kind: Option<&str>,
        pinned_in: Option<i64>,
    ) -> IdleScanCandidate {
        IdleScanCandidate {
            session_id: SessionId::new(),
            sandbox_id: Some(SandboxId::new()),
            host_id: None,
            last_event_at: now - chrono::Duration::seconds(age_secs),
            last_event_kind: kind.map(String::from),
            shell_pinned_until: pinned_in.map(|s| now + chrono::Duration::seconds(s)),
        }
    }

    fn cfg() -> IdleDetectorConfig {
        IdleDetectorConfig::default()
    }

    /// ADR 0039 follow-up #20 (moved here from the host-side module the
    /// ADR 0074 addendum retired): the soft TTL must never revert to an
    /// aggressive value that evicts interactive sessions during normal
    /// think-pauses, and must stay strictly below the hard ceiling so the
    /// soft path fires first for a genuinely abandoned session. Enforced
    /// at compile time.
    const _SOFT_TTL_NOT_AGGRESSIVE: () = {
        assert!(DEFAULT_SOFT_TTL_SECS >= 120);
        assert!(DEFAULT_SOFT_TTL_SECS < DEFAULT_HARD_TTL_SECS);
    };

    #[test]
    fn soft_fires_only_on_idle_kind_past_soft_ttl() {
        let now = Utc::now();
        assert_eq!(
            classify(&cfg(), &cand(now, 301, Some("harness_idle"), None), now),
            Some(IdleKind::Soft)
        );
        // Recent idle: not yet.
        assert_eq!(
            classify(&cfg(), &cand(now, 299, Some("harness_idle"), None), now),
            None
        );
        // A non-idle newest event = the hub's clear-on-any-event rule.
        assert_eq!(
            classify(&cfg(), &cand(now, 301, Some("agent_message"), None), now),
            None
        );
    }

    #[test]
    fn parked_soft_fires_only_past_soft_ttl() {
        let now = Utc::now();
        assert_eq!(
            classify(&cfg(), &cand(now, 301, Some("harness_parked"), None), now),
            Some(IdleKind::Soft)
        );
        assert_eq!(
            classify(&cfg(), &cand(now, 299, Some("harness_parked"), None), now),
            None
        );
    }

    #[test]
    fn hard_fires_on_any_kind_past_hard_ttl() {
        let now = Utc::now();
        assert_eq!(
            classify(&cfg(), &cand(now, 28_801, Some("agent_message"), None), now),
            Some(IdleKind::Hard)
        );
        assert_eq!(
            classify(&cfg(), &cand(now, 28_801, None, None), now),
            Some(IdleKind::Hard)
        );
    }

    /// Ported from the retired `idle_evictor::mem_pressure_from` unit
    /// tests (issue #540): the fail-open direction is TOWARD eviction.
    #[test]
    fn unmeasured_ram_reads_as_pressured() {
        // total==0 → checked_div None → pressured (degrade to TTL-only).
        let pressured = match (0u64.saturating_sub(0) * 100).checked_div(0u64) {
            Some(free_pct) => (free_pct as u8) < DEFAULT_MEM_FLOOR_PCT,
            None => true,
        };
        assert!(pressured);
        // Healthy host (64G total, 26G used → ~59% free) → not pressured.
        let pressured = match (64_304u64.saturating_sub(26_456) * 100).checked_div(64_304u64) {
            Some(free_pct) => (free_pct as u8) < DEFAULT_MEM_FLOOR_PCT,
            None => true,
        };
        assert!(!pressured);
    }

    #[test]
    fn live_shell_pin_suppresses_both_ttls() {
        let now = Utc::now();
        assert_eq!(
            classify(&cfg(), &cand(now, 301, Some("harness_idle"), Some(60)), now),
            None
        );
        assert_eq!(
            classify(&cfg(), &cand(now, 30_000, None, Some(60)), now),
            None
        );
        // An EXPIRED pin suppresses nothing — the bridge died and the
        // pin lapsed, exactly the issue #219 leak this design deletes.
        assert_eq!(
            classify(&cfg(), &cand(now, 30_000, None, Some(-60)), now),
            Some(IdleKind::Hard)
        );
    }
}
