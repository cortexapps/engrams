//! HTTP API + scheduler for Engram.
//!
//! Phase 1 surface: `POST /sessions`, `POST /sessions/:id/exec`,
//! `DELETE /sessions/:id`, plus snapshot/restore stubs and a healthz.
//! Multiple coordinator replicas are supported by virtue of all state
//! living in Postgres; this binary itself is stateless.

use std::sync::Arc;

use engram_core::traits::{CloudBackend, MetadataStore, SandboxBackend, SecretStore};

pub mod api;
pub mod config;
pub mod dead_host;
pub mod error;
pub mod git_workdir;
pub mod harness_registry;
pub mod harness_substrate;
pub mod host_registry;
pub mod idle_evictor;
pub mod image_registry;
pub mod pg_listener;
pub mod preemption_drain;
pub mod scheduler;
pub mod state;
pub mod workspace;

pub use config::CoordinatorConfig;
pub use error::ApiError;
pub use host_registry::HostRegistry;
pub use state::AppState;

/// Container for the wired-up dependencies. Built once at startup;
/// passed by `Arc<AppState>` into the axum router.
pub struct Services {
    pub meta: Arc<dyn MetadataStore>,
    pub cloud: Arc<dyn CloudBackend>,
    pub sandbox: Arc<dyn SandboxBackend>,
    pub secrets: Arc<dyn SecretStore>,
    pub images: image_registry::ImageRegistry,
    /// Host-side harness registry — the closed set of
    /// `HarnessSpec::Builtin{name}` values a session may request.
    /// Resolved from `cfg.harnesses_dir` at startup; visible inside
    /// every sandbox at `/run/engram/harnesses` via the substrate
    /// below.
    pub harnesses: Arc<harness_registry::HarnessRegistry>,
    /// Read-only ext4 image of `cfg.harnesses_dir`, attached as the
    /// second virtio-blk drive on every sandbox. `None` means "no
    /// harnesses to make available" (empty registry, or substrate
    /// build failed at startup — logged but non-fatal).
    pub harness_substrate: Option<harness_substrate::Substrate>,
}

/// Bootstrap the axum server. Returns once the bind future yields.
pub async fn run(cfg: CoordinatorConfig, services: Services) -> Result<(), CoordinatorError> {
    run_with_registry(cfg, services, Arc::new(HostRegistry::new())).await
}

