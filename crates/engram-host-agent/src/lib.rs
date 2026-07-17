//! Per-host daemon. Wraps a [`SandboxBackend`], composes the
//! chunked-OCI image cache / tiered chunk resolver / egress proxy /
//! NBD pool, runs the snapshot manager and resource governor, and
//! heartbeats the coordinator with capacity + local-snapshot state.
//!
//! Phase 1 wires create/exec/destroy through the SandboxBackend trait —
//! currently `engram-sandbox-process` for fast dev loops, with
//! `engram-sandbox-firecracker` filling in the production path. Phase 2
//! lights up the snapshot manager. Phase 3 turns heartbeats into a real
//! gRPC channel; today they go through an in-process trait object so
//! the `--mode=all` single-binary path works.

use std::path::PathBuf;
use std::sync::Arc;

use engram_chunk_store::{ChunkCache, ChunkStore};
use engram_core::traits::{CloudBackend, SandboxBackend};
use engram_core::SandboxId;
// ADR 0098 Phase 2: the teardown-reconcile / TTL-sweep / reattach-source
// paths hold the coordinator via the `CoordControlPlane` seam.
use engram_host_core::CoordControlPlane;

use crate::image_cache::ImageCache;

pub mod admin_handler;
pub mod base_shm_gc;
pub mod bindings;
pub mod blob;
pub mod bundles;
pub mod capabilities;
pub mod capture_job;
pub mod checkpoint;
pub mod config;
pub mod coord_client;
pub mod device_sync;
pub mod dirty_map;
pub mod disk_daemon;
pub mod durable_record;
pub mod egress;
pub mod eviction_finalize;
pub mod grpc_server;
pub mod harness;
pub mod host_client;
pub mod migrate_peer;
pub mod migration;
pub mod peer_fill;
pub mod session_epochs;
pub mod substrate_server;
pub use host_client::LocalHostClient;
pub mod heartbeat;
pub mod idle_evictor;
pub mod image_cache;
pub mod image_prefetch;
pub mod live_attach;
pub mod materialize;
pub mod metrics;
pub mod orphan_reap;
pub mod pooled_backend;
pub mod proxy_port;
pub mod proxy_shell;
pub mod ram_ledger;
pub mod resource;
pub mod snapshot;
pub mod teardown_reconcile;
mod time_source;
pub mod trace_scope;
pub mod util;
pub mod warm_progress;

pub use config::HostAgentConfig;

pub struct HostAgent {
    pub cfg: HostAgentConfig,
    pub sandbox: Arc<dyn SandboxBackend>,
    pub cloud: Arc<dyn CloudBackend>,
    /// ADR 0006: local egress proxy. The production host-agent binary
    /// ALWAYS attaches one (issue #240 made it mandatory — a host that
    /// can't stand up the proxy refuses to start). `None` is reachable
    /// only from in-process test harnesses and the non-FC dev backends
    /// (VZ/Process), which carry no internet-facing guest network.
    pub egress: Option<Arc<egress::HostEgress>>,
    /// ADR 0007: chunk store + per-host materialization root. `None`
    /// keeps the legacy OCI-pulled `rootfs.ext4` path active (chunked
    /// images skip the chunk-store resolve and use the cached file
    /// directly).
    pub chunk_store: Option<(ChunkStore, PathBuf)>,
    /// Per-host OCI image cache. `None` means the host-agent can't
    /// pull images by URI — sessions must arrive with
    /// `spec.rootfs_source` set already (the dev-only path).
    /// Production multi-host topologies must set this.
    pub image_cache: Option<ImageCache>,
    /// ADR 0007 #3a: NVMe-backed chunk cache. Optional; wired in
    /// production to amortise chunk reads across manifests.
    pub chunk_cache: Option<ChunkCache>,
    /// ADR 0007 Phase 4: pool of `/dev/nbdN` device paths the
    /// daemon allocates from when serving chunked rootfs disks.
    /// `None` keeps the legacy materialize-to-file path active.
    pub nbd_pool: Option<Arc<disk_daemon::NbdSlotAllocator>>,
    /// ADR 0009 §6: typed handle on the FC backend (when this host
    /// uses Firecracker) so the live-attach pass at startup can
    /// invoke `reattach_sandbox`. The `Arc<dyn SandboxBackend>` in
    /// `sandbox` can't be downcast to a concrete type (the trait
    /// doesn't extend `Any`), so callers that want live-attach
    /// must wire this separately. `None` skips reattach entirely
    /// (clean-slate startup; matches today's behaviour).
    pub fc_for_reattach: Option<Arc<engram_sandbox_firecracker::FirecrackerBackend>>,
    /// ADR 0007 Phase 5: this host's stable `HostId`. Stamped on
    /// snapshots' `trace_host_hint` (so cross-host restore knows
    /// which trace to prefault) AND passed to the UFFD handler
    /// as `--publish-trace-host`. `None` generates a fresh id at
    /// run time (the pre-Phase-5 default).
    pub host_id: Option<engram_core::HostId>,
    /// ADR 0045 addendum (2026-07-10): the base-shm sweeper's
    /// enabled-image keep-set, shared with `base_shm_gc::spawn`
    /// (main.rs owns the sweeper; the image-prefetch supervisor
    /// spawned in `run` publishes into it). Always present; inert
    /// unless a sweeper holds the same handle.
    pub base_shm_protected: Arc<base_shm_gc::ProtectedPaths>,
}

impl HostAgent {
    pub fn new(
        cfg: HostAgentConfig,
        sandbox: Arc<dyn SandboxBackend>,
        cloud: Arc<dyn CloudBackend>,
    ) -> Self {
        Self {
            cfg,
            sandbox,
            cloud,
            egress: None,
            chunk_store: None,
            image_cache: None,
            chunk_cache: None,
            nbd_pool: None,
            host_id: None,
            fc_for_reattach: None,
            base_shm_protected: base_shm_gc::ProtectedPaths::new(),
        }
    }

    /// ADR 0045 addendum (2026-07-10): share the base-shm sweeper's
    /// enabled-image keep-set so the image-prefetch supervisor
    /// (spawned in `run`) publishes into the SAME registry the
    /// sweeper consults.
    pub fn with_base_shm_protected(mut self, protected: Arc<base_shm_gc::ProtectedPaths>) -> Self {
        self.base_shm_protected = protected;
        self
    }

    /// ADR 0009 §6: register the concrete FC backend for the
    /// startup reattach pass. Optional — only meaningful when
    /// `--sandbox-backend=firecracker` (reattach is unconditional
    /// for the FC backend).
    /// When unset, the host-agent starts clean-slate (reconcile
    /// then flips orphaned sessions per §3).
    pub fn with_fc_reattach(
        mut self,
        fc: Arc<engram_sandbox_firecracker::FirecrackerBackend>,
    ) -> Self {
        self.fc_for_reattach = Some(fc);
        self
    }

    /// Set this host's stable `HostId`. Pair with the same id
    /// stamped on `FirecrackerConfig.host_id` at backend
    /// construction — both sides need to agree so the trace
    /// `traces/<manifest_id>/<host_id>.json` keying is consistent.
    pub fn with_host_id(mut self, id: engram_core::HostId) -> Self {
        self.host_id = Some(id);
        self
    }

    /// Attach a `ChunkCache`. Optional; layers on top of
    /// `with_chunk_store` to amortise chunk reads across manifests
    /// (canonical-base images, forks).
    pub fn with_chunk_cache(mut self, cache: ChunkCache) -> Self {
        self.chunk_cache = Some(cache);
        self
    }

    /// Attach a `/dev/nbdN` slot allocator. When set + a chunk
    /// store + chunk cache are also wired, `create()` spawns the
    /// NBD daemon to serve chunked rootfs disks instead of
    /// materializing them to single files. Linux-only at runtime.
    pub fn with_nbd_pool(mut self, pool: Arc<disk_daemon::NbdSlotAllocator>) -> Self {
        self.nbd_pool = Some(pool);
        self
    }

    /// Attach a local egress proxy. The host-agent will route every
    /// inbound `notify_session_policy` to this proxy's registry,
    /// unregister sandboxes on `destroy`, and expose the CA cert
    /// PEM to substrate-building code paths.
    pub fn with_egress(mut self, egress: Arc<egress::HostEgress>) -> Self {
        self.egress = Some(egress);
        self
    }

    /// Attach a chunk store + per-host materialization directory.
    /// The PooledBackend uses these to resolve chunked image
    /// manifests to per-host materialized rootfs files.
    pub fn with_chunk_store(mut self, chunk_store: ChunkStore, materialize_dir: PathBuf) -> Self {
        self.chunk_store = Some((chunk_store, materialize_dir));
        self
    }

    /// Attach a per-host OCI image cache. Sessions with `image_uri`
    /// set route through this cache (pull on miss, hit on subsequent
    /// references). Required for multi-host production where the
    /// coordinator hands out images by URI.
    pub fn with_image_cache(mut self, cache: ImageCache) -> Self {
        self.image_cache = Some(cache);
        self
    }

    /// Run the host agent's background loops until shutdown.
    ///
    /// Phase 3a: if `coordinator_endpoint` is set, runs the WS dialer
    /// against that coordinator until ctrl-c (reconnecting on drops).
    /// Otherwise stays idle until ctrl-c — useful for `engram-host-agent`
    /// in standalone dev where the binary just hosts a local backend
    /// without phoning home.
    pub async fn run(self) -> Result<(), HostAgentError> {
        tracing::info!(?self.cfg.work_dir, "host-agent starting");
        let _ = self.cloud.host_metadata().await;

        // ADR 0009 §6 / ADR 0044 K2: live-VM reattach pass. Runs once at
        // startup, before connecting to coord, so reattached sandboxes
        // show up in the very first heartbeat's `running_sandboxes` field
        // — coord sees them as continuously-present and doesn't strike-out
        // / flip the owning sessions.
        //
        // This is now UNCONDITIONAL for the FC backend, not opt-in: K2
        // detaches the node's VMs on a host-agent restart (no kill, no
        // checkpoint), so the successor generation MUST re-adopt them or
        // they leak. Non-FC backends register no `fc_for_reattach` and
        // clean-slate (VZ/process don't survive a host-agent restart).
        let mut reattached_roles: Vec<(SandboxId, migration::MigrationRole)> = Vec::new();
        if let Some(fc) = self.fc_for_reattach.as_ref() {
            match live_attach::reattach_pass(&self.cfg.work_dir, fc).await {
                Ok(report) => {
                    tracing::info!("{}", report.summary());
                    // ADR 0045 C2: sandboxes that were mid-post-copy when
                    // the previous generation died; the fences re-arm
                    // once the pooled backend exists below.
                    reattached_roles = live_attach::scan_migration_roles(&self.cfg.work_dir);
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "live-attach pass failed; continuing with clean-slate startup"
                    );
                }
            }
        }

