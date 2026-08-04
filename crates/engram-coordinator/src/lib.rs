//! HTTP API + scheduler for Engram.
//!
//! Phase 1 surface: `POST /sessions`, `POST /sessions/:id/exec`,
//! `DELETE /sessions/:id`, plus snapshot/restore stubs and a healthz.
//! Multiple coordinator replicas are supported by virtue of all state
//! living in Postgres; this binary itself is stateless.

use std::sync::Arc;

use engram_core::traits::{BlobStorage, HostClient, MetadataStore, SecretStore};

pub mod api;
pub mod base_snapshot_retention;
pub mod boot_bundle;
pub mod builtin_harness;
pub mod bundle_gc;
pub mod checkpoint_retention;
pub mod chunk_gc;
pub mod config;
pub mod cow_state;
pub mod dead_host;
pub mod enable_scanner;
pub mod error;
pub mod evac_resumer;
pub mod evacuation;
pub mod grpc_app;
pub mod harness_catalog;
pub mod harness_desync;
pub mod harness_paths;
pub mod host_registry;
pub mod idle_detector;
pub mod idle_evictor;
pub mod integration_ops;
pub mod integrations;
pub mod live_migration;
pub mod metrics;
pub mod oauth;
pub mod oauth_redirect;
pub mod oauth_refresh;
pub mod org_secrets;
pub mod outbox_delivery;
pub mod pg_listener;
pub mod placement;
pub mod queue_scanner;
pub mod reconcile;
pub mod scheduler;
pub mod session_boot;
pub mod session_ops;
pub mod session_shell_pin;
pub mod session_verbs;
pub mod skill_pack;
pub mod snapshot_blob_gc;
#[cfg(test)]
mod span_parenting_tests;
pub mod squashfs;
pub mod state;

pub use config::CoordinatorConfig;
pub use error::ApiError;
pub use host_registry::HostRegistry;
pub use state::AppState;

