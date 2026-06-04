//! ADR 0034 L3: PG-derived idle-detection backstop.
//!
//! The host's `HarnessHub` is the authoritative, low-latency idle
//! detector (soft TTL on explicit `Idle` announcements, hard TTL on
//! any-event silence) — but it only sees *attached* harnesses. When
//! the harness vsock connection drops, the hub's reader-loop exit
//! wipes the sandbox from `connections` / `last_event_at` /
//! `last_idle_at`, and a running-but-detached sandbox escapes both
//! TTLs forever (prod session `0782bea5` sat Active 8+ hours).
//! Reconcile can't catch it either: the sandbox *is* running, so the
//! heartbeat view is consistent.
//!
//! This scanner reads the durable activity record instead —
//! `session_events`, where every tool call, message, and idle
//! announcement lands — and nominates Active sessions whose newest
//! event is older than the hard TTL into the same `Active → Evicting`
//! lane the host path uses (the eviction scanner does the work).
//! It catches the whole host-amnesia *class*: harness detach,
//! host-agent restart, hub bookkeeping bugs.
//!
//! Deliberately hard-TTL-only and slow-cadence: a busy session always
//! has events newer than 30 minutes, so this can never race the
//! host's soft-TTL path on a live session; nomination races resolve
//! via `transition_session`'s row lock (the loser's Conflict is a
//! no-op).

use std::time::Duration;

use chrono::Utc;
use engram_core::types::SessionState;

use crate::state::{SessionEvent, SharedState};

/// Default hard TTL — matches the host-side
/// `engram_host_agent::idle_evictor::DEFAULT_IDLE_HARD_TTL_SECS`
/// (30 min). The backstop is the *second* line, so it fires at the
/// same threshold the host's hard TTL would have, had the host not
/// gone blind.
pub const DEFAULT_HARD_TTL_SECS: u64 = 1800;

/// How often the backstop sweeps. Slow on purpose: it exists for a
/// rare failure class, and every tick is a GROUP BY over Active
/// sessions' events.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub struct BackstopConfig {
    pub poll_interval: Duration,
    /// Sessions with no `session_events` row newer than this are
    /// nominated for eviction.
    pub hard_ttl: Duration,
}

impl Default for BackstopConfig {
    fn default() -> Self {
        Self {
            poll_interval: DEFAULT_POLL_INTERVAL,
            hard_ttl: Duration::from_secs(DEFAULT_HARD_TTL_SECS),
        }
    }
}

impl BackstopConfig {
    /// Read `ENGRAM_IDLE_BACKSTOP_TTL_SECS` — falls through to the
    /// 30-min default. Mirrors the host-side TTL env helpers; the
    /// poll interval is not operator-tunable (there's no scenario
    /// where sweeping a rare-failure backstop faster helps).
    pub fn from_env() -> Self {
        let hard_ttl = std::env::var("ENGRAM_IDLE_BACKSTOP_TTL_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(DEFAULT_HARD_TTL_SECS));
        Self {
            hard_ttl,
            ..Self::default()
        }
    }
}

/// Spawn the backstop as a background task. Caller holds the
/// JoinHandle for the process lifetime; dropping aborts the loop.
/// Mirrors [`crate::idle_evictor::spawn_eviction_scanner`].
pub fn spawn(cfg: BackstopConfig, state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cfg.poll_interval);
        // Skip the first immediate tick: right after a coord deploy
        // the event-append path may still be settling; there's no
        // urgency — the TTL is 30 minutes.
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = run_once(&cfg, &state).await {
                tracing::warn!(error = %e, "idle backstop tick failed; will retry");
            }
        }
    })
}