/// Variant of [`run`] that takes an externally-built [`HostRegistry`].
/// `main.rs` uses this so it can pre-register a local backend in
/// `--mode=all` before any HTTP routes accept traffic.
pub async fn run_with_registry(
    cfg: CoordinatorConfig,
    services: Services,
    host_registry: Arc<HostRegistry>,
) -> Result<(), CoordinatorError> {
    let meta_for_listener = services.meta.clone();
    let state = Arc::new(AppState::new_with_registry(
        cfg.clone(),
        services,
        host_registry,
    ));

    // Phase 3 follow-up: rebuild in-memory routing maps from
    // sessions persisted in Postgres. After a coordinator restart
    // the SandboxRegistry (session_id → sandbox_id) and
    // HostRegistry.sandbox_owner (sandbox_id → host_id) are empty,
    // so any existing Active session would get "no live sandbox"
    // until reseed. Best-effort — failure here just means existing
    // sessions need /resume after a restart, which they would
    // anyway in some failure modes.
    if let Err(e) = repopulate_routing(&state).await {
        tracing::warn!(error = %e, "routing-map repopulate at startup failed");
    }

    // Phase 3c HA: every replica subscribes to the shared
    // `session_events` channel so SSE clients connected to any one
    // replica see events emitted via any other. The same listener
    // also handles `host_dead` notifications (Phase 3d follow-up) so
    // every replica drops its in-memory `HostRegistry` entry when
    // any replica wins the dead-host race. Drops the JoinHandle —
    // the task lives for the coordinator's lifetime and exits when
    // axum::serve returns (process shutdown).
    let _pg_listener = pg_listener::spawn(
        cfg.database_url.clone(),
        meta_for_listener.clone(),
        state.events.clone(),
        state.host_registry.clone(),
    );

    // Phase 3d follow-up: dead-host auto-detector. Opens its own
    // PgPool for advisory locks (the trait doesn't expose one;
    // sharing a connection between the trait and lock-holding code
    // would tangle the abstraction). On Postgres-backed deployments
    // a stale heartbeat triggers eviction within ~poll_interval +
    // stale_threshold; deployments using a non-Postgres MetadataStore
    // will see this task fail to connect and log the error — they
    // can still use `POST /sessions/:id/migrate` for operator-
    // initiated transitions.
    let _dead_host = match sqlx::postgres::PgPool::connect(&cfg.database_url).await {
        Ok(pool) => Some(dead_host::spawn(
            dead_host::DeadHostConfig::default(),
            pool,
            meta_for_listener,
            state.host_registry.clone(),
            state.events.clone(),
        )),
        Err(e) => {
            tracing::warn!(error = %e, "dead-host detector disabled — couldn't open PgPool");
            None
        }
    };

    // Phase 4 Track B: idle-session evictor. Polls the harness hub
    // every ~10s for sandboxes whose last harness event is older
    // than `ENGRAM_IDLE_TTL_SECS` (default 60s) and runs the suspend
    // pipeline (checkpoint → FC snapshot → destroy → mark Idle).
    // Auto-resume on next request lands on the existing /resume path.
    // Drops the JoinHandle — task lives for coord's lifetime.
    let _idle_evictor = idle_evictor::spawn(
        state.clone(),
        idle_evictor::idle_ttl_from_env(),
        idle_evictor::idle_hard_ttl_from_env(),
        idle_evictor::DEFAULT_POLL_INTERVAL,
    );

    // Phase 4 Track D: preemption best-effort drain. Subscribes to
    // `cloud.preemption_signal()` (engram-cloud-gcp polls the GCE
    // metadata server, MockCloud's `trigger_preemption` for tests)
    // and on notice fans out across all active sessions on this
    // host: workspace checkpoint → destroy → mark Dead.
    // Caller-driven recovery via `POST /sessions/:id/resume`.
    let _preemption_drain = preemption_drain::spawn(state.clone());

    // Demo wiring: bind the harness-channel TCP listener so
    // `SandboxSpec::agent`-spawned harnesses (today: the dev
    // `engram-harness-noop`) can dial the hub. Off the critical
    // path for sessions that don't declare an agent.
    let (harness_addr, _harness_listener) = engram_host_agent::harness::spawn_tcp_listener(
        (*state.harness_hub).clone(),
        cfg.harness_listen_addr,
    )
    .await
    .map_err(CoordinatorError::Io)?;
    *state.harness_listen_addr.lock() = Some(harness_addr);

    // Firecracker path: register a HarnessSink on the sandbox
    // backend so per-VM vsock UDS accepts feed the same HarnessHub
    // as the TCP listener above. ProcessBackend's default no-op
    // is fine — its harness binary dials TCP loopback directly.
    {
        let hub = state.harness_hub.clone();
        let sink: engram_core::traits::HarnessSink = std::sync::Arc::new(move |stream| {
            hub.accept_via_session_lookup(stream);
        });
        state.services.sandbox.set_harness_sink(sink);
    }

    let app = api::router(state.clone());
    let listener = tokio::net::TcpListener::bind(cfg.bind_addr.as_str())
        .await
        .map_err(CoordinatorError::Io)?;
    tracing::info!(addr = %cfg.bind_addr, "coordinator listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(CoordinatorError::Io)
}

/// Read every active session and re-bind its `sandbox_id`/`host_id`
/// in the in-memory maps so a coordinator restart doesn't leave
/// `Active` sessions stranded. Pre-populates `HostRegistry`'s
/// `sandbox_owner` with the persisted `sandbox_id → host_id` pairs;
/// once the host dials back in via `/api/hosts/connect` and registers
/// its backend, routing resumes for those sessions without further
/// intervention. Sessions in `Pending` (sandbox not created yet),
/// `Idle` (evicted), or `Dead` (awaiting reschedule) are
/// left for `/resume` to handle on next access.
async fn repopulate_routing(state: &AppState) -> Result<(), engram_core::MetaError> {
    let sessions = state.services.meta.list_active_sessions().await?;
    let mut bound = 0usize;
    for s in sessions {
        if let (Some(sandbox_id), Some(host_id)) = (s.sandbox_id, s.host_id) {
            state.registry.bind(s.id, sandbox_id);
            state
                .host_registry
                .record_sandbox_owner(sandbox_id, host_id);
            bound += 1;
        }
    }
    if bound > 0 {
        tracing::info!(
            sessions = bound,
            "rebuilt in-memory routing for active sessions",
        );
    }
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("coordinator received ctrl-c, shutting down");
}

#[derive(Debug)]
pub enum CoordinatorError {
    Config(String),
    Io(std::io::Error),
}

impl std::fmt::Display for CoordinatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(msg) => write!(f, "coordinator config error: {msg}"),
            Self::Io(e) => write!(f, "coordinator io error: {e}"),
        }
    }
}

impl std::error::Error for CoordinatorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}