/// Container for the wired-up dependencies. Built once at startup;
/// passed by `Arc<AppState>` into the axum router.
pub struct Services {
    pub meta: Arc<dyn MetadataStore>,
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
    /// ADR 0007: where an in-process host-agent would materialize
    /// chunked manifests, for the admin orphan-reap endpoint.
    /// Currently always `None`: `--mode=all`'s only backend is
    /// `ProcessBackend` (the FC/VZ in-proc arms were retired, #530
    /// item f), which has no chunk store / materialize wiring, and
    /// `--mode=coordinator`'s reap would need a multi-host fanout
    /// that doesn't exist yet. Kept as a field (not deleted) since a
    /// real `--mode=all` materialize-dir producer would plug back in
    /// here without a wire change.
    pub materialize_dir: Option<std::path::PathBuf>,
    /// ADR 0098 D1: wall-clock/monotonic time as a world input. All
    /// decision-feeding `now` reads in this crate go through here
    /// (enforced by clippy `disallowed-methods`); the DST harness
    /// substitutes a scheduler-driven `SimClock`.
    pub clock: Arc<dyn engram_core::traits::Clock>,
    /// ADR 0098 D1: randomness as a world input — UUID minting and
    /// backoff jitter. The DST harness substitutes a seeded stream so
    /// ids replay from a seed.
    pub entropy: Arc<dyn engram_core::traits::Entropy>,
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
    // ADR 0056: configured provider integrations (the GitHub App, etc), or
    // empty. Set onto `AppState.integrations` for the in-session forge endpoints.
    integrations: crate::integrations::IntegrationBroker,
) -> Result<(), CoordinatorError> {
    let meta_for_listener = services.meta.clone();
    let mut app = AppState::new_with_registry(cfg.clone(), services, host_registry);
    app.integrations = integrations;
    let state = Arc::new(app);
    crate::oauth::spawn_cleanup(state.oauth.clone(), state.subscribe_shutdown());
    crate::oauth_refresh::spawn_connector_refresh(state.oauth.clone(), state.subscribe_shutdown());
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

    // ADR 0048 (queue fairness): shared wake handle between the pg
    // listener (fires it on `placement_changed` NOTIFYs) and the queue
    // scanner (parks on it instead of a pure poll). One per coord
    // replica — see the `queue_scanner` module doc.
    let queue_wake = Arc::new(tokio::sync::Notify::new());

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
        state.integrations.clone(),
        state.boot_bundles.clone(),
        queue_wake.clone(),
        state.outbox_wake.clone(),
        state.session_ops_wake.clone(),
    );

    // Phase 3d follow-up: dead-host auto-detector. Fully trait-driven
    // (ADR 0098 D4): cross-replica mutual exclusion is a MetadataStore
    // leasing row (`dead_host_inflight`), not an advisory lock on a
    // private PgPool — so it works against any store, including the
    // simulator. A stale heartbeat triggers eviction within
    // ~poll_interval + stale_threshold.
    let _dead_host = dead_host::spawn(dead_host::DeadHostConfig::default(), state.clone());
    // ADR 0018 commit 12c: the evac-resumer scanner picks up sessions
    // marked Evacuating (by the admin /drain, /evacuate, or
    // dead_host.rs) and drives Evacuating → Created → Active on a
    // peer host via the shared resume primitives. Without it,
    // sessions transitioned to Evacuating just sit there. Doesn't
    // require its own PgPool — it goes through MetadataStore.
    let _evac_resumer =
        evac_resumer::spawn(evac_resumer::EvacResumerConfig::default(), state.clone());

    // ADR 0048: the session queue scanner. Drives `queued` sessions to
    // placement (best-fit, per-fit-class FIFO) as capacity frees / the
    // fleet scales up, or times them out. Lease-guarded → replica-safe.
    // Without it, a session enqueued on no-capacity sits forever.
    // Push-driven via `queue_wake` (see above); polling is the fallback.
    // ADR 0073 phase 2: the outbox delivery driver — resume-behind-
    // enqueue + at-least-once forward + redelivery-until-acked.
    let _outbox_delivery = outbox_delivery::spawn(state.clone(), state.outbox_wake.clone());
    // ADR 0079: the session-op executor — the lifecycle kernel.
    let _session_ops = session_ops::spawn(state.clone(), state.session_ops_wake.clone());
    let _queue_scanner = queue_scanner::spawn(
        queue_scanner::QueueScannerConfig::default(),
        state.clone(),
        queue_wake,
    );

    // ADR 0028 Fix A: prune aged-out per-session checkpoint rows
    // (latest-per-session always kept; the window doubles as the
    // forkable history). Without it, periodic checkpoints grow
    // `snapshots` one row per session per cadence interval forever.
    let _checkpoint_retention = checkpoint_retention::spawn(
        checkpoint_retention::CheckpointRetentionConfig::default(),
        state.clone(),
    );

    // Orphaned base-snapshot reaper: image refresh swaps
    // `enabled_images.base_snapshot_id` to a fresh capture and leaves the
    // prior base row dangling (session_id NULL, no pointer). Nothing else
    // deletes it, and it keeps pinning its own chunks via pin-set sources
    // #3/#4 — so without this every refresh leaks a base snapshot's chunks
    // (20–32 GB for the heavy images). Complements checkpoint_retention,
    // which is `session_id IS NOT NULL` only.
    let _base_snapshot_retention = base_snapshot_retention::spawn(
        base_snapshot_retention::BaseSnapshotRetentionConfig::default(),
        state.clone(),
    );

    // ADR 0036: the enable-job scanner drives async image enables
    // (pending → materializing → capturing → ready) recorded by
    // POST /api/enabled-images. Lease-claimed per job, so multiple
    // coord pods cooperate instead of duplicating pipelines.
    let _enable_scanner = enable_scanner::spawn(
        enable_scanner::EnableScannerConfig::from_env(),
        state.clone(),
    );

    // ADR 0079: the session-lease reaper is GONE — the op executor's
    // reclaim sweep (session_ops.rs) is the fence-then-resume successor
    // for any lifecycle holder that dies mid-pipeline.

    // ADR 0013 + ADR 0011 #2: the idle-eviction *detection driver*
    // runs on each host-agent (its local HarnessHub is authoritative
    // for "is this sandbox idle?"). The host POSTs candidates to
    // `/api/hosts/:id/idle-eviction-candidates`.
    //
    // ADR 0034: the receiving handler only flips Active → Evicting;
    // this scanner sweeps `status='evicting'` and runs the snapshot
    // pipeline detached from any request lifetime (the pre-0034
    // inline pipeline died by cancellation at the host's POST
    // timeout). Its first tick after startup also recovers rows
    // wedged across a coord deploy.
    let _eviction_scanner = idle_evictor::spawn_eviction_scanner(
        idle_evictor::EvictionScannerConfig::default(),
        state.clone(),
    );

    // ADR 0034 L3: PG-derived idle-detection backstop. Catches
    // Active sessions whose harness the host has gone blind to
    // (vsock detach wipes the hub's tracking; `idle_sandboxes` only
    // nominates attached harnesses) by reading the durable activity
    // record — session_events — instead. Hard-TTL only; nominates
    // into the same Evicting lane the host path uses.
    // ADR 0073 phase 4: THE idle detector (the host detection plane +
    // the L3 backstop are unified here — see idle_detector.rs).
    let _idle_detector =
        idle_detector::spawn(idle_detector::IdleDetectorConfig::from_env(), state.clone());

    // Track A: harness-desync watchdog. Catches the wedge class the
    // silence-only backstop misses — a harness whose event stream desynced
    // from the run state machine (a run-scoped event with no open run, or a
    // stuck-open run) — and recovers it with a non-destructive harness
    // re-handshake, escalating to the eviction lane if the nudges don't take.

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

    // ADR 0023: register a ForgeSink so per-VM forge vsock dials are
    // served by the coord's GitForge (Firecracker path). ProcessBackend's
    // default no-op is fine — its in-guest helper hits the HTTP forge
    // endpoint on loopback.
    {
        let forge_state = state.clone();
        let sink: engram_core::traits::ForgeSink = std::sync::Arc::new(move |stream| {
            let st = forge_state.clone();
            tokio::spawn(async move {
                crate::api::forge::handle_vsock_connection(st, stream).await;
            });
        });
        state.services.host.set_forge_sink(sink);
    }

    // ADR 0026: register an UploadSink so per-VM artifact-upload vsock
    // dials are served by the coord (Firecracker in-proc path). Split
    // mode uses the host-agent's own relay → POST /api/hosts/upload;
    // ProcessBackend's default no-op is fine.
    {
        let upload_state = state.clone();
        let sink: engram_core::traits::UploadSink = std::sync::Arc::new(move |stream| {
            let st = upload_state.clone();
            tokio::spawn(async move {
                crate::api::upload::handle_vsock_connection(st, stream).await;
            });
        });
        state.services.host.set_upload_sink(sink);
    }

    // ADR 0051 B: the orchestrator-facing app gRPC surface, served
    // BESIDE the axum API (additive — no REST route changes). Spawned so
    // a runtime serve crash doesn't take the web surface down with it
    // (the task just logs). Awaits the same `shutdown_signal()` future
    // the axum side uses below — tokio signal listeners are
    // multi-subscriber, so one SIGTERM/ctrl-c gracefully closes both
    // servers — and the JoinHandle is awaited after axum returns so
    // shutdown drains BOTH servers before the process exits.
    let app_grpc = {
        let grpc_listener = tokio::net::TcpListener::bind(cfg.app_grpc_addr)
            .await
            .map_err(CoordinatorError::Io)?;
        let incoming =
            tonic::transport::server::TcpIncoming::from_listener(grpc_listener, true, None)
                .map_err(|e| {
                    CoordinatorError::Config(format!("app gRPC listener setup failed: {e}"))
                })?;
        tracing::info!(addr = %cfg.app_grpc_addr, "app gRPC server listening");
        let serve = grpc_app::server(state.clone())
            .serve_with_incoming_shutdown(incoming, shutdown_signal());
        tokio::spawn(async move {
            if let Err(e) = serve.await {
                tracing::error!(error = %e, "app gRPC server exited");
            }
        })
    };

    let app = api::router(state.clone());
    let listener = tokio::net::TcpListener::bind(cfg.bind_addr.as_str())
        .await
        .map_err(CoordinatorError::Io)?;
    tracing::info!(addr = %cfg.bind_addr, "coordinator listening");

    // ADR 0050 B: graceful shutdown that doesn't hang on long-lived
    // streams. The graceful future waits for SIGTERM/ctrl-c, then
    // `trigger_shutdown()` flips the watch so SSE `/events` +
    // `/exec/stream` handlers end their streams (tokio-rs/axum#2673:
    // hyper otherwise waits for those connections to close on their own,
    // which they never do → SIGKILL at the grace period). Only then does
    // axum begin draining; the now-finite connections drain fast.
    let shutdown_state = state.clone();
    let graceful = async move {
        shutdown_signal().await;
        shutdown_state.trigger_shutdown();
    };

    // Force-exit backstop: if the drain somehow exceeds the budget
    // (a stuck non-streaming request), exit(0) cleanly BEFORE the
    // kubelet's SIGKILL so we never abandon connections uncleanly.
    // `terminationGracePeriodSeconds` (chart) must exceed
    // preStop drain + this budget.
    {
        let mut rx = state.subscribe_shutdown();
        let budget = shutdown_drain_budget();
        tokio::spawn(async move {
            let _ = rx.wait_for(|shutting_down| *shutting_down).await;
            tokio::time::sleep(budget).await;
            tracing::warn!(
                budget_secs = budget.as_secs(),
                "graceful drain exceeded budget; forcing clean exit before SIGKILL",
            );
            std::process::exit(0);
        });
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(graceful)
        .await
        .map_err(CoordinatorError::Io)?;

    // Graceful shutdown drains both servers: axum has returned, now
    // wait for the app gRPC serve loop to finish its own drain. A
    // JoinError here means the spawned task panicked — log it rather
    // than turning a clean web-side shutdown into a process error.
    if let Err(e) = app_grpc.await {
        tracing::error!(error = %e, "app gRPC server task panicked");
    }
    Ok(())
}

