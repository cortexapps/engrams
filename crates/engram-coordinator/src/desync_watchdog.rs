//! Track A: harness-desync watchdog.
//!
//! The ADR 0034 idle backstop ([`crate::idle_detect_backstop`]) keys off
//! event *silence* (`MAX(session_events.created_at)`). That misses a whole
//! failure class the `bf3dbbcb` incident exposed: the harness event stream
//! desyncs from the run state machine, so the session sits `Active` without
//! ever reaching a clean idle resting state — a run-scoped event lands with
//! no open run (an `agent_message` after the run already closed), or a
//! `run_started` hangs with zero progress. The session is wedged, but the
//! backstop only reaps it 30 minutes after the *last* event, and a desync
//! that keeps trickling events can defeat it entirely.
//!
//! This scanner reads the durable record for the two desync *shapes*
//! (`MetadataStore::list_active_sessions_desynced`) rather than mere
//! silence, and — because the recovery is a NON-destructive harness
//! re-handshake (a live run just reconnects and re-emits, it cannot be
//! killed) — fires on a short TTL, well before the 30-minute backstop.
//!
//! Recovery is a non-destructive re-handshake ([`HostClient::rehandshake`]):
//! the harness drops + re-dials and re-emits `Idle`, resyncing the
//! coordinator's run state without touching the running agent. A successful
//! one advances `last_event_at` and the session leaves the flagged set; a
//! session whose `last_event_at` stays older than the *escalate* TTL (the
//! nudges aren't taking) falls through to the proven eviction → resume lane.
//!
//! The watchdog deliberately does NOT write a desync marker to
//! `session_events` (a coord-emitted event would bump `last_event_at` and
//! mask both its own re-detection and the backstop) — detection is metric +
//! WARN only; the activity log stays a pure harness record.
//!
//! [`HostClient::rehandshake`]: engram_core::traits::HostClient::rehandshake

use std::time::Duration;

use chrono::Utc;
use engram_core::traits::metadata::DesyncedSession;
use engram_core::types::SessionState;

use crate::state::{SessionEvent, SharedState};

/// Default stuck TTL. Short relative to the 30-min idle backstop: the
/// re-handshake recovery is non-destructive, so flagging a merely-quiet
/// session costs only a harmless reconnect — there's no eviction to
/// guard against with a long TTL.
pub const DEFAULT_STUCK_TTL_SECS: u64 = 300;

/// Default escalate TTL. Once a flagged session's `last_event_at` is this
/// old, the re-handshakes are deemed not to be taking (a successful one
/// re-emits `Idle` and bumps `last_event_at`, dropping the session out of
/// the flagged set), and the watchdog escalates to the eviction lane.
/// 3× the stuck TTL: several re-handshake attempts before giving up.
pub const DEFAULT_ESCALATE_TTL_SECS: u64 = 900;

/// How often the watchdog sweeps. A GROUP BY over Active sessions' events
/// like the backstop, but on a faster cadence since this is now the
/// primary wedge-recovery path.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct WatchdogConfig {
    pub poll_interval: Duration,
    /// A desync signature must persist (no newer event) this long before
    /// the watchdog acts (re-handshakes).
    pub stuck_ttl: Duration,
    /// Once a flagged session is older than this, escalate from
    /// re-handshake to the eviction lane. Must be > `stuck_ttl`.
    pub escalate_ttl: Duration,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            poll_interval: DEFAULT_POLL_INTERVAL,
            stuck_ttl: Duration::from_secs(DEFAULT_STUCK_TTL_SECS),
            escalate_ttl: Duration::from_secs(DEFAULT_ESCALATE_TTL_SECS),
        }
    }
}

impl WatchdogConfig {
    /// Read `ENGRAM_DESYNC_WATCHDOG_TTL_SECS` /
    /// `ENGRAM_DESYNC_WATCHDOG_ESCALATE_SECS`; falls through to the
    /// defaults. The poll interval is not operator-tunable.
    pub fn from_env() -> Self {
        let env_secs = |k: &str, default: u64| {
            std::env::var(k)
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(Duration::from_secs(default))
        };
        Self {
            stuck_ttl: env_secs("ENGRAM_DESYNC_WATCHDOG_TTL_SECS", DEFAULT_STUCK_TTL_SECS),
            escalate_ttl: env_secs(
                "ENGRAM_DESYNC_WATCHDOG_ESCALATE_SECS",
                DEFAULT_ESCALATE_TTL_SECS,
            ),
            ..Self::default()
        }
    }
}

