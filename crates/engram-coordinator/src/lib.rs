//! HTTP API + scheduler for Engram.
//!
//! Phase 1 surface: `POST /sessions`, `POST /sessions/:id/exec`,
//! `DELETE /sessions/:id`, plus snapshot/restore stubs and a healthz.
//! Multiple coordinator replicas are supported by virtue of all state
//! living in Postgres; this binary itself is stateless.

use std::sync::Arc;

use engram_core::traits::{BlobStorage, CloudBackend, HostClient, MetadataStore, SecretStore};

pub mod api;
pub mod blob;
pub mod chunk_gc;
pub mod config;
pub mod cow_state;
pub mod dead_host;
pub mod error;
pub mod evacuation;
pub mod harness_paths;
pub mod host_registry;
pub mod idle_evictor;
pub mod metrics;
pub mod nbd_loss_trigger;
pub mod pg_listener;
pub mod preemption_drain;
pub mod reconcile;
pub mod scheduler;
pub mod state;

pub use config::CoordinatorConfig;
pub use error::ApiError;
pub use host_registry::HostRegistry;
pub use state::AppState;

/// Container for the wired-up dependencies. Built once at startup;
/// passed by `Arc<AppState>` into the axum router.
pub struct Services {
    pub meta: Arc<dyn MetadataStore>,
    pub cloud: Arc<dyn CloudBackend>,
    pub host: Arc<dyn HostClient>,
    /// ADR 0013: coord-side gRPC channel pool keyed by `HostId`.
    /// Populated on host registration (POST `/api/hosts/register`)
    /// and pre-warmed at coord startup from `hosts.host_addr`.
    /// Used by `HostRegistry`'s dispatch path to send unary +
    /// streaming RPCs to host-agents over HTTP/2.
    pub host_pool: Arc<engram_protocol::grpc_pool::GrpcHostPool>,
    pub secrets: Arc<dyn SecretStore>,
    /// Master key provider used to wrap/unwrap registry-credential
    /// DEKs. Initialised from `--kek-provider`. Phase 5+; envelope-
    /// encrypted creds live in the `registry_credentials` table.
    pub kek: Arc<dyn engram_crypto::MasterKeyProvider>,
    /// OCI client for pulling registry artifacts. Used by
    /// `/api/enabled-images` POST/refresh to fetch the manifest.toml
    /// at enable time so session-create has zero registry I/O. Shared
    /// with the host-agent's `image_cache` in `--mode=all` so one
    /// auth-resolver cache backs both.
    pub oci: Arc<engram_oci::OciClient>,
    /// Same resolver the `oci` client was built with, surfaced
    /// separately so the host-side `ConnectedHost` request handler
    /// (ADR 0007: `ResolveRegistryAuth`) can call it directly when a
    /// standalone host-agent asks for creds. Coord process boundary
    /// — credentials never leave this domain except over the WS at
    /// pull time.
    pub auth_resolver: Arc<dyn engram_oci::RegistryAuthResolver>,
    /// Cold-tier blob storage (ADR 0005). Used by the flush primitive
    /// (Stage 5) and the cross-host cold-resume path (Stage 6).
    /// Selected at boot via `ENGRAM_BLOB_BACKEND={local,gcs}`;
    /// defaults to `local` so `just dev` works without cloud creds.
    pub blob: Arc<dyn BlobStorage>,
    /// ADR 0007: chunked-storage primitive layered on top of the
    /// same `blob`. Shared between the `--mode=all` host-agent's
    /// `PooledBackend` (materialize-on-create) and the coord's
    /// admin GC endpoint (sweep unreferenced chunks).
    pub chunk_store: engram_chunk_store::ChunkStore,
    /// ADR 0007: where the in-process host-agent (active in
    /// `--mode=all`) materializes chunked manifests. `Some` when
    /// running `--mode=all`; `None` in `--mode=coordinator` (the
    /// admin reaper endpoint then becomes a multi-host fanout —
    /// out of scope for this slice).
    pub materialize_dir: Option<std::path::PathBuf>,
}