/// ADR 0050 B: how long after shutdown begins to wait for connections to
/// drain before force-exiting (env `ENGRAM_SHUTDOWN_DRAIN_SECS`, default
/// 25s). Must be < `terminationGracePeriodSeconds − drainSeconds` so the
/// clean exit beats the kubelet's SIGKILL.
fn shutdown_drain_budget() -> std::time::Duration {
    let secs = std::env::var("ENGRAM_SHUTDOWN_DRAIN_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(25);
    std::time::Duration::from_secs(secs)
}

/// Warm `HostRegistry`'s `sandbox_owner` read-through cache from the
/// persisted `sandbox_id → host_id` pairs of active sessions, so the
/// first `/exec` after a coordinator (re)start routes without a
/// PG read-through storm. This is a pure latency optimization — the
/// session→sandbox binding itself lives only in Postgres now
/// (ADR 0047; see [`AppState::resolve_sandbox`]), and `sandbox_owner`
/// self-populates via `host_for_sandbox` on any miss — so a skipped
/// warm just costs one extra read on first access. Sessions in
/// `Pending` / `Idle` / `Dead` have no live sandbox and are left for
/// `/resume`.
async fn repopulate_routing(state: &AppState) -> Result<(), engram_core::MetaError> {
    let sessions = state.services.meta.list_active_sessions().await?;
    let mut warmed = 0usize;
    for s in sessions {
        if let (Some(sandbox_id), Some(host_id)) = (s.sandbox_id, s.host_id) {
            state
                .host_registry
                .record_sandbox_owner(sandbox_id, host_id);
            warmed += 1;
        }
    }
    if warmed > 0 {
        tracing::info!(
            sessions = warmed,
            "warmed sandbox_owner cache for active sessions",
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
    let rows = state.services.meta.list_active_hosts().await?;
    let cutoff = state.services.clock.now_utc() - chrono::Duration::seconds(60);
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