/// Spawn the watchdog as a background task. Mirrors
/// [`crate::idle_detect_backstop::spawn`].
pub fn spawn(cfg: WatchdogConfig, state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cfg.poll_interval);
        // Skip the immediate first tick: let the event-append path settle
        // after a coord deploy.
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = run_once(&cfg, &state).await {
                tracing::warn!(error = %e, "desync watchdog tick failed; will retry");
            }
        }
    })
}

/// Single watchdog tick. Returns the number of sessions flagged this tick
/// (so tests can assert detection deterministically). `pub(crate)` so tests
/// can drive it without spawning the loop.
pub(crate) async fn run_once(
    cfg: &WatchdogConfig,
    state: &SharedState,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let flagged = state
        .services
        .meta
        .list_active_sessions_desynced(cfg.stuck_ttl.as_secs() as i64)
        .await?;
    for d in &flagged {
        // Detection signal (every flagged tick). A desync is a harness
        // protocol-invariant violation — a sustained rate is a real bug
        // until the streaming rewrite (ADR 0052) removes the EOF inference
        // that causes it.
        ::metrics::counter!(
            crate::metrics::HARNESS_DESYNC_DETECTED_TOTAL,
            "signature" => d.signature.clone(),
        )
        .increment(1);

        let age = Utc::now().signed_duration_since(d.last_event_at);
        let escalate = age.to_std().map(|a| a >= cfg.escalate_ttl).unwrap_or(false);

        if escalate {
            // The re-handshakes didn't take (a working one re-emits Idle and
            // bumps last_event_at, dropping the session out of this set).
            // Fall through to the proven eviction → resume recovery lane.
            match state
                .services
                .meta
                .transition_session(d.session_id, SessionState::Evicting)
                .await
            {
                Ok(prev) => {
                    ::metrics::counter!(
                        crate::metrics::EVICTION_NOMINATED_TOTAL,
                        "source" => "desync_watchdog",
                    )
                    .increment(1);
                    tracing::warn!(
                        session_id = %d.session_id,
                        sandbox_id = %d.sandbox_id,
                        signature = %d.signature,
                        age_secs = age.num_seconds(),
                        "desync watchdog escalating to eviction — re-handshakes did not settle \
                         the session within the escalate TTL",
                    );
                    let _ = state
                        .emit(
                            d.session_id,
                            SessionEvent::StatusChanged {
                                from: prev,
                                to: SessionState::Evicting,
                                at: Utc::now(),
                            },
                        )
                        .await;
                }
                // Raced the host/backstop nomination or a concurrent delete.
                Err(engram_core::MetaError::Conflict(_)) => {}
                Err(e) => tracing::warn!(
                    session_id = %d.session_id,
                    error = %e,
                    "desync watchdog: escalation transition failed",
                ),
            }
        } else {
            // Non-destructive recovery: nudge the harness to drop + re-dial
            // (re-emitting Idle), resyncing the coordinator's run state. A
            // live run is untouched; a settled session leaves the flagged
            // set next tick.
            tracing::warn!(
                session_id = %d.session_id,
                sandbox_id = %d.sandbox_id,
                signature = %d.signature,
                latest_kind = %d.latest_kind,
                "desync watchdog re-handshaking wedged Active session",
            );
            match state.services.host.rehandshake(d.sandbox_id).await {
                Ok(()) => {
                    ::metrics::counter!(crate::metrics::HARNESS_REHANDSHAKE_TOTAL).increment(1);
                }
                // `NotFound` (= `NotAttached`): no live vsock to nudge — the
                // harness link died without an EOF (its blocking read never
                // woke, the reconnect loop never fired), or the harness exited
                // and agentd, which only (re)spawns on a host `SpawnHarness`,
                // sat idle. The FC VM is likely still alive, so re-establish
                // the harness IN PLACE: re-issue the resume `start_agent`, which
                // drives agentd's ADR 0045 C1 reattach arm (SIGUSR1 a live
                // harness to re-dial, or respawn an exited one) — no
                // snapshot/destroy/restore. Only on a persistent failure does
                // the time-based escalation above fall through to teardown.
                Err(engram_core::SandboxError::NotFound) => {
                    match reattach_harness_in_place(state, d).await {
                        Ok(true) => {
                            ::metrics::counter!(crate::metrics::HARNESS_INPLACE_REATTACH_TOTAL)
                                .increment(1);
                            tracing::info!(
                                session_id = %d.session_id,
                                sandbox_id = %d.sandbox_id,
                                "desync watchdog re-attached harness in place (live VM, no teardown)",
                            );
                        }
                        // Nothing to reattach: the session moved off this
                        // sandbox since the scan, or no manifest bundle (dev-VM
                        // / process backend). Escalation catches a real wedge.
                        Ok(false) => tracing::debug!(
                            session_id = %d.session_id,
                            "desync watchdog: in-place reattach skipped; escalation will catch a persistent wedge",
                        ),
                        Err(e) => tracing::debug!(
                            session_id = %d.session_id,
                            error = %e,
                            "desync watchdog: in-place reattach failed; escalation will catch a persistent wedge",
                        ),
                    }
                }
                // A transient host error — the time-based escalation above
                // catches it if the session stays wedged.
                Err(e) => tracing::debug!(
                    session_id = %d.session_id,
                    error = %e,
                    "desync watchdog: re-handshake failed; escalation will catch a persistent wedge",
                ),
            }
        }
    }
    Ok(flagged.len())
}