        if let Some(coord_url) = self.cfg.coordinator_endpoint.clone() {
            // Re-use the host_id that was stamped on the FC
            // backend at construction time, so trace replay keys
            // line up. Falls back to a fresh id when the caller
            // didn't set one — that's the pre-Phase-5 behaviour
            // (no trace replay).
            let host_id = self.host_id.unwrap_or_default();
            tracing::info!(
                host_id = %host_id,
                coordinator = %coord_url,
                "dialing coordinator",
            );
            // Wrap the underlying SandboxBackend in a PooledBackend so
            // session creation can attach the egress / chunk-store /
            // image-cache state without burdening the underlying VZ /
            // FC drivers with knowledge of those subsystems.
            let pooled = {
                let mut p = pooled_backend::PooledBackend::new(self.sandbox.clone());
                if let Some(egress) = self.egress.clone() {
                    p = p.with_egress(egress);
                }
                if let Some((cs, dir)) = self.chunk_store.clone() {
                    p = p.with_chunk_store(cs, dir);
                    // ADR 0080 §C: the MaterializeImage scratch root —
                    // gated on the chunk store because a materialize
                    // without one has nowhere durable to land. Same
                    // volume as the chunk cache (work_dir), so the
                    // statvfs headroom check measures the disk that
                    // actually fills. Sweep orphans from a previous
                    // generation that died mid-run before serving.
                    let scratch = self.cfg.work_dir.join("materialize-scratch");
                    crate::materialize::reconcile_scratch(&scratch);
                    p = p.with_materialize_scratch(scratch);
                }
                if let Some(cache) = self.chunk_cache.clone() {
                    p = p.with_chunk_cache(cache);
                }
                if let Some(ic) = self.image_cache.clone() {
                    // ADR 0008 Phase 5: also feed the image cache's
                    // OciClient into the pooled backend so chunked-
                    // OCI images can fault chunks from the registry
                    // via the tiered resolver path.
                    p = p.with_oci_client(ic.oci_client());
                    p = p.with_image_cache(ic);
                }
                if let Some(pool) = self.nbd_pool.clone() {
                    p = p.with_nbd_pool(pool);
                }
                // ADR 0016 Phase B: wire the coord-bound
                // live-manifest publisher. The publisher's drain
                // task is owned by `p` (via
                // `LiveManifestPublisherHandle`), so it dies with
                // the host-agent process.
                let publisher_coord: Arc<dyn CoordControlPlane> =
                    Arc::new(coord_client::HttpCoordClient::new(
                        coord_url.clone(),
                        self.cfg.coordinator_token.clone(),
                    ));
                p = p.with_live_manifest_coord_publisher(publisher_coord, host_id);
                // ADR 0028 Fix A: checkpoint chains (rolling memory
                // images + durable records) live under the work dir.
                // Gated on (a) a chunk store being wired — nothing
                // durable to chain without it — AND (b) the backend
                // actually producing coherent memory checkpoints (FC
                // with dirty tracking). The latter keeps a split-mode
                // VZ / Process host (which the Tilt local setup runs)
                // from ever engaging the chain or the periodic driver
                // below — VZ has no guest-memory snapshot, so a
                // checkpoint there is just a wasteful VM pause.
                let checkpoints_supported = self.sandbox.supports_diff_checkpoints();
                if self.chunk_store.is_some() && checkpoints_supported {
                    p = p.with_checkpoint_dir(self.cfg.work_dir.join("checkpoints"));
                }
                let arc = Arc::new(p);
                // Issue #529: must run before anything spawns a finalize
                // job against this backend (the checkpoint driver, an
                // incoming snapshot_begin, or resume_pending_finalizes
                // below all rely on it for the terminal destroy call).
                arc.set_self_ref(&arc);
                // Re-seed checkpoint chains for pidfd-reattached
                // survivors from their durable chain-head records (and
                // GC records for sandboxes that didn't survive). Must
                // run before the checkpoint driver, the eviction
                // redrive, and coordinator registration — the first
                // post-roll capture of every survivor rides the diff
                // path instead of a FULL multi-GiB memory re-chunk
                // (2026-07-13 incident: a survivor's evict capture ran
                // 40+ minutes).
                arc.rehydrate_chain_heads().await;
                arc
            };
            // ADR 0084 P1b: the capture-job executor + durable-record
            // registry. `pooled` (a `SandboxBackend`) is the executor's
            // engine, unchanged — still `PooledBackend::
            // build_base_snapshot` under the hood; this layer adds
            // durable per-job records (surviving a host-agent restart)
            // and a live-sandbox registry that replaces the retired
            // `PooledBackend::is_base_capture` reaper exemption.
            // Unconditional (no chunk-store/diff-checkpoint gate like
            // checkpointing has): a job record's persistence doesn't
            // depend on any optional subsystem.
            // The FC snapshot-format version stamp (probed once — the
            // binary can't change under a running host-agent; None on
            // VZ/Process). The coordinator's finalize requires it on any
            // capture that produced a cold base (ADR 0084 §B content
            // key), and the host that runs the VMM is its authority.
            let fc_snapshot_version = capabilities::fc_snapshot_version(
                self.fc_for_reattach
                    .as_ref()
                    .map(|fc| fc.config().firecracker_bin.clone())
                    .as_deref(),
            )
            .await;
            let capture_jobs = capture_job::CaptureJobExecutor::new(
                pooled.clone(),
                self.cfg.work_dir.join("capture-jobs"),
                fc_snapshot_version,
            );
            capture_jobs.rehydrate().await;
            // ADR 0045 C1: the migration export TTL sweep — the
            // dumb-host rule. An export past EXPORT_TTL means the
            // coordinator never sent commit/abort (it died mid-move):
            // ask it who owns the sandbox now and abort-in-place /
            // destroy / stay-paused per `migration::ttl_verdict`.
            {
                let pooled_for_ttl = pooled.clone();
                let coord_for_ttl: Arc<dyn CoordControlPlane> =
                    Arc::new(coord_client::HttpCoordClient::new(
                        coord_url.clone(),
                        self.cfg.coordinator_token.clone(),
                    ));
                let host_id_for_ttl = host_id;
                tokio::spawn(async move {
                    let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    loop {
                        tick.tick().await;
                        for (sandbox_id, session, export_id) in
                            pooled_for_ttl.expired_migration_exports()
                        {
                            // Panic containment for the TTL safety net: a
                            // panic anywhere in a single export's verdict
                            // execution must NOT propagate and abort this
                            // spawned task — that would silently kill the
                            // dumb-host sweep for the rest of the process
                            // lifetime, leaking every later abandoned
                            // export. Wrap each per-export iteration in
                            // `catch_unwind` so one poisoned export is
                            // logged at error! and the loop keeps ticking.
                            use futures::future::FutureExt as _;
                            let pooled_iter = pooled_for_ttl.clone();
                            let coord_iter = &coord_for_ttl;
                            let export_id_iter = export_id.clone();
                            let outcome = std::panic::AssertUnwindSafe(async move {
                                let ownership = match session {
                                    Some(session_id) => coord_iter
                                        .sandbox_ownership(host_id_for_ttl, session_id, sandbox_id)
                                        .await
                                        .ok(),
                                    None => None,
                                };
                                let state_served = pooled_iter.migration_state_served(sandbox_id);
                                let verdict = migration::ttl_verdict(
                                    session.is_some(),
                                    ownership,
                                    state_served,
                                );
                                tracing::warn!(
                                    %sandbox_id,
                                    ?session,
                                    ?verdict,
                                    "migration export exceeded TTL with no commit/abort",
                                );
                                use engram_core::traits::sandbox::SandboxBackend as _;
                                match verdict {
                                    migration::TtlVerdict::AbortInPlace => {
                                        if let Err(e) = pooled_iter
                                            .migration_abort(sandbox_id, &export_id_iter)
                                            .await
                                        {
                                            tracing::warn!(%sandbox_id, error = %e,
                                                "export TTL abort failed");
                                        }
                                    }
                                    migration::TtlVerdict::Destroy => {
                                        if let Err(e) = pooled_iter
                                            .migration_commit(sandbox_id, &export_id_iter)
                                            .await
                                        {
                                            tracing::warn!(%sandbox_id, error = %e,
                                                "export TTL destroy failed");
                                        }
                                    }
                                    migration::TtlVerdict::StayPaused => {}
                                }
                            })
                            .catch_unwind()
                            .await;
                            if outcome.is_err() {
                                tracing::error!(
                                    %sandbox_id,
                                    "migration TTL sweep iteration panicked; \
                                     contained — sweep continues",
                                );
                            }
                        }
                    }
                });
            }

            // ADR 0050 E: host-local teardown reconcile. The coordinator
            // drives every destroy as a best-effort RPC; when that RPC
            // fails (transient gRPC) the FC leaks, and nothing reaps it
            // (ADR 0009 reconcile only sweeps session→sandbox-missing).
            // This generalizes the migration source-ownership rule
            // (ADR 0045 C1) to ALL sandboxes: each tick, for every
            // non-migration sandbox, ask the coord whether its session
            // still owns it; once the answer has been "no" for
            // ORPHAN_STRIKES consecutive ticks (debounce vs an in-flight
            // create's not-yet-published binding + a transient coord
            // outage), destroy it LOCALLY — a destroy that can't be
            // defeated by the same coord→host gRPC flakiness that leaked
            // it. `sandbox_ownership` reads `sessions.sandbox_id` (PG,
            // ADR 0047's sole authority), so a terminal/idle/rebound
            // session reliably answers "not owned".
            {
                // ADR 0098 P3: the tick body is now
                // `teardown_reconcile::reconcile_once` (pure classify +
                // focused backend seam, driven directly by the host-internal
                // simulator). This wrapper keeps only the interval cadence +
                // the caller-owned strike ledger; `reconcile_once` returns
                // `Err` only when `list()` fails, which we log + skip exactly
                // as the old inline `continue` did.
                let reap_backend = Arc::new(teardown_reconcile::PooledReconcileBackend::new(
                    pooled.clone(),
                    capture_jobs.clone(),
                ));
                let coord_for_reap: Arc<dyn CoordControlPlane> =
                    Arc::new(coord_client::HttpCoordClient::new(
                        coord_url.clone(),
                        self.cfg.coordinator_token.clone(),
                    ));
                let host_id_for_reap = host_id;
                tokio::spawn(async move {
                    use crate::teardown_reconcile::{reconcile_once, RECONCILE_INTERVAL};
                    let mut strikes: std::collections::HashMap<SandboxId, u32> =
                        std::collections::HashMap::new();
                    let mut tick = tokio::time::interval(RECONCILE_INTERVAL);
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    loop {
                        tick.tick().await;
                        if let Err(e) = reconcile_once(
                            &*reap_backend,
                            &*coord_for_reap,
                            host_id_for_reap,
                            &mut strikes,
                        )
                        .await
                        {
                            tracing::warn!(error = %e,
                                "teardown reconcile: list() failed; skipping tick");
                        }
                    }
                });
            }

            // ADR 0045 C2: re-arm post-copy fences for sandboxes that
            // were mid-move across the host-agent restart. A DEST mid-
            // drain is reaped immediately (its drain state died with
            // the previous generation; the coordinator's PeerLost /
            // lease machinery rewinds the session). A SOURCE is NEVER
            // resumed — state may have shipped — so it runs the
            // dumb-host ownership rule until the answer is `false`
            // (destroy) — `true` keeps it paused (the scanner will
            // rehome the session and flip the answer within a cycle).
            for (sandbox_id, role) in reattached_roles {
                match role {
                    migration::MigrationRole::PostCopyDest => {
                        tracing::warn!(%sandbox_id,
                            "reattached post-copy DEST: reaping (drain state lost with the old generation)");
                        let pooled_for_reap = pooled.clone();
                        tokio::spawn(async move {
                            use engram_core::traits::sandbox::SandboxBackend as _;
                            pooled_for_reap.set_migration_role(sandbox_id, None).await;
                            if let Err(e) = pooled_for_reap.destroy(sandbox_id).await {
                                tracing::warn!(%sandbox_id, error = %e,
                                    "reattached post-copy dest reap failed");
                            }
                        });
                    }
                    migration::MigrationRole::PostCopySource => {
                        tracing::warn!(%sandbox_id,
                            "reattached post-copy SOURCE: staying paused under the ownership rule (never self-resumes)");
                        pooled.note_migration_role(sandbox_id, Some(role));
                        let pooled_for_src = pooled.clone();
                        let coord_for_src: Arc<dyn CoordControlPlane> =
                            Arc::new(coord_client::HttpCoordClient::new(
                                coord_url.clone(),
                                self.cfg.coordinator_token.clone(),
                            ));
                        let host_id_for_src = host_id;
                        tokio::spawn(async move {
                            use engram_core::traits::sandbox::SandboxBackend as _;
                            let mut tick =
                                tokio::time::interval(std::time::Duration::from_secs(30));
                            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                            loop {
                                tick.tick().await;
                                // Issue #216 Gap 3: NEVER destroy on a
                                // missing in-memory binding. The binding
                                // rehydrates via the register task's
                                // `rehydrate_survivors`, which races this
                                // tick and backs off up to 30 s when the
                                // coordinator is unreachable — the very
                                // condition that caused the restart. A
                                // `None` here means "not learned yet," not
                                // "unowned." Ask the coordinator (only
                                // possible once bound) and obey the
                                // dumb-host rule: destroy ONLY on an
                                // explicit `owned == false`; stay paused on
                                // unbound / unreachable / still-owned.
                                let session = pooled_for_src.session_for_sandbox(sandbox_id);
                                let ownership = match session {
                                    Some(session_id) => coord_for_src
                                        .sandbox_ownership(host_id_for_src, session_id, sandbox_id)
                                        .await
                                        .ok(),
                                    None => None,
                                };
                                let verdict = migration::reattach_source_verdict(
                                    session.is_some(),
                                    ownership,
                                );
                                match verdict {
                                    migration::ReattachSourceVerdict::Destroy => {
                                        tracing::info!(%sandbox_id,
                                            "ownership moved on; destroying the frozen post-copy source");
                                        pooled_for_src.set_migration_role(sandbox_id, None).await;
                                        if let Err(e) = pooled_for_src.destroy(sandbox_id).await {
                                            tracing::warn!(%sandbox_id, error = %e,
                                                "frozen source destroy failed");
                                        }
                                        return;
                                    }
                                    migration::ReattachSourceVerdict::StayPaused => {
                                        tracing::debug!(%sandbox_id, ?session,
                                            "reattached post-copy source staying paused (unbound, unreachable, or still owned); re-asking next tick");
                                    }
                                }
                            }
                        });
                    }
                }
            }

            // ADR 0028 Fix A: the periodic checkpoint driver. No-ops
            // when ENGRAM_CHECKPOINT_INTERVAL_SECS=0, the backend has
            // no checkpoint dir, or the backend can't do diff
            // checkpoints (VZ / Process — never pauses their VMs).
            let _checkpoint_driver = if self.sandbox.supports_diff_checkpoints() {
                checkpoint::spawn_checkpoint_driver(
                    pooled.clone(),
                    checkpoint::CheckpointConfig::from_env(),
                )
            } else {
                None
            };

            // Issue #529: re-drive every un-acked eviction finalize
            // record left on disk by a prior host-agent process (crash /
            // OOM / rolling update mid-upload) — the crash-recovery half
            // of the host-durable finalize redesign. No-ops when there
            // are none (the common case).
            {
                let pooled_for_finalize_redrive = pooled.clone();
                tokio::spawn(async move {
                    pooled_for_finalize_redrive.resume_pending_finalizes().await;
                });
            }
            // ADR 0013: every harness event POSTs to the coord via
            // HTTP. Any coord pod can serve the POST (the
            // `state.emit` path on the receiving pod handles
            // session_events persistence + SSE fan-out via
            // pg_listener). Per-event POSTs are slightly chattier
            // than the old WS-frame approach but eliminate the
            // pod-pinning required for a long-lived stream and the
            // back-to-back duplicate `harness_idle` de-dup at the
            // coord side already protects against the few extra
            // events that might land out-of-order across pods.
            let coord_client_for_events = coord_client::HttpCoordClient::new(
                coord_url.clone(),
                self.cfg.coordinator_token.clone(),
            );
            // ADR 0098 D1: the harness-event timestamp is read through the
            // injected production clock, constructed once for this sink.
            let events_clock: Arc<dyn engram_core::traits::Clock> =
                Arc::new(engram_core::traits::SystemClock::new());
            let event_sink = crate::harness::event_sink_to(move |session_id, sandbox_id, ev| {
                let cc = coord_client_for_events.clone();
                let events_clock = events_clock.clone();
                async move {
                    let req = coord_client::HarnessEventRequest {
                        sandbox_id,
                        event: ev,
                        at: events_clock.now_utc(),
                    };
                    if let Err(e) = cc.harness_event(session_id, &req).await {
                        tracing::debug!(
                            %session_id, %sandbox_id, error = %e,
                            "forward harness event to coord failed",
                        );
                    }
                }
            });
            let bindings = crate::bindings::BindingStore::open(self.cfg.work_dir.join("bindings"))
                .expect("open binding store under work_dir (ADR 0073)");
            let harness_hub =
                std::sync::Arc::new(crate::harness::HarnessHub::new(event_sink, bindings));
            // Plumb the hub into the FC/VZ backend's vsock-accept
            // sink so inbound harness connections land on the local
            // hub's adapter loop. Without this the FC backend drops
            // every dial with "no sink registered" — the bug Phase 2
            // closes.
            let sink_hub = harness_hub.clone();
            let sink: engram_core::traits::HarnessSink =
                std::sync::Arc::new(move |stream| sink_hub.accept_via_session_lookup(stream));
            pooled.set_harness_sink(sink);

            // ADR 0023 split-mode forge forwarding. The forge sink can't
            // live on the host (it needs the coord's GitForge + broker
            // map), so — exactly like the harness event forwarding above
            // — the host proxies each in-guest forge dial to the coord
            // over HTTP: read the `ForgeRequest` off the vsock stream,
            // POST it to `/api/hosts/forge`, write the `ForgeResponse`
            // back. Without this the FC forge accept loop has no sink and
            // drops every dial (guest sees "Broken pipe").
            let coord_client_for_forge = coord_client::HttpCoordClient::new(
                coord_url.clone(),
                self.cfg.coordinator_token.clone(),
            );
            let forge_sink: engram_core::traits::ForgeSink = std::sync::Arc::new(
                move |mut stream| {
                    let cc = coord_client_for_forge.clone();
                    tokio::spawn(async move {
                        let req: engram_harness_proto::ForgeRequest =
                            match engram_harness_proto::read_msg(&mut stream).await {
                                Ok(r) => r,
                                Err(e) => {
                                    tracing::debug!(error = %e, "forge: malformed request from guest");
                                    return;
                                }
                            };
                        let resp = match cc.forge(&req).await {
                            Ok(r) => r,
                            Err(e) => {
                                tracing::warn!(error = %e, "forge: forward to coord failed");
                                engram_harness_proto::ForgeResponse::Error {
                                    message: format!("forge forward to coord failed: {e}"),
                                }
                            }
                        };
                        if let Err(e) = engram_harness_proto::write_msg(&mut stream, &resp).await {
                            tracing::debug!(error = %e, "forge: response write to guest failed");
                        }
                    });
                },
            );
            pooled.set_forge_sink(forge_sink);

            // ADR 0026 split-mode artifact forwarding. Like forge above,
            // the upload sink can't live on the host (needs the coord's
            // BlobStorage + broker map). But unlike forge it must NOT
            // buffer — a multi-hundred-MB video would OOM the host. So the
            // host is a pure byte relay: read the `UploadRequest` header
            // off the vsock, then stream the raw body straight into a
            // streaming POST to `/api/hosts/upload`, and write the coord's
            // `UploadResponse` back to the guest.
            let coord_client_for_upload = coord_client::HttpCoordClient::new(
                coord_url.clone(),
                self.cfg.coordinator_token.clone(),
            );
            let upload_sink: engram_core::traits::UploadSink = std::sync::Arc::new(move |stream| {
                let cc = coord_client_for_upload.clone();
                tokio::spawn(async move {
                    // Split so we can stream the body off the read half
                    // (handed to reqwest) while keeping the write half
                    // to reply on.
                    let (mut read_half, mut write_half) = tokio::io::split(stream);
                    let header: engram_harness_proto::UploadRequest =
                        match engram_harness_proto::read_msg(&mut read_half).await {
                            Ok(h) => h,
                            Err(e) => {
                                tracing::debug!(error = %e, "upload: malformed header from guest");
                                return;
                            }
                        };
                    let engram_harness_proto::UploadOp::ShareFile { size_bytes, .. } = &header.op;
                    // Cap the body read at the declared size so the
                    // guest can't over-feed the relay; the coord also
                    // enforces MAX_ARTIFACT_BYTES while draining.
                    let body = tokio::io::AsyncReadExt::take(read_half, *size_bytes);
                    let resp = match cc.upload_artifact(&header, body).await {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!(error = %e, "upload: forward to coord failed");
                            engram_harness_proto::UploadResponse::Error {
                                message: format!("upload forward to coord failed: {e}"),
                            }
                        }
                    };
                    if let Err(e) = engram_harness_proto::write_msg(&mut write_half, &resp).await {
                        tracing::debug!(error = %e, "upload: response write to guest failed");
                    }
                });
            });
            pooled.set_upload_sink(upload_sink);
            // ADR 0015 M5: warm-pool retired. LocalHostClient is just
            // the cold-create dispatch wrapper now.
            let local_host: Arc<dyn engram_core::traits::HostClient> = Arc::new(
                crate::host_client::LocalHostClient::new(pooled.clone(), harness_hub.clone()),
            );
            // ADR 0009 §2: populate `running_sandboxes` from
            // `backend.list()` on each heartbeat tick. The coord
            // intersects this against expected-active sessions to
            // detect divergence (missing sandbox → flip session per
            // §3). On `list()` error, ship an empty list — the
            // 3-strike grace window (15s) absorbs transient errors
            // without flipping live sessions.
            // Seed capacity once at startup (/proc/meminfo on Linux,
            // hw.memsize on macOS). Used by the scheduler's fit check —
            // without this the coord sees `total_mib=0` and rejects
            // every session, and rung-2 park never fires (ADR 0096).
            // TODO: `used_mib` accounting. Plumbing record_start /
            // record_stop hooks into the SandboxBackend is a follow-up;
            // until then the host always looks "fully available",
            // which lets the scheduler place but doesn't prevent
            // oversubscription.
            let host_total_mib = crate::resource::read_total_memory_mib();
            tracing::info!(
                host_total_mib,
                "capacity reporting seeded from /proc/meminfo"
            );
            // ADR 0007: surface the reaper to the coord. The host's
            // gRPC server exposes ReapMaterializeDir; the coord's
            // POST /api/admin/reap-materialize-dir fans it out
            // across every connected host. Hosts without a
            // chunk_store wiring leave admin_handler = None and
            // the gRPC method returns Unimplemented.
            let admin_handler: Option<
                std::sync::Arc<dyn engram_protocol::admin::HostAdminHandler>,
            > = self.chunk_store.as_ref().map(|(_, dir)| {
                std::sync::Arc::new(admin_handler::MaterializeDirReaper::new(dir.clone()))
                    as std::sync::Arc<dyn engram_protocol::admin::HostAdminHandler>
            });