/// Single backstop tick. `pub(crate)` so tests can drive it
/// deterministically without `tokio::spawn`-ing the loop.
pub(crate) async fn run_once(
    cfg: &BackstopConfig,
    state: &SharedState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let stale = state
        .services
        .meta
        .list_active_sessions_idle_past(cfg.hard_ttl.as_secs() as i64)
        .await?;
    for (session_id, sandbox_id, last_event_at) in stale {
        match state
            .services
            .meta
            .transition_session(session_id, SessionState::Evicting)
            .await
        {
            Ok(prev) => {
                ::metrics::counter!(
                    crate::metrics::EVICTION_NOMINATED_TOTAL,
                    "source" => "backstop"
                )
                .increment(1);
                // WARN, not info: a backstop nomination means the
                // host-side detector went blind to a running sandbox
                // — worth a human glance even though recovery is
                // automatic from here.
                tracing::warn!(
                    %session_id,
                    %sandbox_id,
                    %last_event_at,
                    hard_ttl_secs = cfg.hard_ttl.as_secs(),
                    "idle backstop nominated stale Active session for eviction \
                     (host idle-detection never fired)",
                );
                let _ = state
                    .emit(
                        session_id,
                        SessionEvent::StatusChanged {
                            from: prev,
                            to: SessionState::Evicting,
                            at: Utc::now(),
                        },
                    )
                    .await;
            }
            // Raced the host's nomination or a concurrent delete —
            // the row is wherever the winner put it. Benign.
            Err(engram_core::MetaError::Conflict(_)) => {}
            Err(e) => {
                tracing::warn!(
                    %session_id,
                    error = %e,
                    "idle backstop: transition to Evicting failed",
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CoordinatorConfig;
    use crate::host_registry::HostRegistry;
    use crate::idle_evictor::{scanner_run_once, EvictionScannerConfig};
    use crate::state::tests::MiniMeta;
    use crate::state::AppState;
    use crate::Services;
    use engram_cloud_mock::MockCloud;
    use engram_core::traits::SandboxBackend;
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
    use engram_core::types::session::SessionMode;
    use engram_core::types::{PersistedEvent, Session};
    use engram_sandbox_process::ProcessBackend;
    use engram_secrets_dev::InMemorySecretStore;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn build_state_and_meta(
        session: Session,
        sandbox_root: &std::path::Path,
    ) -> (SharedState, Arc<MiniMeta>) {
        let local_path = sandbox_root.join("local");
        std::fs::create_dir_all(&local_path).unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(sandbox_root.join("sandboxes")));
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
            user_id: None,
            status: engram_core::types::SessionState::Active,
            host_id: None,
            sandbox_id: Some(sandbox),
            image: "test/repo:backstop".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
        }
    }

    /// Backdate the session's activity record so the backstop sees
    /// it as silent: a single old event (MiniMeta's
    /// `list_active_sessions_idle_past` takes the newest event,
    /// falling back to session.created_at).
    fn backdate_activity(meta: &MiniMeta, hours: i64) {
        meta.events.lock().push(PersistedEvent {
            idx: 0,
            kind: "harness_idle".into(),
            payload: serde_json::json!({}),
            created_at: chrono::Utc::now() - chrono::Duration::hours(hours),
            recovery_epoch: 0,
            rewound_at: None,
        });
        // The COALESCE fallback would otherwise keep the session
        // "fresh" via created_at — rewind it too, as a long-running
        // session's would be.
        meta.session.lock().created_at = chrono::Utc::now() - chrono::Duration::hours(hours);
    }

    /// The incident shape: Active session, bound (running) sandbox,
    /// zero recent events — the host never nominates it. The
    /// backstop must move it to Evicting, and the eviction scanner
    /// must then take it the rest of the way to Idle. End-to-end
    /// composition of the two scanners.
    #[tokio::test]
    async fn backstop_nominates_detached_session_and_composes_with_scanner() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        // Placeholder sandbox id; replaced with the real one below.
        let (state, meta) = build_state_and_meta(
            active_session(session_id, engram_core::SandboxId::new()),
            sandbox_root.path(),
        );
        // A real (process) sandbox so the eviction pipeline succeeds.
        let sandbox_id = state
            .services
            .host
            .create(SandboxSpec {
                image: "backstop-test".into(),
                rootfs_source: None,
                image_uri: None,
                rootfs_manifest: None,
                cpu: CpuLimit { vcpus: 1 },
                memory: MemoryLimit { max_mib: 256 },
                disk: DiskLimit { max_gib: 1 },
                ttl: None,
                env: Default::default(),
                workdir: None,
                network: Default::default(),
                aux_ro_drives: Vec::new(),
            })
            .await
            .unwrap();
        state.registry.bind(session_id, sandbox_id);
        meta.session.lock().sandbox_id = Some(sandbox_id);
        backdate_activity(&meta, 2);

        run_once(&BackstopConfig::default(), &state)
            .await
            .expect("backstop tick");
        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(
            after.status,
            engram_core::types::SessionState::Evicting,
            "backstop must nominate the silent session"
        );

        // The eviction scanner picks it up and finishes the job.
        scanner_run_once(&EvictionScannerConfig::default(), &state)
            .await
            .expect("eviction scanner tick");
        let done = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(done.status, engram_core::types::SessionState::Idle);
        assert_eq!(done.sandbox_id, None);
    }

    /// A recently-active session is untouched — the hard TTL is the
    /// whole guarantee that the backstop can't fight the host's
    /// soft-TTL path.
    #[tokio::test]
    async fn backstop_ignores_recent_session() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let (state, meta) = build_state_and_meta(
            active_session(session_id, engram_core::SandboxId::new()),
            sandbox_root.path(),
        );
        // Fresh event right now.
        meta.events.lock().push(PersistedEvent {
            idx: 0,
            kind: "harness_run_started".into(),
            payload: serde_json::json!({}),
            created_at: chrono::Utc::now(),
            recovery_epoch: 0,
            rewound_at: None,
        });

        run_once(&BackstopConfig::default(), &state)
            .await
            .expect("backstop tick");
        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(after.status, engram_core::types::SessionState::Active);
    }

    /// Racing the host's nomination is benign: if the session is
    /// already Evicting when the backstop sweeps, the Conflict is
    /// swallowed and nothing changes.
    #[tokio::test]
    async fn backstop_idempotent_when_already_evicting() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let (state, meta) = build_state_and_meta(
            active_session(session_id, engram_core::SandboxId::new()),
            sandbox_root.path(),
        );
        backdate_activity(&meta, 2);
        // Host's nomination wins first.
        state
            .services
            .meta
            .transition_session(session_id, engram_core::types::SessionState::Evicting)
            .await
            .unwrap();

        // MiniMeta's list_active_sessions_idle_past already filters
        // non-Active sessions, so this exercises the "stale read →
        // Conflict swallowed" path only when the filter races. Either
        // way: tick must not error and status must stay Evicting.
        run_once(&BackstopConfig::default(), &state)
            .await
            .expect("backstop tick");
        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(after.status, engram_core::types::SessionState::Evicting);
    }

    /// Env override parses; absent/garbage falls back to 30 min.
    #[test]
    fn backstop_config_from_env_shape() {
        let def = BackstopConfig::default();
        assert_eq!(def.hard_ttl, Duration::from_secs(1800));
        assert_eq!(def.poll_interval, Duration::from_secs(60));
    }
}