/// Bootstrap the axum server. Returns once the bind future yields.
pub async fn run(cfg: CoordinatorConfig, services: Services) -> Result<(), CoordinatorError> {
    let registry = Arc::new(HostRegistry::new(services.meta.clone()));
    run_with_registry(cfg, services, registry).await
}

/// Variant of [`run`] that takes an externally-built [`HostRegistry`].
/// `main.rs` uses this so it can pre-register a local backend in
/// `--mode=all` before any HTTP routes accept traffic.
pub async fn run_with_registry(
    cfg: CoordinatorConfig,
    services: Services,
    host_registry: Arc<HostRegistry>,
) -> Result<(), CoordinatorError> {
    run_with_registry_and_local(cfg, services, host_registry, None).await
}

/// Variant of [`run_with_registry`] that also accepts a local VMM
/// backend to register as an in-process host. Used by `--mode=all`:
/// the backend is wrapped in a `LocalHostClient` that shares the
/// AppState's `HarnessHub`, so harness ops routed through
/// `services.host` land on the same hub that `lib.rs::set_harness_sink`
/// plumbs vsock dials into. Without this, in-process harness routing
/// would target a different hub than the FC backend's sink writes to.
pub async fn run_with_registry_and_local(
    cfg: CoordinatorConfig,
    services: Services,
    host_registry: Arc<HostRegistry>,
    in_proc_local: Option<(
        engram_core::HostId,
        Arc<dyn engram_core::traits::SandboxBackend>,
    )>,
) -> Result<(), CoordinatorError> {
    let meta_for_listener = services.meta.clone();
    let state = Arc::new(AppState::new_with_registry(
        cfg.clone(),
        services,
        host_registry,
    ));
    if let Some((host_id, backend)) = in_proc_local {
        state.register_local_host(host_id, backend);
    }

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

    // ADR 0013: hydrate the in-memory `HostRegistry` + `GrpcHostPool`
    // from the `hosts` rows persisted in Postgres. Closes the gap
    // between coord pod boot and the first heartbeat from each host
    // (~5s otherwise). Without this, a session-create that lands on
    // a freshly-booted pod within that window errors with "no hosts
    // connected to the coordinator". Heartbeats then self-heal on
    // their own tick (see the ADR 0013 path in
    // `api/host_http::heartbeat`), but the startup hydrate cuts the
    // first-request failure window to zero. Best-effort.
    if let Err(e) = prewarm_host_registry(&state).await {
        tracing::warn!(error = %e, "startup host-registry prewarm failed");
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

    // ADR 0016 §A.1.5c: stale-lease reaper for the
    // `eviction_inflight` PG table. Any lease whose RAII Drop was
    // skipped (panic, OOM, pod terminated mid-pipeline) becomes
    // a permanent block on re-evicting that session until reaped.
    // 180s max-age matches the host-side §A.1.5a sweep; 30s poll
    // interval is cheap (one DELETE every 30s, rows are short-
    // lived under normal operation).
    let _eviction_lease_reaper = idle_evictor::spawn_eviction_lease_reaper(
        state.services.meta.clone(),
        std::time::Duration::from_secs(180),
        std::time::Duration::from_secs(30),
    );

    // ADR 0013 + ADR 0011 #2: the idle-eviction *driver* runs on
    // each host-agent (its local HarnessHub is authoritative for
    // "is this sandbox idle?"). The host POSTs candidates to
    // `/api/hosts/:id/idle-eviction-candidates`; the receiving
    // coord pod runs the pipeline (`evict_idle_session` below).
    // No background task lives here anymore.

    // Phase 4 Track D: preemption best-effort drain. Subscribes to
    // `cloud.preemption_signal()` (engram-cloud-gcp polls the GCE
    // metadata server, MockCloud's `trigger_preemption` for tests)
    // and on notice fans out across all active sessions on this
    // host: workspace checkpoint → destroy → mark Dead.
    // Caller-driven recovery via `POST /sessions/:id/resume`.
    let _preemption_drain = preemption_drain::spawn(state.clone());

    // ADR 0009 §1-§3: in-process reconcile driver. In `--mode=all`
    // (single-process coord+host) and `--mode=host` test fixtures
    // there's no WS heartbeat path — the reconcile hook in
    // `api/hosts.rs::handle_connection` only fires for hosts that
    // dialed `/api/hosts/connect`. Without this background task the
    // in-proc host's stuck-sandbox sessions would sit `active`
    // forever (the exact bug ADR 0009 is meant to close).
    //
    // In `--mode=coordinator` the WS path already drives reconcile
    // for every connected host, and `host_registry.host_ids()` is
    // empty at startup (hosts dial in later, each WS connection
    // calls reconcile directly), so this task is harmless even when
    // it runs — but we skip it in coordinator-only mode to keep the
    // wire path the single source of truth.
    let _reconcile_driver = if matches!(state.cfg.mode, crate::config::RunMode::All) {
        Some(reconcile::spawn_in_proc(
            state.clone(),
            reconcile::DEFAULT_TICK_INTERVAL,
        ))
    } else {
        None
    };

    // ADR 0016 Phase C: chunk-store GC, redesigned. Pin-set unions
    // enabled_images + sessions.live_disk_manifest_* + recoverable
    // snapshots (disk + memory); `chunk_generation` barrier catches
    // mid-sweep flush races; 24h grace period in
    // `chunk_gc_candidates` absorbs straggling races. Gated by
    // `ENGRAM_CHUNK_GC_ENABLED` (default ON; the grace window is
    // the real safety net in active-development posture).
    let _chunk_gc_sweep = {
        let cfg = chunk_gc::ChunkGcConfig::from_env();
        tokio::spawn(chunk_gc::gc_sweep_loop(state.clone(), cfg))
    };

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
        state.services.host.set_harness_sink(sink);
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

/// ADR 0013 startup hydrate. Read the `hosts` rows persisted in
/// Postgres, and for each one whose `host_addr` is populated +
/// status is Ready/Draining + last heartbeat is recent enough that
/// the dead-host detector wouldn't reap it, warm the pool entry +
/// register a `GrpcHostClient` into `HostRegistry`.
///
/// "Recent enough" is conservative: 60s, which is 2× the default
/// dead-host threshold of 30s. Hosts whose heartbeats are older
/// than that are likely actually dead — we skip them and let
/// dead_host's detector do its job. Their `host_addr` row will be
/// reused if they restart and re-register.
async fn prewarm_host_registry(state: &AppState) -> Result<(), engram_core::MetaError> {
    use chrono::Utc;
    let rows = state.services.meta.list_active_hosts().await?;
    let cutoff = Utc::now() - chrono::Duration::seconds(60);
    let mut warmed = 0usize;
    for row in rows {
        let Some(host_addr) = row.host_addr.clone() else {
            continue;
        };
        if row.last_heartbeat_at < cutoff {
            tracing::debug!(
                host_id = %row.id,
                last_heartbeat_at = %row.last_heartbeat_at,
                "skipping prewarm for stale host; dead-host detector will reap",
            );
            continue;
        }
        if let Err(e) = state
            .services
            .host_pool
            .warm(row.id, host_addr.clone())
            .await
        {
            tracing::warn!(
                host_id = %row.id,
                host_addr = %host_addr,
                error = %e,
                "startup pool.warm failed; heartbeat will self-heal",
            );
            continue;
        }
        let client = match state.services.host_pool.get(row.id) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    host_id = %row.id,
                    error = %e,
                    "startup pool.get failed after warm",
                );
                continue;
            }
        };
        let backend: std::sync::Arc<dyn engram_core::traits::HostClient> =
            std::sync::Arc::new(client);
        state.host_registry.register(row.id, backend);
        warmed += 1;
    }
    if warmed > 0 {
        tracing::info!(
            hosts = warmed,
            "prewarmed host_registry + grpc pool from persisted hosts rows",
        );
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "failed to install SIGTERM handler; ctrl-c only");
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("coordinator received ctrl-c, shutting down");
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("coordinator received ctrl-c, shutting down");
            }
            _ = term.recv() => {
                tracing::info!("coordinator received SIGTERM, shutting down");
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("coordinator received ctrl-c, shutting down");
    }
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