            // ADR 0068: resolve once, up front — the SAME `ProbeInputs`
            // feeds both the register POST below and every heartbeat
            // tick's re-probe. `bundle_dir` is hoisted here (out of the
            // later ADR 0035 bundle-supervisor block) so the register
            // POST carries an honest `bundle_stamp` from the first
            // attempt, not `Unknown` until the first heartbeat.
            let bundle_dir = self.sandbox.bundle_dir().to_path_buf();
            let probe_inputs = capabilities::ProbeInputs {
                backend: self.cfg.backend_name.clone(),
                grpc_probe_addr: self.cfg.grpc_listen_addr,
                bundle_dir: bundle_dir.clone(),
                fc: self.fc_for_reattach.as_ref().map(|fc| {
                    let cfg = fc.config();
                    capabilities::FcProbeInputs {
                        firecracker_bin: cfg.firecracker_bin.clone(),
                        uffd_base_dir: cfg.uffd_base_dir.clone(),
                    }
                }),
            };

            // ADR 0013: per-process HttpCoordClient for HTTP traffic
            // (register, heartbeat, registry-auth, harness-events,
            // idle-eviction).
            let coord_client = coord_client::HttpCoordClient::new(
                coord_url.clone(),
                self.cfg.coordinator_token.clone(),
            );