/// ADR 0034 Track A in-place reattach. Thin wrapper over the shared
/// [`crate::api::snapshot::reattach_harness_in_place`] primitive (also used by
/// the inline delivery self-heal): re-establish a desynced session's harness on
/// its EXISTING live sandbox without a teardown. Called only when `rehandshake`
/// returned `NotFound` (dead vsock).
///
/// - `Ok(true)`  — reattach issued. The harness re-emits `Idle`, which bumps
///   `last_event_at` and drops the session out of the flagged set.
/// - `Ok(false)` — nothing to do: the session moved off `d.sandbox_id` since
///   the scan, or no manifest bundle (dev-VM / process backend).
/// - `Err(_)`    — host/meta error; the time-based escalation catches a
///   persistent wedge.
pub(crate) async fn reattach_harness_in_place(
    state: &SharedState,
    d: &DesyncedSession,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    crate::api::snapshot::reattach_harness_in_place(state, d.session_id, d.sandbox_id)
        .await
        .map_err(|e| format!("in-place harness reattach: {e}").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CoordinatorConfig;
    use crate::host_registry::HostRegistry;
    use crate::state::tests::MiniMeta;
    use crate::state::AppState;
    use crate::Services;
    use engram_cloud_mock::MockCloud;
    use engram_core::traits::metadata::DesyncedSession;
    use engram_core::traits::SandboxBackend;
    use engram_core::types::session::SessionMode;
    use engram_core::types::Session;
    use engram_sandbox_process::ProcessBackend;
    use engram_secrets_dev::InMemorySecretStore;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn build_state_and_meta(
        session: Session,
        root: &std::path::Path,
    ) -> (SharedState, Arc<MiniMeta>) {
        let local_path = root.join("local");
        std::fs::create_dir_all(&local_path).unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(root.join("sandboxes")));
        let meta = Arc::new(MiniMeta::new(session));
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        host_registry.register(
            engram_core::HostId::new(),
            Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(
                backend.clone(),
            )),
        );
        let services = Services {
            meta: meta.clone(),
            cloud: Arc::new(MockCloud::new()),
            host: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-blobs-test"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
                engram_storage_local::LocalBlobStorage::new(
                    std::env::temp_dir().join("engram-blobs-test"),
                ),
            )),
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
        };
        let cfg = CoordinatorConfig {
            local_path,
            ..CoordinatorConfig::default()
        };
        (
            Arc::new(AppState::new_with_registry(cfg, services, host_registry)),
            meta,
        )
    }

    fn active_session(id: engram_core::SessionId, sandbox: engram_core::SandboxId) -> Session {
        Session {
            id,
            status: engram_core::types::SessionState::Active,
            host_id: None,
            sandbox_id: Some(sandbox),
            image: "test/repo:desync".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
            selected_skills: Vec::new(),
        }
    }

    fn desynced(
        session_id: engram_core::SessionId,
        sandbox_id: engram_core::SandboxId,
        age: chrono::Duration,
    ) -> DesyncedSession {
        DesyncedSession {
            session_id,
            sandbox_id,
            latest_kind: "agent_message".into(),
            signature: "orphan_after_close".into(),
            last_event_at: chrono::Utc::now() - age,
        }
    }

    /// The incident shape, freshly past the stuck TTL: the watchdog flags it
    /// and takes the non-destructive recovery path — NOT escalation. The test
    /// host has no live harness, so `rehandshake` returns `NotFound`, which now
    /// routes to the in-place reattach (ADR 0034 Track A). With no manifest
    /// bundle in the test that's a graceful no-op, so the session stays Active
    /// (the reattach branch ran without escalating or erroring).
    #[tokio::test]
    async fn rehandshakes_recently_desynced_session() {
        let session_id = engram_core::SessionId::new();
        let sandbox_id = engram_core::SandboxId::new();
        let root = TempDir::new().unwrap();
        let (state, meta) =
            build_state_and_meta(active_session(session_id, sandbox_id), root.path());
        // Just past the stuck TTL, well within the escalate TTL.
        *meta.desynced.lock() = vec![desynced(
            session_id,
            sandbox_id,
            chrono::Duration::minutes(6),
        )];

        let n = run_once(&WatchdogConfig::default(), &state)
            .await
            .expect("watchdog tick");
        assert_eq!(n, 1, "the desynced session must be flagged");
        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(
            after.status,
            engram_core::types::SessionState::Active,
            "within the escalate TTL the watchdog recovers in place, not evicts",
        );
    }

    /// In-place reattach guard: a session that has moved off the sandbox the
    /// watchdog flagged (a concurrent eviction/resume re-bound it) must be a
    /// no-op — we must never re-issue `start_agent` against a stale sandbox.
    #[tokio::test]
    async fn reattach_in_place_skips_a_moved_session() {
        let session_id = engram_core::SessionId::new();
        let flagged_sandbox = engram_core::SandboxId::new();
        let moved_to_sandbox = engram_core::SandboxId::new();
        let root = TempDir::new().unwrap();
        // The live session is now bound to a DIFFERENT sandbox than the one the
        // desync record names.
        let (state, _meta) =
            build_state_and_meta(active_session(session_id, moved_to_sandbox), root.path());
        let d = desynced(session_id, flagged_sandbox, chrono::Duration::minutes(6));

        assert!(
            !reattach_harness_in_place(&state, &d).await.unwrap(),
            "a session re-bound to another sandbox must not be reattached in place",
        );
    }

    /// A session whose re-handshakes never took (last_event_at older than
    /// the escalate TTL) is escalated into the eviction lane.
    #[tokio::test]
    async fn escalates_persistently_wedged_session() {
        let session_id = engram_core::SessionId::new();
        let sandbox_id = engram_core::SandboxId::new();
        let root = TempDir::new().unwrap();
        let (state, meta) =
            build_state_and_meta(active_session(session_id, sandbox_id), root.path());
        // Past the escalate TTL (default 15m).
        *meta.desynced.lock() = vec![desynced(
            session_id,
            sandbox_id,
            chrono::Duration::minutes(20),
        )];

        run_once(&WatchdogConfig::default(), &state)
            .await
            .expect("watchdog tick");
        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(
            after.status,
            engram_core::types::SessionState::Evicting,
            "a persistently wedged session must be escalated to the eviction lane",
        );
    }

    /// A clean session (nothing desynced) is left alone.
    #[tokio::test]
    async fn ignores_healthy_session() {
        let session_id = engram_core::SessionId::new();
        let sandbox_id = engram_core::SandboxId::new();
        let root = TempDir::new().unwrap();
        let (state, _meta) =
            build_state_and_meta(active_session(session_id, sandbox_id), root.path());

        let n = run_once(&WatchdogConfig::default(), &state)
            .await
            .expect("watchdog tick");
        assert_eq!(n, 0, "no desync → nothing flagged");
    }

    #[test]
    fn config_from_env_shape() {
        let def = WatchdogConfig::default();
        assert_eq!(def.stuck_ttl, Duration::from_secs(300));
        assert_eq!(def.escalate_ttl, Duration::from_secs(900));
        assert_eq!(def.poll_interval, Duration::from_secs(30));
    }
}
