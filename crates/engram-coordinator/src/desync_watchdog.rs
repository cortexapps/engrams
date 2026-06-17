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
//! This commit is detection-only (metric + WARN). The re-handshake
//! recovery + escalation-after-N lands in the following commits; the
//! watchdog deliberately does NOT write to `session_events` (a coord-emitted
//! marker would bump `last_event_at` and mask both its own re-detection and
//! the backstop), so the activity log stays a pure harness record.

use std::time::Duration;

use crate::state::SharedState;

/// Default stuck TTL. Short relative to the 30-min idle backstop: the
/// re-handshake recovery is non-destructive, so flagging a merely-quiet
/// session costs only a harmless reconnect — there's no eviction to
/// guard against with a long TTL.
pub const DEFAULT_STUCK_TTL_SECS: u64 = 300;

/// How often the watchdog sweeps. A GROUP BY over Active sessions' events
/// like the backstop, but on a faster cadence since this is now the
/// primary wedge-recovery path.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct WatchdogConfig {
    pub poll_interval: Duration,
    /// A desync signature must persist (no newer event) this long before
    /// the watchdog acts.
    pub stuck_ttl: Duration,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            poll_interval: DEFAULT_POLL_INTERVAL,
            stuck_ttl: Duration::from_secs(DEFAULT_STUCK_TTL_SECS),
        }
    }
}

impl WatchdogConfig {
    /// Read `ENGRAM_DESYNC_WATCHDOG_TTL_SECS`; falls through to the 5-min
    /// default. The poll interval is not operator-tunable.
    pub fn from_env() -> Self {
        let stuck_ttl = std::env::var("ENGRAM_DESYNC_WATCHDOG_TTL_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(DEFAULT_STUCK_TTL_SECS));
        Self {
            stuck_ttl,
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
        ::metrics::counter!(
            crate::metrics::HARNESS_DESYNC_DETECTED_TOTAL,
            "signature" => d.signature.clone(),
        )
        .increment(1);
        // WARN: a desync is a harness protocol-invariant violation. Until
        // the streaming rewrite (ADR 0052) removes the EOF inference that
        // causes it, every hit is worth a human glance — recovery (next
        // commit) is automatic, but a sustained rate is a real bug.
        tracing::warn!(
            session_id = %d.session_id,
            sandbox_id = %d.sandbox_id,
            signature = %d.signature,
            latest_kind = %d.latest_kind,
            stuck_ttl_secs = cfg.stuck_ttl.as_secs(),
            "desync watchdog flagged wedged Active session \
             (harness event stream desynced from run state machine)",
        );
        // Commit 3/4: recover via a non-destructive harness re-handshake
        // (host.rehandshake → SIGUSR1 → vsock re-dial → re-emit state),
        // escalating to the eviction lane after N failed nudges.
    }
    Ok(flagged.len())
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
        }
    }

    /// The incident shape: an Active session the watchdog must flag.
    #[tokio::test]
    async fn flags_desynced_active_session() {
        let session_id = engram_core::SessionId::new();
        let sandbox_id = engram_core::SandboxId::new();
        let root = TempDir::new().unwrap();
        let (state, meta) =
            build_state_and_meta(active_session(session_id, sandbox_id), root.path());
        *meta.desynced.lock() = vec![DesyncedSession {
            session_id,
            sandbox_id,
            latest_kind: "agent_message".into(),
            signature: "orphan_after_close".into(),
        }];

        let n = run_once(&WatchdogConfig::default(), &state)
            .await
            .expect("watchdog tick");
        assert_eq!(n, 1, "the desynced session must be flagged");
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
        assert_eq!(def.poll_interval, Duration::from_secs(30));
    }
}