            // ADR 0013: register over HTTP so the coord persists
            // our host_addr column. Mandatory — the coord-side
            // GrpcHostPool can't dispatch to us until host_addr
            // lands. Background loop with exponential backoff.
            //
            // Issue #224: capture this task's JoinHandle so the SIGTERM
            // path can ABORT it. It is the largest insert-after-sweep
            // source: it awaits `rehydrate_survivors` (a multi-second
            // pool-claim + GCS manifest fetch + netlink RECONFIGURE per
            // survivor) AFTER registration succeeds, and an unaborted
            // rehydrate can `nbd_sandboxes.insert` a survivor's live
            // data plane after `abandon_nbd_data_planes_for_shutdown`
            // already swept the (then-absent) entry. The terminal
            // `abandoning` flag in PooledBackend is the correctness
            // backstop, but aborting the task removes the race window
            // entirely for the common case.
            let mut registration_task: Option<tokio::task::JoinHandle<()>> = None;
            if let Some(advertise_addr) = self.cfg.grpc_advertise_addr.clone() {
                // hostname: prefer the env-supplied value (set by
                // the deployment / systemd unit on production
                // hosts), fall back to a synthetic host-<id>
                // string. The coord uses this for human display
                // only; uniqueness comes from host_id.
                let hostname = std::env::var("HOSTNAME")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("host-{host_id}"));
                // ADR 0068: probe BEFORE the first register, not after —
                // a host that can't yet prove `grpc_self_connect` /
                // `bundle_stamp` should say so on its very first row,
                // not claim `schema: 0` (soft-tolerated) until its
                // first heartbeat 5s later.
                let capabilities = capabilities::probe_all(&probe_inputs).await;
                let register_req = coord_client::RegisterRequest {
                    host_id,
                    hostname,
                    host_addr: advertise_addr.clone(),
                    agent_version: env!("CARGO_PKG_VERSION").to_string(),
                    wire_version: engram_protocol::WIRE_VERSION,
                    cloud_metadata: None,
                    capabilities,
                };
                let cc = coord_client.clone();
                let pooled_for_rehydrate = pooled.clone();
                registration_task = Some(tokio::spawn(async move {
                    let mut backoff = std::time::Duration::from_millis(500);
                    let cap = std::time::Duration::from_secs(30);
                    loop {
                        match cc.register(&register_req).await {
                            Ok(resp) => {
                                tracing::info!(
                                    host_id = %register_req.host_id,
                                    host_addr = %register_req.host_addr,
                                    rehydrate_sandboxes = resp.rehydrate_sandboxes.len(),
                                    "registered with coord via /api/hosts/register",
                                );
                                // ADR 0073: no harness-hub rebind pass here.
                                // Binding records live on disk under work_dir
                                // and survive the roll, so the survivor
                                // harness's re-dial validates against the
                                // durable record with zero rebuild step (the
                                // b9b28452/#447 gap is closed by construction,
                                // not by replay).
                                // Issue #229: the coord echoes the wire
                                // version it understands. Previously this was
                                // deserialized and silently dropped. A skew is
                                // a mixed-version fleet mid rolling deploy:
                                // log LOUD + emit a metric so it's visible
                                // here (not just at the coord), where the gRPC
                                // server refuses skewed RPCs (503) and the
                                // scheduler drains us off until we roll.
                                if resp.coord_wire_version != 0
                                    && resp.coord_wire_version != engram_protocol::WIRE_VERSION
                                {
                                    ::metrics::counter!("engram_host_wire_skew_at_register_total")
                                        .increment(1);
                                    tracing::error!(
                                        host_wire_version = engram_protocol::WIRE_VERSION,
                                        coord_wire_version = resp.coord_wire_version,
                                        "wire_version skew at register: this host and the \
                                         coordinator disagree on the bincode wire version \
                                         (mixed-version fleet during a rolling deploy); the \
                                         coordinator will drain this host from scheduling and \
                                         refuse RPCs to it until it rolls to the matching \
                                         version (issue #229)",
                                    );
                                }
                                // ADR 0016 Phase B commit 7: rehydrate
                                // any survivors PG knew about before
                                // our restart. Best-effort per row;
                                // one failure shouldn't block the
                                // rest. Linux-only NBD path; the
                                // PooledBackend method short-circuits
                                // on missing config.
                                #[cfg(target_os = "linux")]
                                {
                                    rehydrate_survivors(
                                        &pooled_for_rehydrate,
                                        &resp.rehydrate_sandboxes,
                                    )
                                    .await;
                                    // Local-first backstop (session 731df805,
                                    // 2026-07-17): the coord's list is derived
                                    // from PG status and CAN be wrong — it
                                    // omitted rung-parked survivors, nothing
                                    // claimed their NBD devices, and the sweep
                                    // below disconnected the live rootfs under
                                    // the paused guests. Re-serve any live
                                    // survivor the list missed from the durable
                                    // chain-head records before the sweep
                                    // snapshots the free pool.
                                    let (local_rehydrated, local_failed) =
                                        pooled_for_rehydrate.rehydrate_local_survivors().await;
                                    if local_rehydrated + local_failed > 0 {
                                        tracing::warn!(
                                            rehydrated = local_rehydrated,
                                            failed = local_failed,
                                            "local survivor rehydrate pass acted on sandboxes \
                                             the coordinator's rehydrate list missed",
                                        );
                                    }
                                    // ADR 0044 K2: stale-binding sweep AFTER
                                    // the survivors have claimed their slots
                                    // — only still-free devices are probed,
                                    // so a survivor's live (busy-by-design)
                                    // device can never be disconnected by
                                    // this pass. Replaces the old eager
                                    // sweep at pool construction, which
                                    // killed a survivor's disk in prod
                                    // (2026-06-11, /dev/nbd4).
                                    if let Some(nbd_pool) = pooled_for_rehydrate.nbd_pool() {
                                        // The sweep snapshots free paths,
                                        // then claim-then-disconnects each
                                        // candidate so a session that races
                                        // the (slow, 100ms/device) sweep for
                                        // the same slot can never have its
                                        // live binding torn out. Detached so
                                        // register returns promptly; the
                                        // claim is the correctness gate, not
                                        // ordering.
                                        let unclaimed = nbd_pool.free_paths().await;
                                        tokio::spawn(async move {
                                            disk_daemon::recover_stuck_nbd_devices(
                                                &nbd_pool, &unclaimed,
                                            )
                                            .await;
                                        });
                                    }
                                }
                                #[cfg(not(target_os = "linux"))]
                                {
                                    let _ = pooled_for_rehydrate;
                                    if !resp.rehydrate_sandboxes.is_empty() {
                                        tracing::info!(
                                            count = resp.rehydrate_sandboxes.len(),
                                            "rehydrate list returned but host isn't Linux/NBD; ignoring",
                                        );
                                    }
                                }
                                break;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    host_id = %register_req.host_id,
                                    error = %e,
                                    "host registration failed; will retry",
                                );
                                tokio::time::sleep(backoff).await;
                                backoff = (backoff * 2).min(cap);
                            }
                        }
                    }
                }));
            }

            // ADR 0095: shared "which images is this host ready to
            // serve" view — written by the prefetch supervisor (spawned
            // below), read by the heartbeat builder AND the peer-chunk
            // serve arm's BaseImage scope check. Created here because
            // the gRPC server needs it before the supervisor exists.
            let readiness = image_prefetch::ImageReadiness::new();

            // ADR 0095: the standing peer-chunk tier — serve state for
            // the gRPC arm (cache-less hosts serve nothing and answer
            // `unavailable`), plus the background scrubber that drains
            // the unverified-origin backlog bulk peer pulls create.
            let peer_serve = self
                .chunk_cache
                .clone()
                .map(|cache| peer_fill::PeerServe::new(cache, readiness.clone()));
            let _scrubber_task = self.chunk_cache.as_ref().map(|cache| {
                let bps = std::env::var("ENGRAM_CHUNK_SCRUB_BPS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .filter(|&v| v > 0)
                    .unwrap_or(256 * 1024 * 1024);
                cache.spawn_scrubber(bps)
            });

            // ADR 0013: boot the gRPC HostService server. The
            // coord's GrpcHostPool dials this address (populated
            // via /api/hosts/register) to dispatch coord→host
            // RPCs. Required in production; mode=all leaves
            // `grpc_listen_addr=None`.
            let grpc_task = self.cfg.grpc_listen_addr.map(|addr| {
                let local_for_grpc = local_host.clone();
                let admin_for_grpc = admin_handler.clone();
                let peer_for_grpc = peer_serve.clone();
                // ADR 0079: the per-session fencing-epoch high-water,
                // durable under work_dir like the binding records.
                let epochs = session_epochs::SessionEpochStore::open(
                    self.cfg.work_dir.join("epochs"),
                )
                .expect("open session epoch store under work_dir (ADR 0079)");
                tokio::spawn(async move {
                    if let Err(e) = grpc_server::boot(
                        addr,
                        local_for_grpc,
                        admin_for_grpc,
                        epochs,
                        peer_for_grpc,
                    )
                    .await
                    {
                        tracing::error!(addr = %addr, error = %e, "gRPC server terminated with error");
                    }
                })
            });

            // ADR 0045 C2: the post-copy page server (TCP 9102). Auth is
            // the per-export token, so the listener is inert until a C2
            // capture registers an export. The handle rides on the pooled
            // backend so the capture path can reach the registry.
            let _migrate_peer_task = self.cfg.migrate_peer_listen_addr.map(|addr| {
                let server = migrate_peer::PeerServer::new(
                    addr.port(),
                    self.chunk_cache.clone(),
                    self.chunk_store.as_ref().map(|(cs, _)| cs.clone()),
                );
                pooled.set_migrate_peer_server(server.clone());
                tokio::spawn(async move {
                    if let Err(e) = server.serve(addr).await {
                        tracing::error!(addr = %addr, error = %e, "migrate-peer server terminated with error");
                    }
                })
            });

            // Issue #540: the host RAM ledger. One long-lived instance
            // holds the pending-base-shm-charge registry; `image_prefetch`
            // registers a charge before each prewarm write and the
            // heartbeat tick's `sample()` reads it back out every tick.
            let ram_ledger = std::sync::Arc::new(ram_ledger::RamLedger::new());
            // Published once per heartbeat tick (issue #540). ADR 0073
            // deleted the channel's only local subscriber (the host
            // eviction tick's pressure gate — now coordinator-side,
            // reading the hosts.utilization these snapshots feed); the
            // tx side stays as the heartbeat's single measurement
            // source, and the watch shape stays so a future local
            // consumer (epic #545's rung residency) subscribes cheaply.
            let (ram_ledger_tx, _ram_ledger_rx) =
                tokio::sync::watch::channel(ram_ledger::RamLedgerSnapshot::default());

            // ADR 0015 M5: image-prefetch supervisor. Watches the
            // heartbeat-ack's `enabled_images` set and pulls the
            // chunked rootfs for any image not yet local on this
            // host. Updates the shared `ImageReadiness` (created above
            // with the peer-serve state) which the heartbeat builder
            // reads to populate `ready_images`. Spawned only when
            // chunk_store + image_cache are wired (production hosts;
            // dev-process backend lacks both and simply never reports
            // ready).
            // The heartbeat's `stages_images` field (below) must exactly
            // track whether the supervisor spawn below actually happens —
            // derive both from the same pure gate rather than letting
            // `enabled_images_tx.is_some()` implicitly stand in for it.
            let stages_images =
                stages_images_gate(self.chunk_store.is_some(), self.chunk_cache.is_some());
            let enabled_images_tx = match (
                self.chunk_store.as_ref().map(|(cs, _)| cs.clone()),
                self.chunk_cache.clone(),
            ) {
                (Some(chunk_store), Some(chunk_cache)) => {
                    // ADR 0022 Option A / ADR 0045 D3: when base-create is
                    // on the File backend (no substrate dir configured),
                    // the supervisor materializes each enabled image's
                    // contiguous per-template base memfile at residency —
                    // at the SAME path a base session.create restore reads
                    // (`pooled.snapshot_path_for(base_snapshot_id)`), so
                    // they agree by construction. With the substrate
                    // (ENGRAM_FC_UFFD_BASE_DIR set) fresh-creates go Uffd
                    // against the lazily-populated base shm instead, and
                    // the eager multi-second per-template materialization
                    // is retired along with the memfile it built.
                    // ADR 0092: the memfile is needed whenever fresh
                    // creates take the File path — derived (no substrate
                    // dir) OR explicitly overridden onto a substrate host
                    // (`ENGRAM_FC_FRESH_RESTORE_MODE=file`, the
                    // reclaimable-residency density config).
                    let fresh_is_file =
                        match engram_sandbox_firecracker::fresh_restore_mode_from_env() {
                            Some(m) => m == engram_sandbox_firecracker::RestoreMode::File,
                            None => engram_sandbox_firecracker::uffd_base_dir_from_env().is_none(),
                        };
                    let base_memfile_dir: Option<image_prefetch::SnapshotDirResolver> =
                        fresh_is_file.then(|| {
                            let p = pooled.clone();
                            let resolver: image_prefetch::SnapshotDirResolver =
                                std::sync::Arc::new(move |id| p.snapshot_path_for(id));
                            resolver
                        });
                    let (tx, _handle) = image_prefetch::spawn_supervisor(
                        chunk_store,
                        chunk_cache,
                        readiness.clone(),
                        base_memfile_dir,
                        ram_ledger.clone(),
                        self.base_shm_protected.clone(),
                    );
                    Some(tx)
                }
                _ => {
                    tracing::warn!(
                        "image_prefetch supervisor disabled: chunk_store / chunk_cache \
                         missing — host will never report ready_images and coord will 503 every session",
                    );
                    None
                }
            };
            debug_assert_eq!(
                enabled_images_tx.is_some(),
                stages_images,
                "stages_images_gate must track the image-prefetch supervisor's actual spawn condition",
            );

            // ADR 0035: bundle store + supervisor. The stamp is read once
            // (hosts are immutable; only a host-agent pod restart changes it). The
            // supervisor consumes the ack's `live_bundles` pin set:
            // prefetch missing pinned generations, sweep unpinned ones.
            // ADR 0062: read the stamp from the BACKEND's bundle dir — its
            // single source of truth (`SandboxBackend::bundle_dir`) — so the
            // `current_bundles` this host advertises is, by construction, the
            // exact dir restore will attach generations from. (Previously this
            // re-resolved `bundle_dir_from_env()` independently of the FC
            // config, which silently defaulted elsewhere — a host could then
            // advertise a sha it couldn't attach.) The supervisor materializes
            // pinned generations into the same dir.
            let bundle_ext = self.sandbox.bundle_file_ext();
            let current_bundles = bundles::read_stamp(&bundle_dir).await;
            let live_bundles_tx = self.chunk_store.as_ref().map(|(cs, _)| {
                bundles::spawn_supervisor(
                    bundles::BundleStore::new(
                        cs.blob_storage().clone(),
                        bundle_dir.clone(),
                        bundle_ext,
                    ),
                    current_bundles.clone(),
                )
            });

            // ADR 0068: the blocking "gRPC readiness gate" (ADR 0050 D)
            // that used to live here — a synchronous up-to-30s TCP-connect
            // retry loop with a "heartbeat anyway" optimism escape hatch
            // once it gave up — is RETIRED, not hardened. The heartbeat
            // loop below now starts immediately and its every-tick
            // `capabilities::probe_all` re-runs this exact TCP self-connect
            // as the `grpc_self_connect` capability; a still-binding
            // listener just reports `Failed` on this tick (and `Ok` on a
            // later one) instead of the host racing to heartbeat before it
            // can actually serve anything. The coordinator's placement
            // filter (`host_meets_capabilities`, PR 2) requires
            // `grpc_self_connect: Ok` for ANY placement, so the same
            // "coord can't dispatch before the listener is up" failure
            // mode this gate closed is now closed at the scheduler instead
            // of at host-agent startup.

            // ADR 0013: HTTP heartbeat loop. Posts
            // {capacity, local_snapshots, running_sandboxes,
            // draining, ready_images} every `heartbeat_interval` to
            // any coord pod via the L4-LB. Coord pod that receives
            // it updates host_registry, runs ADR 0009 reconcile, and
            // persists to PG. Idempotent across pods. The ack
            // carries `enabled_images`, which the supervisor diffs
            // against the local ready set.
            let heartbeat_interval = self.cfg.heartbeat_interval;
            let coord_for_heartbeat = coord_client.clone();
            let harness_hub_for_heartbeat = harness_hub.clone();
            let pooled_for_heartbeat = pooled.clone();
            let capture_jobs_for_heartbeat = capture_jobs.clone();
            let host_addr_for_heartbeat = self.cfg.grpc_advertise_addr.clone();
            let readiness_for_heartbeat = readiness.clone();
            let util_work_dir = self.cfg.work_dir.clone();
            let probe_inputs_for_heartbeat = probe_inputs.clone();
            let ram_ledger_for_heartbeat = ram_ledger.clone();
            let ram_ledger_tx_for_heartbeat = ram_ledger_tx;
            let stages_images_for_heartbeat = stages_images;
            // ENGRAM_FC_UFFD_BASE_DIR doesn't change at runtime; resolve
            // once outside the loop (same pattern `base_memfile_dir`
            // above uses).
            let base_shm_dir_for_heartbeat = engram_sandbox_firecracker::uffd_base_dir_from_env();
            // ADR 0090: consecutive heartbeat delivery failures. The
            // 2026-07-12 outbound-network wedge kept a healthy host out of
            // the registry for 1.5h with only DEBUG traces — the dead-host
            // detector fired (correctly), but nothing on THIS side ever
            // escalated, so no alert could exist. Past the threshold every
            // further failure logs ERROR and bumps the counter metric the
            // fleet alert rule watches. (No self-heal attempt here: the
            // wedge class is node-network-down — a re-register would fail
            // identically, and a returning network heals via the normal
            // heartbeat/registration path anyway.)
            const HEARTBEAT_FAILURES_BEFORE_ESCALATION: u32 = 6; // ~30s at 5s cadence
            let mut consecutive_heartbeat_failures: u32 = 0;
            let heartbeat_task = tokio::spawn(async move {
                let mut tick = tokio::time::interval(heartbeat_interval);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                // Observed disk/mem/cpu for the fleet view. Holds the
                // prior `/proc/stat` sample so CPU is a true interval
                // delta; the first tick reports cpu_pct=0.
                let mut util_probe = crate::util::UtilizationProbe::new();
                loop {
                    tick.tick().await;
                    // Issue #215: a `list()` error is "no information",
                    // not "no sandboxes running". Reporting empty would
                    // make the coord's ADR 0009 reconcile strike every
                    // active session on this host that tick. Carry an
                    // explicit `running_sandboxes_known = false` so the
                    // coord skips reconcile for this heartbeat instead of
                    // mistaking the empty set for a real running set.
                    let (running_sandboxes, running_sandboxes_known) = match pooled_for_heartbeat
                        .list()
                        .await
                    {
                        Ok(ids) => (ids, true),
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "backend.list() failed; heartbeat carries running_sandboxes_known=false (coord skips reconcile)",
                            );
                            (Vec::new(), false)
                        }
                    };
                    let running_count = running_sandboxes.len() as u32;
                    // ADR 0022 Option A: sample summed guest PSS/RSS across
                    // this host's live FC sandboxes — the density signal
                    // (Σpss/Σrss → ~1.0 under UFFD private copies, < 1.0 as
                    // File-backend siblings share one base memfile). Set
                    // both gauges every tick so a drained host reports 0,
                    // never a stale value. Error-tolerant + non-blocking
                    // (telemetry must not gate the workload); absent on
                    // VZ/non-Linux (guest_memory_stats → None).
                    let guest_mem = pooled_for_heartbeat.guest_memory_stats().await;
                    if let Some(mem) = &guest_mem {
                        ::metrics::gauge!(crate::metrics::SANDBOX_GUEST_PSS_BYTES)
                            .set(mem.pss_bytes as f64);
                        ::metrics::gauge!(crate::metrics::SANDBOX_GUEST_RSS_BYTES)
                            .set(mem.rss_bytes as f64);
                        ::metrics::gauge!(crate::metrics::SANDBOX_GUEST_PARKED_PSS_BYTES)
                            .set(mem.parked_pss_bytes as f64);
                        ::metrics::gauge!(crate::metrics::SANDBOX_GUEST_PARKED_RSS_BYTES)
                            .set(mem.parked_rss_bytes as f64);
                    }
                    // Issue #540: one RAM-ledger snapshot per tick — the
                    // meminfo read, the guest-PSS running/parked split
                    // above, and this ledger's own pending base-shm
                    // charges. Both `UtilizationProbe::sample` (below) and
                    // the idle-evictor's pressure gate (via the watch
                    // channel) derive their numbers from THIS snapshot, so
                    // the two can never disagree.
                    let ram_snapshot = ram_ledger_for_heartbeat.sample(
                        base_shm_dir_for_heartbeat.as_deref(),
                        &guest_mem.unwrap_or_default(),
                    );
                    ram_ledger_tx_for_heartbeat.send_replace(ram_snapshot);
                    // Issue #540 review finding 3: gate every ledger gauge
                    // on `measured` — VZ/Process/non-Linux backends (and a
                    // genuine `/proc/meminfo` parse failure) never took a
                    // real sample, so `ram_snapshot` is the all-zero
                    // default. Emitting that as a value would look like
                    // "this host has 0 MiB of everything" on a dashboard
                    // instead of "unmeasured" — matches the acceptance
                    // criterion's "gauges not emitted" posture.
                    if ram_snapshot.measured {
                        ::metrics::gauge!(crate::metrics::HOST_RAM_LEDGER_MIB, "category" => "running_vms")
                            .set(ram_snapshot.running_vm_pss_mib as f64);
                        ::metrics::gauge!(crate::metrics::HOST_RAM_LEDGER_MIB, "category" => "parked_paused")
                            .set(ram_snapshot.parked_paused_pss_mib as f64);
                        ::metrics::gauge!(crate::metrics::HOST_RAM_LEDGER_MIB, "category" => "base_shm")
                            .set(ram_snapshot.base_shm_mib as f64);
                        ::metrics::gauge!(crate::metrics::HOST_RAM_LEDGER_MIB, "category" => "base_shm_pending")
                            .set(ram_snapshot.base_shm_pending_mib as f64);
                        ::metrics::gauge!(crate::metrics::HOST_RAM_LEDGER_MIB, "category" => "parked_local_memfiles")
                            .set(ram_snapshot.parked_local_memfile_mib as f64);
                        ::metrics::gauge!(crate::metrics::HOST_RAM_ALLOCATABLE_MIB)
                            .set(ram_snapshot.allocatable_mib() as f64);
                        ::metrics::gauge!(crate::metrics::HOST_BASE_SHM_TMPFS_TOTAL_MIB)
                            .set(ram_snapshot.base_shm_tmpfs_total_mib as f64);
                        // Issue #540 review finding 5: this is the tmpfs
                        // mount's own `statfs` used figure (`f_blocks -
                        // f_bfree`), NOT `base_shm_mib` (this ledger's
                        // st_blocks walk over known files) — the two can
                        // legitimately diverge (an unlinked-but-open file,
                        // a stray subdir) and this gauge exists specifically
                        // to catch that divergence during an ENOSPC-class
                        // incident.
                        ::metrics::gauge!(crate::metrics::HOST_BASE_SHM_TMPFS_USED_MIB)
                            .set(ram_snapshot.base_shm_tmpfs_used_mib as f64);
                    }
                    // Issue #540: single emission site for this gauge (was
                    // previously only set inside the idle-evictor's
                    // pressure-aware branch, so it read stale/unset when
                    // that mode was off). Every tick now, unconditionally
                    // (still gated on `measured` via `free_pct()`'s own
                    // `None` return).
                    if let Some(pct) = ram_snapshot.free_pct() {
                        ::metrics::gauge!(crate::metrics::HOST_MEM_FREE_PCT).set(f64::from(pct));
                    }
                    // ADR 0028 Fix A: re-advertise every un-acked
                    // durable checkpoint record until a coord acks it
                    // into PG. Empty when checkpointing is disabled.
                    let checkpoint_records = match pooled_for_heartbeat.checkpoint_records_dir() {
                        Some(dir) => checkpoint::CheckpointRecord::load_all(&dir).await,
                        None => Vec::new(),
                    };
                    let checkpoints = checkpoint_records
                        .iter()
                        .map(|r| engram_protocol::heartbeat::CheckpointAdvert {
                            snapshot_id: r.snapshot_id,
                            session_id: r.session_id,
                            sandbox_id: r.sandbox_id,
                            image_version: r.image_version.clone(),
                            size_bytes: r.size_bytes,
                            disk_manifest: r.disk_manifest,
                            memory_manifest: r.memory_manifest,
                            aux_bundles: r.aux_bundles.clone(),
                            paused_at: r.paused_at,
                            captured_at: r.captured_at,
                            kind: r.kind,
                        })
                        .collect();
                    let utilization = util_probe.sample(&util_work_dir, &ram_snapshot);
                    // ADR 0068: re-run every probe this tick. Cheap
                    // (statfs/stat/one TCP connect/a memfd-backed uffd
                    // self-test; the FC binary version is cached after
                    // its first call) — this IS the retry for whatever
                    // the old blocking gRPC gate used to loop on.
                    let capabilities = capabilities::probe_all(&probe_inputs_for_heartbeat).await;
                    // ADR 0084 P1b: re-advertise every un-acked capture-job
                    // report (running progress + un-acked terminal outcomes)
                    // until the coord's ack names it — the
                    // `checkpoints`/`acked_checkpoints` pattern verbatim.
                    let capture_job_reports = capture_jobs_for_heartbeat.current_reports();
                    let req = coord_client::HeartbeatRequest {
                        capacity: engram_protocol::heartbeat::HostCapacityReport {
                            total_mib: host_total_mib,
                            used_mib: 0,
                            running_sandboxes: running_count,
                        },
                        running_sandboxes,
                        running_sandboxes_known,
                        draining: false,
                        host_addr: host_addr_for_heartbeat.clone(),
                        ready_images: readiness_for_heartbeat.snapshot(),
                        current_bundles: current_bundles.clone(),
                        checkpoints,
                        utilization,
                        // ADR 0048: the CPU packing budget's basis.
                        total_vcpus: std::thread::available_parallelism()
                            .map(|n| n.get() as u32)
                            .unwrap_or(0),
                        // ADR 0073 phase 4: harness-attach liveness for the
                        // coordinator's disagreement alarm.
                        harness_attached: harness_hub_for_heartbeat.attached_sandboxes(),
                        // Issue #229: report our bincode wire version so the
                        // coordinator drains us off scheduling on a skew
                        // (mixed-version fleet mid rolling deploy).
                        wire_version: engram_protocol::WIRE_VERSION,
                        // ADR 0036 amendment (issue #538): true iff the
                        // image-prefetch supervisor actually spawned
                        // (chunk_store + chunk_cache configured — see
                        // `stages_images_gate` + the gating a few hundred
                        // lines up). The scanner's prestage stage waits
                        // only on hosts reporting this.
                        stages_images: stages_images_for_heartbeat,
                        capabilities,
                        capture_job_reports,
                        // ADR 0090: re-advertised until the sandbox is
                        // destroyed; the coord enqueues evict_local (the op
                        // layer dedups repeats).
                        quarantined_survivors: pooled_for_heartbeat.quarantined_survivors(),
                        // ADR 0091: control-plane-dead guests; the coord
                        // flips their sessions Active → Unreachable.
                        unreachable_guests: pooled_for_heartbeat.unreachable_guests(),
                    };
                    match coord_for_heartbeat.heartbeat(host_id, &req).await {
                        Ok(resp) => {
                            if consecutive_heartbeat_failures
                                >= HEARTBEAT_FAILURES_BEFORE_ESCALATION
                            {
                                tracing::info!(
                                    host_id = %host_id,
                                    after_failures = consecutive_heartbeat_failures,
                                    "heartbeat delivery recovered",
                                );
                            }
                            consecutive_heartbeat_failures = 0;
                            if let Some(tx) = enabled_images_tx.as_ref() {
                                // ADR 0036 amendment (issue #538): the
                                // supervisor watches the UNION of
                                // enabled + prestaging images — it needs
                                // zero changes to warm a prestaging
                                // image, since from its point of view
                                // that's just another image to fetch,
                                // pin, and report ready. The coordinator's
                                // enable-scanner is the one reading
                                // `ready_images` back out during its wait.
                                let union = image_prefetch::union_image_refs(
                                    &resp.enabled_images,
                                    &resp.prestage_images,
                                );
                                // send_modify avoids notifying on
                                // no-op (same set as last tick).
                                tx.send_if_modified(|cur| {
                                    if *cur == union {
                                        false
                                    } else {
                                        *cur = union;
                                        true
                                    }
                                });
                            }
                            // ADR 0035 §5: hand the pin set to the
                            // bundle supervisor (same no-op dedup).
                            if let Some(tx) = live_bundles_tx.as_ref() {
                                tx.send_if_modified(|cur| {
                                    if *cur == resp.live_bundles {
                                        false
                                    } else {
                                        *cur = resp.live_bundles;
                                        true
                                    }
                                });
                            }
                            // ADR 0028 Fix A: the coord recorded these
                            // checkpoints into PG — drop the durable
                            // record files (the PG rows own the
                            // references now).
                            if !resp.acked_checkpoints.is_empty() {
                                if let Some(dir) = pooled_for_heartbeat.checkpoint_records_dir() {
                                    checkpoint::CheckpointRecord::delete_acked(
                                        &dir,
                                        &resp.acked_checkpoints,
                                    )
                                    .await;
                                }
                            }
                            // ADR 0084 P1b: the coord recorded these
                            // terminal capture-job reports into PG — drop
                            // the in-memory report + the durable record
                            // file (the PG row owns the reference now).
                            capture_jobs_for_heartbeat
                                .ack(&resp.acked_capture_jobs)
                                .await;
                            // `None` = the coord's assignment read failed
                            // (unknown) — take no action at all this tick.
                            // `Some` is authoritative: converge-cancel any
                            // still-running attempt absent from it (it was
                            // reassigned away / superseded — fenced-off
                            // work must not keep burning a VM), then claim
                            // anything new. For every assignment this host
                            // doesn't already own at the assigned epoch,
                            // claim the full dispatch and start the
                            // executor. Each claim is its own spawned task
                            // so a slow (or failing) claim round trip for
                            // one job never delays this tick's ack
                            // processing or the next heartbeat send.
                            let Some(assignments) = &resp.capture_assignments else {
                                continue;
                            };
                            capture_jobs_for_heartbeat.cancel_absent(assignments);
                            for assignment in assignments {
                                if !capture_jobs_for_heartbeat
                                    .should_claim(assignment.job_id, assignment.epoch)
                                {
                                    continue;
                                }
                                let coord = coord_for_heartbeat.clone();
                                let executor = capture_jobs_for_heartbeat.clone();
                                let job_id = assignment.job_id;
                                let epoch = assignment.epoch;
                                tokio::spawn(async move {
                                    match coord.claim_capture_job(host_id, job_id, epoch).await {
                                        Ok(spec) => executor.start(job_id, epoch, spec),
                                        Err(e) => {
                                            tracing::warn!(
                                                %job_id,
                                                epoch,
                                                error = %e,
                                                "claim_capture_job failed; will retry on a \
                                                 later heartbeat tick if the assignment persists",
                                            );
                                        }
                                    }
                                });
                            }
                        }
                        Err(e) => {
                            consecutive_heartbeat_failures =
                                consecutive_heartbeat_failures.saturating_add(1);
                            ::metrics::counter!(crate::metrics::HEARTBEAT_DELIVERY_FAILURES_TOTAL)
                                .increment(1);
                            if consecutive_heartbeat_failures
                                >= HEARTBEAT_FAILURES_BEFORE_ESCALATION
                            {
                                tracing::error!(
                                    host_id = %host_id,
                                    error = %e,
                                    consecutive = consecutive_heartbeat_failures,
                                    "heartbeat delivery failing repeatedly — this host is \
                                     (or will shortly be) invisible to the coordinator; \
                                     check node egress/network (ADR 0090)",
                                );
                            } else {
                                tracing::debug!(
                                    host_id = %host_id,
                                    error = %e,
                                    consecutive = consecutive_heartbeat_failures,
                                    "heartbeat POST failed; retrying next tick",
                                );
                            }
                        }
                    }
                }
            });

            // ADR 0073 phase 4: no host-side idle detection. The
            // coordinator's PG-derived idle detector (idle_detector.rs)
            // is the ONLY detection plane — same soft/hard TTL
            // semantics, sourced from the durable event log instead of
            // hub memory (which went amnesiac on every detach/restart;
            // the reason the L3 backstop existed). The disk-pressure
            // brake and pressure-aware gating moved with it, reading
            // the heartbeat-persisted hosts.utilization.
            // Issue #540 note: the RAM ledger still feeds the heartbeat's
            // allocatable_mib (its watch channel is consumed by the
            // heartbeat tick above); the deleted eviction tick was its
            // OTHER consumer, and that pressure gate now lives in the
            // coordinator's detector reading hosts.utilization — the
            // same ledger numbers, one hop later.

            shutdown_signal().await;
            heartbeat_task.abort();
            if let Some(t) = grpc_task {
                t.abort();
            }
            // Issue #224: abort the registration/rehydrate task BEFORE
            // the abandon sweep below. It is the largest insert-after-
            // sweep source — its `rehydrate_survivors` await window can
            // `nbd_sandboxes.insert` a survivor's live data plane after
            // the sweep, which `NbdHandle::Drop` would then netlink-
            // disconnect at process exit (the very "Disconnected due to
            // user request → successor's RECONFIGURE meets 'not
            // configured'" failure the K2 fix shipped to eliminate).
            // The terminal `abandoning` flag set inside the sweep is the
            // correctness backstop for the in-flight-gRPC and
            // already-past-abort-point cases; this abort removes the
            // common-case window outright.
            if let Some(t) = registration_task.take() {
                t.abort();
            }

            // ADR 0044 K2: detach-on-shutdown. The host-agent's VM
            // lifecycle is decoupled from its own process — FC (and any
            // uffd-handler) are spawned without `kill_on_drop` and live
            // in the node's PID namespace (`hostPID: true`), so they
            // survive this process exiting. We do NOT checkpoint or kill
            // them on the way out: the successor host-agent generation
            // pidfd-reattaches them off their on-disk `sandbox.json`
            // manifests (`live_attach::reattach_pass`), so a routine
            // DaemonSet pod restart / upgrade drops zero sessions.
            //
            // There is intentionally no SIGTERM-checkpoint pipeline.
            // Durability for an *uncontrolled* node loss rides the
            // always-on periodic checkpoint, not this path; a
            // *controlled* node drain migrates active sessions off first
            // (the K3 operator / admin endpoints), so by the time SIGTERM
            // lands there is nothing left here to lose.
            //
            // ADR 0044 K2: the NBD data planes must be ABANDONED, not
            // dropped — process unwind would otherwise run
            // NbdHandle::Drop → netlink disconnect and tear down the
            // survivors' disks on the way out (the 2026-06-12 canary:
            // "Disconnected due to user request" at old-pod SIGTERM,
            // then the successor's RECONFIGURE met "not configured").
            // Abandon kills only in-process resources; the kernel
            // config persists with guest I/O parked under
            // dead_conn_timeout until the successor reconfigures.
            #[cfg(target_os = "linux")]
            {
                // Issue #225: BEFORE abandoning the data planes, run a
                // bounded final disk-flush pass. NBD WRITEs are acked
                // from the in-RAM dirty tier and only made durable on
                // the FlushScheduler's ~30 s cadence; abandoning drops
                // that tier, so without this pass a routine pod roll
                // silently rolls a surviving guest's disk back by up to
                // one cadence window of ACKED writes. The pass drains +
                // uploads each survivor's dirty chunks and synchronously
                // republishes its live_disk_manifest so the successor
                // rehydrates from the current ref. It is budgeted against
                // the pod's terminationGracePeriodSeconds (minus headroom
                // for the abandon sweep + detach below); on overrun it
                // logs the still-dirty survivors loudly and proceeds. The
                // budget parse+default is the pure `plan_shutdown` decision
                // (ADR 0098 P4, Flow A) so the simulator drives the same
                // deadline arithmetic.
                let plan = engram_host_core::plan_shutdown(
                    std::env::var("ENGRAM_SHUTDOWN_FLUSH_BUDGET_SECS")
                        .ok()
                        .and_then(|v| v.parse::<f64>().ok()),
                );
                pooled
                    .flush_nbd_data_planes_for_shutdown(plan.flush_deadline)
                    .await;
                // The abandon sweep also exports any still-un-uploaded
                // dirty chunks to the node-local shutdown spool (2026-07-16
                // session-85e0298a RCA) so the successor adopts them
                // instead of rolling the live guest's disk back.
                let abandoned = pooled.abandon_nbd_data_planes_for_shutdown().await;
                if abandoned > 0 {
                    tracing::info!(
                        abandoned,
                        "SIGTERM: abandoned NBD data planes (kernel configs left \
                         alive for the successor to RECONFIGURE)",
                    );
                }
            }
            tracing::info!(
                "SIGTERM: detaching running microVMs (left alive for the successor \
                 host-agent to reattach); not checkpointing"
            );
        } else {
            tracing::info!("no coordinator_endpoint set; standalone dev mode (ctrl-c to exit)");
            shutdown_signal().await;
        }

        tracing::info!("host-agent shutting down");
        Ok(())
    }
}

/// ADR 0016 Phase B commit 7: rebuild chunked-disk tracking for
/// survivors PG already had bound to this host pre-restart. Called
/// from the host-registration success branch with the list coord
/// returned. Best-effort per row: a failure on one survivor logs
/// + continues; the host doesn't fail registration over it.
///
/// Each survivor gets:
/// 1. NBD slot + ChunkedDiskBackend rebuilt from the effective
///    disk manifest (newer of `live_disk_manifest_*` and the
///    latest recoverable snapshot — picked server-side by
///    `MetadataStore::list_resident_sandboxes_on_host_with_disk_manifest`).
/// 2. NBD daemon spawned.
/// 3. `nbd_sandboxes` entry installed.
/// 4. FlushScheduler spawned with the (session_id, sandbox_id)
///    pair. Continuous flush resumes immediately.
/// 5. `session_bindings` pre-populated so the publisher's
///    sandbox→session lookup finds the binding on the first
///    post-rehydrate tick.
///
/// Survivors without a chunked disk manifest (legacy sessions, or
/// hosts where the publisher never landed a value) are skipped
/// with a debug log. They run "untracked" until the next eviction
/// snapshot.
/// ADR 0036 amendment (issue #538): whether this host's heartbeat should
/// advertise `stages_images: true` — the queue-scanner's prestage-wait
/// gates on this flag, so it must exactly track whether the
/// image-prefetch supervisor actually spawned (`run`'s gate at the
/// `enabled_images_tx` match, which needs both a `ChunkStore` and a
/// `ChunkCache` configured). Pure so the boolean derivation is
/// unit-testable without wiring a real supervisor.
fn stages_images_gate(has_chunk_store: bool, has_chunk_cache: bool) -> bool {
    has_chunk_store && has_chunk_cache
}

#[cfg(target_os = "linux")]
async fn rehydrate_survivors(
    pooled: &Arc<pooled_backend::PooledBackend>,
    survivors: &[coord_client::RehydrateSandboxRef],
) {
    if survivors.is_empty() {
        return;
    }
    let mut rehydrated = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    for entry in survivors {
        let (Some(mid), Some(ver)) = (entry.disk_manifest_id, entry.disk_manifest_version) else {
            tracing::debug!(
                session_id = %entry.session_id,
                sandbox_id = %entry.sandbox_id,
                "rehydrate skipped: no disk manifest on the survivor row",
            );
            skipped += 1;
            continue;
        };
        let manifest_ref = engram_core::types::manifest::ManifestRef {
            manifest_id: mid,
            version: ver,
        };
        match pooled
            .rehydrate_sandbox(entry.session_id, entry.sandbox_id, manifest_ref)
            .await
        {
            Ok(true) => rehydrated += 1,
            Ok(false) => skipped += 1,
            Err(e) => {
                tracing::warn!(
                    session_id = %entry.session_id,
                    sandbox_id = %entry.sandbox_id,
                    manifest = %manifest_ref,
                    error = %e,
                    "rehydrate failed for survivor sandbox; continuing with the rest",
                );
                failed += 1;
            }
        }
    }
    tracing::info!(
        total = survivors.len(),
        rehydrated,
        skipped,
        failed,
        "Phase B rehydration pass complete",
    );
}

/// Await either SIGINT (ctrl-c) or SIGTERM (Kubernetes shutdown).
/// On non-unix platforms, falls back to ctrl-c only.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "failed to install SIGTERM handler; ctrl-c only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("host-agent received ctrl-c");
            }
            _ = term.recv() => {
                tracing::info!("host-agent received SIGTERM");
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[derive(Debug)]
pub enum HostAgentError {
    Config(String),
    Io(std::io::Error),
    Backend(engram_core::BackendError),
}

impl std::fmt::Display for HostAgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(msg) => write!(f, "host-agent config error: {msg}"),
            Self::Io(e) => write!(f, "host-agent io error: {e}"),
            Self::Backend(e) => write!(f, "host-agent backend error: {e}"),
        }
    }
}

impl std::error::Error for HostAgentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Backend(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for HostAgentError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::SessionId;

    fn noop_hub_over(dir: &std::path::Path) -> harness::HarnessHub {
        harness::HarnessHub::new(
            std::sync::Arc::new(|_, _, _| Box::new(Box::pin(async {}))),
            bindings::BindingStore::open(dir).expect("open binding store"),
        )
    }

    /// ADR 0073 replacement for the retired `rebind_survivor_sessions`
    /// coverage (regression b9b28452 / #447): a host-agent restart must
    /// accept a survivor harness's re-dial with ZERO rebuild pass. The
    /// binding record on disk IS the routing — a hub constructed fresh
    /// over a pre-populated bindings dir (what a restarted process
    /// sees) resolves the survivor immediately.
    #[test]
    fn fresh_hub_over_surviving_bindings_dir_routes_survivors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::new();
        let sandbox_id = SandboxId::new();

        // "Old" process binds, then dies (dropped hub).
        {
            let hub = noop_hub_over(dir.path());
            hub.bind_session(session_id, sandbox_id, 1).expect("bind@1");
        }

        // "New" process: fresh hub, same dir, no coordinator involved.
        let hub = noop_hub_over(dir.path());
        assert_eq!(
            hub.bound_sandbox(session_id),
            Some(sandbox_id),
            "survivor binding must be readable by a restarted host-agent",
        );
    }

    /// An empty bindings dir routes nothing — parity with the old
    /// empty-list no-op behavior.
    #[test]
    fn fresh_hub_over_empty_bindings_dir_routes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hub = noop_hub_over(dir.path());
        assert!(hub.bound_sandbox(SessionId::new()).is_none());
    }

    /// The image-prefetch supervisor only spawns (and this host only
    /// reports `stages_images: true`) when BOTH a chunk_store and a
    /// chunk_cache are configured — a dev `Process` backend missing
    /// either must never advertise it stages images, or the coordinator's
    /// enable-scanner would wait forever on a prestage this host can't do.
    #[test]
    fn stages_images_gate_requires_both_chunk_store_and_chunk_cache() {
        assert!(stages_images_gate(true, true));
        assert!(!stages_images_gate(true, false));
        assert!(!stages_images_gate(false, true));
        assert!(!stages_images_gate(false, false));
    }
}
