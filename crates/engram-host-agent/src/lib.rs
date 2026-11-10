//! Per-host daemon. Wraps a [`SandboxBackend`], composes the
//! chunked-OCI image cache / session volumes / egress proxy, runs the
//! snapshot manager and resource governor, and heartbeats the
//! coordinator with capacity + local-snapshot state.
//!
//! Phase 1 wires create/exec/destroy through the SandboxBackend trait —
//! currently `engram-sandbox-process` for fast dev loops, with
//! `engram-sandbox-firecracker` filling in the production path. Phase 2
//! lights up the snapshot manager. Phase 3 turns heartbeats into a real
//! gRPC channel; today they go through an in-process trait object so
//! the `--mode=all` single-binary path works.

use std::sync::Arc;

use engram_chunk_store::{ChunkCache, ChunkStore};
use engram_core::traits::{CloudBackend, SandboxBackend};
use engram_core::SandboxId;
// ADR 0098 Phase 2: the teardown-reconcile / TTL-sweep / reattach-source
// paths hold the coordinator via the `CoordControlPlane` seam.
use engram_host_core::CoordControlPlane;

use crate::image_cache::ImageCache;

/// The session-disk pool's state for the capability vector, and its
/// capacity when it answered.
async fn probe_pool(
    disk: Option<&dyn engram_host_core::SessionDisk>,
) -> (
    capabilities::SessionDiskState,
    Option<engram_host_core::Capacity>,
) {
    let Some(disk) = disk else {
        return (capabilities::SessionDiskState::Absent, None);
    };
    match disk.capacity().await {
        Ok(cap) => (capabilities::SessionDiskState::Ready, Some(cap)),
        Err(e) => (capabilities::SessionDiskState::Faulted(e.to_string()), None),
    }
}

/// Purge released sandboxes' kept checkpoint volumes until the pool
/// holds twice the placement floor free, so the host stays placeable
/// before the coordinator stops choosing it.
async fn reap_kept_checkpoints(volumes: &engram_host_core::SessionVolumes) {
    match volumes
        .reap(2 * engram_core::types::host::pool_data_floor_bytes())
        .await
    {
        Ok(report) => {
            if !report.deleted.is_empty() {
                tracing::info!(
                    deleted = report.deleted.len(),
                    "reaped kept checkpoint volumes for pool space"
                );
            }
            for (name, error) in &report.failed {
                tracing::warn!(volume = %name, %error, "kept checkpoint reap: purge failed");
            }
        }
        Err(error) => tracing::warn!(%error, "kept checkpoint reap failed"),
    }
}

pub mod base_shm_gc;
pub mod bindings;
pub mod bundles;
pub mod capabilities;
pub mod capture_job;
pub(crate) mod capture_lock;
pub mod checkpoint;
pub mod config;
pub mod coord_client;
pub mod dirty_map;
pub mod durable_envelope;
pub mod durable_record;
pub mod egress;
pub mod eviction_finalize;
pub mod grpc_server;
pub mod harness;
pub mod host_client;
pub mod migrate_peer;
pub mod migration;
pub mod page_idle;
pub mod peer_fill;
pub mod session_epochs;
pub(crate) mod snapshot_post;
pub mod substrate_server;
pub use host_client::{IdleEvictionHarness, LocalHostClient};
#[cfg(target_os = "linux")]
pub mod blk_source;
#[cfg(target_os = "linux")]
pub mod heap_profile;
pub mod heartbeat;
pub mod idle_evictor;
pub mod image_cache;
pub mod image_prefetch;
pub mod image_volumes;
pub mod live_attach;
pub mod materialize;
pub mod metrics;
pub mod pooled_backend;
pub(crate) mod postcopy_finish;
pub mod proxy_port;
pub mod proxy_shell;
pub mod proxyd_client;
pub mod ram_ledger;
pub mod resource;
pub mod teardown_reconcile;
mod time_source;
pub mod util;
pub mod warm_progress;

pub use config::{HostAgentConfig, DEFAULT_DISK_HYDRATION_WINDOW_CHUNKS};

pub struct HostAgent {
    pub cfg: HostAgentConfig,
    managed_ownership: Option<Arc<managed_policy::ManagedOwnership>>,
    managed_backend: Option<managed_boot::ManagedBackend>,
    pub sandbox: Arc<dyn SandboxBackend>,
    pub cloud: Arc<dyn CloudBackend>,
    /// Local egress proxy. The production binary refuses to start
    /// without one; `None` is for in-process tests and the VZ/Process
    /// backends, whose guests have no internet-facing network.
    pub egress: Option<Arc<egress::HostEgress>>,
    /// `None` keeps the OCI-pulled `rootfs.ext4` path. Always
    /// `fill.store()` when set.
    pub chunk_store: Option<ChunkStore>,
    /// The host's chunk fill engine, over `chunk_store`.
    pub fill: Option<engram_fill::FillEngine>,
    /// Per-host OCI image cache. `None` means sessions must arrive with
    /// `spec.rootfs_source` set (the dev-only path).
    pub image_cache: Option<ImageCache>,
    /// NVMe-backed chunk cache shared across manifests.
    pub chunk_cache: Option<ChunkCache>,
    /// The concrete Firecracker backend, for the startup reattach pass.
    /// `sandbox` cannot be downcast (the trait does not extend `Any`).
    /// `None` starts clean-slate.
    pub fc_for_reattach: Option<Arc<engram_sandbox_firecracker::FirecrackerBackend>>,
    /// Where image and session volumes live. `None` keeps readiness on
    /// the chunk cache alone and leaves the disks to the backend.
    pub session_disk: Option<Arc<dyn engram_host_core::SessionDisk>>,
    /// This host's stable id. `None` generates one at run time.
    pub host_id: Option<engram_core::HostId>,
    /// The base-shm sweeper's keep-set. The image-prefetch supervisor
    /// publishes into it; inert unless a sweeper holds the same handle.
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
            managed_ownership: None,
            managed_backend: None,
            sandbox,
            cloud,
            egress: None,
            chunk_store: None,
            fill: None,
            image_cache: None,
            chunk_cache: None,
            fc_for_reattach: None,
            session_disk: None,
            host_id: None,
            base_shm_protected: base_shm_gc::ProtectedPaths::new(),
        }
    }

    pub fn with_egress(mut self, egress: Arc<egress::HostEgress>) -> Self {
        self.egress = Some(egress);
        self
    }

    /// [`Self::with_fill`] with an engine of default limits.
    pub fn with_chunk_store(self, chunk_store: ChunkStore) -> Self {
        self.with_fill(engram_fill::FillEngine::new(
            chunk_store,
            engram_fill::Config::default(),
        ))
    }

    pub fn with_fill(mut self, fill: engram_fill::FillEngine) -> Self {
        self.chunk_store = Some(fill.store().clone());
        self.fill = Some(fill);
        self
    }

    pub fn with_image_cache(mut self, cache: ImageCache) -> Self {
        self.image_cache = Some(cache);
        self
    }

    pub fn with_chunk_cache(mut self, cache: ChunkCache) -> Self {
        self.chunk_cache = Some(cache);
        self
    }

    pub fn with_fc_reattach(
        mut self,
        fc: Arc<engram_sandbox_firecracker::FirecrackerBackend>,
    ) -> Self {
        self.fc_for_reattach = Some(fc);
        self
    }

    pub fn with_session_disk(mut self, disk: Arc<dyn engram_host_core::SessionDisk>) -> Self {
        self.session_disk = Some(disk);
        self
    }

    pub fn with_host_id(mut self, id: engram_core::HostId) -> Self {
        self.host_id = Some(id);
        self
    }

    pub fn with_base_shm_protected(mut self, protected: Arc<base_shm_gc::ProtectedPaths>) -> Self {
        self.base_shm_protected = protected;
        self
    }

    pub fn with_managed_backend(mut self, backend: managed_boot::ManagedBackend) -> Self {
        self.managed_backend = Some(backend);
        self
    }

    pub fn with_managed_ownership(
        mut self,
        ownership: Option<Arc<managed_policy::ManagedOwnership>>,
    ) -> Self {
        self.managed_ownership = ownership;
        self
    }

    /// Run until ctrl-c: the reattach pass, the background drivers, and
    /// the coordinator dialer when `coordinator_endpoint` is set.
    pub async fn run(self) -> Result<(), HostAgentError> {
        let managed_ownership = match self.managed_ownership.clone() {
            Some(owner) => Some(owner),
            None => managed_policy::ManagedOwnership::for_mode(
                &self.cfg.work_dir.join("bindings"),
                self.cfg.managed_tls.is_some(),
            )?,
        };
        let managed_server = if let Some(tls) = &self.cfg.managed_tls {
            if self.cfg.grpc_listen_addr.is_none()
                || !self
                    .cfg
                    .grpc_advertise_addr
                    .as_ref()
                    .is_some_and(|a| a.starts_with("https://"))
                || self.egress.is_none()
                || self.host_id.is_none()
            {
                return Err(HostAgentError::Config("managed host mode requires an HTTPS lifecycle listener, stable host identity, and egress proxy".into()));
            }
            Some(
                grpc_server::PreparedHostServer::bind(
                    self.cfg.grpc_listen_addr.expect("managed listener checked"),
                    Some(tls.clone()),
                )
                .map_err(|error| HostAgentError::Config(error.to_string()))?,
            )
        } else {
            None
        };
        tracing::info!(?self.cfg.work_dir, "host-agent starting");
        let _ = self.cloud.host_metadata().await;

        // Reattach before the first heartbeat, so the coordinator sees
        // surviving VMs as continuously present. A host-agent restart
        // detaches Firecracker VMs rather than killing them; the new
        // generation must adopt them or they leak.
        // `live_set_known` is false only when the pass failed: an
        // incomplete live set must never widen the residue sweep below.
        let (live_set_known, reattached_roles): (bool, Vec<(SandboxId, migration::MigrationRole)>) =
            match self.fc_for_reattach.as_ref() {
                Some(fc) => match live_attach::reattach_pass(&self.cfg.work_dir, fc).await {
                    Ok(report) => {
                        tracing::info!("{}", report.summary());
                        // Sandboxes mid-post-copy when the previous generation
                        // died; their fences re-arm once the pooled backend exists.
                        (true, live_attach::scan_migration_roles(&self.cfg.work_dir))
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "live-attach pass failed; continuing with clean-slate startup"
                        );
                        (false, Vec::new())
                    }
                },
                // No VM survives a restart of the other backends: every
                // uuid entry in the work dir is residue.
                None => (true, Vec::new()),
            };
        // Sweep jail dirs, vsock sockets, and canonical entries whose
        // sandbox did not survive, before the coordinator can create or
        // resume anything.
        if live_set_known {
            let live: std::collections::HashSet<SandboxId> = self
                .sandbox
                .list()
                .await
                .unwrap_or_default()
                .into_iter()
                .collect();
            match live_attach::sweep_dead_sandbox_residue(&self.cfg.work_dir, &live) {
                Ok(report) => {
                    if report.jail_dirs
                        + report.vsock_files
                        + report.canonical_entries
                        + report.swap_backings
                        > 0
                    {
                        tracing::info!(
                            jail_dirs = report.jail_dirs,
                            vsock_files = report.vsock_files,
                            canonical_entries = report.canonical_entries,
                            swap_backings = report.swap_backings,
                            live = live.len(),
                            "swept dead sandbox residue",
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "dead-sandbox residue sweep failed");
                }
            }
        }

        if let Some(coord_url) = self.cfg.coordinator_endpoint.clone() {
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
                if let Some(fill) = self.fill.clone() {
                    if let Some(disk) = self.session_disk.clone() {
                        let volumes = engram_host_core::SessionVolumes::new(disk, fill.clone());
                        #[cfg(target_os = "linux")]
                        let volumes = match blk_source::available() {
                            Ok(()) => volumes.with_sources(Arc::new(blk_source::UblkSources {
                                window_ahead: self.cfg.disk_hydration_window_chunks,
                            })),
                            Err(error) => {
                                tracing::error!(
                                    %error,
                                    "no ublk control device (is ublk_drv loaded?): every resume \
                                     writes its whole disk before the guest starts"
                                );
                                volumes
                            }
                        };
                        p = p.with_session_volumes(Arc::new(volumes));
                    }
                    p = p.with_fill(fill);
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
                    // `MaterializeImage` pulls with the image cache's
                    // auth resolver.
                    p = p.with_oci_client(ic.oci_client());
                    p = p.with_image_cache(ic);
                }
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
                let bindings =
                    crate::bindings::BindingStore::open(self.cfg.work_dir.join("bindings"))
                        .map_err(HostAgentError::Io)?;
                arc.reconcile_volumes(&bindings).await;
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
            // ADR 0045: the migration export TTL sweep — the
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
                    let mut tick = engram_core::periodic(std::time::Duration::from_secs(30));
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
                                if let Err(e) = pooled_iter
                                    .run_export_ttl_verdict(sandbox_id, &export_id_iter, verdict)
                                    .await
                                {
                                    tracing::warn!(%sandbox_id, error = %e,
                                        "export TTL verdict failed");
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
            // ADR 0116 A5: only the UNBOUND arm asks the coordinator
            // ("does ANY session own this?") — a locally BOUND sandbox
            // is owned until its tombstone arrives on the heartbeat (the
            // polling bound arm + the ORPHAN_STRIKES verdict counter are
            // retired). A coordinator-confirmed no-owner answer destroys
            // locally once the create→bind age grace clears.
            {
                // ADR 0098 P3: the tick body is now
                // `teardown_reconcile::reconcile_once` (pure classify +
                // focused backend seam, driven directly by the host-internal
                // simulator). This wrapper keeps only the interval cadence +
                // the caller-owned first-seen ledger; `reconcile_once` returns
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
                    let mut first_seen: std::collections::HashMap<SandboxId, std::time::Duration> =
                        std::collections::HashMap::new();
                    let mut tick = engram_core::periodic(RECONCILE_INTERVAL);
                    // The age gate's monotonic origin. The sim passes its
                    // own mark at the reconcile_once seam; this wrapper is
                    // prod-only.
                    let epoch = crate::time_source::metrics_now();
                    loop {
                        tick.tick().await;
                        if let Err(e) = reconcile_once(
                            &*reap_backend,
                            &*coord_for_reap,
                            host_id_for_reap,
                            &mut first_seen,
                            crate::time_source::metrics_now().duration_since(epoch),
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
                            if let Err(e) = pooled_for_reap.destroy(sandbox_id).await {
                                tracing::warn!(%sandbox_id, error = %e,
                                    "reattached post-copy dest reap failed");
                            }
                        });
                    }
                    // A held snapshot source has the same contract as a frozen
                    // post-copy source: its state may already run elsewhere, so
                    // it never self-resumes; the coordinator's rollback or
                    // release ends it, and the ownership rule reaps a leftover.
                    migration::MigrationRole::PostCopySource
                    | migration::MigrationRole::HeldSource => {
                        tracing::warn!(%sandbox_id, ?role,
                            "reattached frozen SOURCE: staying paused under the ownership rule (never self-resumes)");
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
                                engram_core::periodic(std::time::Duration::from_secs(30));
                            loop {
                                tick.tick().await;
                                // Issue #216 Gap 3: NEVER destroy on a
                                // missing in-memory binding. The binding
                                // arrives with the register task's
                                // response, which races this
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
            let pooled_for_events = pooled.clone();
            let event_sink =
                crate::harness::event_sink_to(move |session_id, sandbox_id, ev, delivery| {
                    pooled_for_events.note_harness_event(sandbox_id, &ev);
                    let cc = coord_client_for_events.clone();
                    let events_clock = events_clock.clone();
                    async move {
                        let req = coord_client::HarnessEventRequest {
                            sandbox_id,
                            event: ev,
                            at: events_clock.now_utc(),
                            delivery,
                        };
                        cc.harness_event(session_id, &req)
                            .await
                            .map_err(|e| crate::harness::SinkError(e.to_string()))
                    }
                });
            let bindings = crate::bindings::BindingStore::open(self.cfg.work_dir.join("bindings"))
                .expect("open binding store under work_dir (ADR 0073)");
            let harness_hub =
                std::sync::Arc::new(crate::harness::HarnessHub::new(event_sink, bindings));
            let managed_policy = if self.cfg.managed_tls.is_some() {
                use engram_core::traits::Entropy;
                let managed = crate::managed_policy::ManagedPolicyHost::new(
                    host_id,
                    engram_core::traits::OsEntropy.uuid().to_string(),
                    harness_hub.binding_store(),
                    pooled.clone(),
                    self.egress.clone().expect("managed configuration checked"),
                    managed_ownership
                        .clone()
                        .expect("managed ownership acquired"),
                )?;
                Some(Arc::new(managed))
            } else {
                None
            };
            if let Some(managed) = &managed_policy {
                managed
                    .recover_all()
                    .await
                    .map_err(|e| HostAgentError::Config(e.to_string()))?;
            }

            // Plumb the hub into the FC/VZ backend's vsock-accept
            // sink so inbound harness connections land on the local
            // hub's adapter loop. Without this the FC backend drops
            // every dial with "no sink registered" — the bug Phase 2
            // closes.
            let sink_hub = harness_hub.clone();
            let sink: engram_core::traits::HarnessSink =
                std::sync::Arc::new(move |stream| sink_hub.accept_via_session_lookup(stream));
            pooled.set_harness_sink(sink);

            // ADR 0111 + ADR 0121: sync the node-local egress daemon
            // from the policies persisted beside the binding records.
            // The reattach pass above re-adopted the surviving VMs; an
            // adopted daemon keeps their registrations live through the
            // roll, and this sync makes the daemon's registry agree
            // with THIS generation's live set either way (a fresh
            // daemon gets the full replay; an adopted one gets pruned
            // of entries whose VM died with the old pod).
            match harness_hub.persisted_egress_policies() {
                Ok(policies) => pooled.rebuild_egress_from_policies(policies).await,
                Err(e) => {
                    if self.cfg.managed_tls.is_some() {
                        return Err(HostAgentError::Io(e));
                    }
                    tracing::warn!(error = %e, "list persisted egress policies failed");
                }
            }
            if let Some(egress) = self.egress.as_ref() {
                // ADR 0121 §6: the app-relay dial-back server — the
                // daemon asks THIS process to open guest streams (the
                // sandbox backend + ADR 0066 handshake live here).
                let dialer = std::sync::Arc::new(crate::proxy_port::BackendGuestPortDialer::new(
                    pooled.clone() as std::sync::Arc<dyn engram_core::traits::SandboxBackend>,
                ));
                if let Err(e) = crate::proxy_port::spawn_dialback_server(&self.cfg.work_dir, dialer)
                {
                    tracing::error!(error = %e, "egress dial-back server failed to start; app relay degraded");
                }
                // ADR 0121: event-driven daemon supervision — on the
                // daemon's exit event, respawn + re-sync from the
                // persisted policies. Not armed until here because the
                // replay needs the pooled backend.
                let hub_for_replay = harness_hub.clone();
                let managed_for_replay = managed_policy.clone();
                let pooled_for_replay = pooled.clone();
                egress.spawn_supervisor(std::sync::Arc::new(move || {
                    let hub = hub_for_replay.clone();
                    let managed = managed_for_replay.clone();
                    let pooled = pooled_for_replay.clone();
                    Box::pin(async move {
                        if let Some(managed) = managed {
                            if let Err(error) = managed.recover_all().await {
                                tracing::error!(%error, "managed proxy recovery failed; admission remains closed");
                                return;
                            }
                        }
                        match hub.persisted_egress_policies() {
                            Ok(policies) => pooled.rebuild_egress_from_policies(policies).await,
                            Err(e) => {
                                tracing::warn!(error = %e, "list persisted egress policies failed on daemon respawn");
                            }
                        }
                    })
                }));
            }

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
                    // Cap the body read at the declared size so the
                    // guest can't over-feed the relay; the coord also
                    // enforces MAX_ARTIFACT_BYTES while draining.
                    let body = tokio::io::AsyncReadExt::take(read_half, header.op.size_bytes());
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
            let mut local = crate::host_client::LocalHostClient::new(
                pooled.clone(),
                harness_hub.clone(),
                self.cfg.idle_eviction_harness,
            );
            if let Some(managed) = managed_policy {
                local = local.with_managed_policy(managed);
            }
            let mut managed_boot_host = None;
            if self.cfg.managed_tls.is_some() {
                use engram_core::traits::Entropy;
                let backend = self.managed_backend.clone().ok_or_else(|| {
                    HostAgentError::Config("managed backend is not installed".into())
                })?;
                let fill = self
                    .fill
                    .as_ref()
                    .ok_or_else(|| {
                        HostAgentError::Config("managed disk storage is not installed".into())
                    })?
                    .clone();
                let boot = managed_boot::ManagedBootHost::new(
                    managed_boot::ManagedBootHostConfig {
                        host_id,
                        incarnation: engram_core::traits::OsEntropy.uuid().to_string(),
                        directory: self.cfg.work_dir.join("managed-boots"),
                    },
                    backend,
                    pooled.clone(),
                    self.egress.clone().expect("managed egress checked"),
                    harness_hub.clone(),
                    fill,
                )
                .map_err(|e| HostAgentError::Config(e.to_string()))?;
                let boot = Arc::new(boot);
                managed_boot_host = Some(boot.clone());
                local = local.with_managed_boot(boot);
            }
            let tombstone_host = Arc::new(local);
            let local_host: Arc<dyn engram_core::traits::HostClient> = tombstone_host.clone();
            // The coordinator compares known inventory against active sessions.
            // A failed read reports unknown inventory and cannot prove absence.
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
            // ADR 0068: resolve once, up front — the SAME `ProbeInputs`
            // feeds both the register POST below and every heartbeat
            // tick's re-probe. `bundle_dir` is hoisted here (out of the
            // later ADR 0035 bundle-supervisor block) so the register
            // POST carries an honest `bundle_stamp` from the first
            // attempt, not `Unknown` until the first heartbeat.
            let bundle_dir = self.sandbox.bundle_dir().to_path_buf();
            let session_volumes = pooled.session_volumes().cloned();
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
                session_disk: probe_pool(session_volumes.as_deref().map(|v| v.disk()))
                    .await
                    .0,
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
            // The SIGTERM path aborts this task.
            //
            // The gRPC listener is bound first: the register POST carries
            // the `grpc_self_connect` probe, and a probe that runs before
            // the bind fails, which keeps the host out of placement until
            // the next heartbeat. The server starts serving it below.
            let host_server = match managed_server {
                Some(server) => Some(server),
                None => self.cfg.grpc_listen_addr.and_then(|addr| {
                    grpc_server::PreparedHostServer::bind(addr, None)
                        .inspect_err(|e| {
                            tracing::error!(addr = %addr, error = %e, "gRPC listener bind failed");
                        })
                        .ok()
                }),
            };
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
                    // ADR 0116 A-D2: adopt (read + delete) the
                    // predecessor's handoff marker — tells the coord
                    // this register is a roll succession, and how long
                    // the swap really took.
                    handoff_marker: adopt_handoff_marker(&self.cfg.work_dir),
                };
                let cc = coord_client.clone();
                let pooled_for_bindings = pooled.clone();
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
                                // durable record with zero rebuild step (#447
                                // is closed by construction, not by replay).
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
                                // Survivors run on; the coordinator's row
                                // is the session binding their next
                                // checkpoint publishes under.
                                for entry in &resp.rehydrate_sandboxes {
                                    pooled_for_bindings
                                        .record_session_binding(entry.sandbox_id, entry.session_id);
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
            // `unavailable`).
            let peer_serve = self
                .chunk_cache
                .clone()
                .map(|cache| peer_fill::PeerServe::new(cache, readiness.clone()));

            // ADR 0013: boot the gRPC HostService server. The
            // coord's GrpcHostPool dials this address (populated
            // via /api/hosts/register) to dispatch coord→host
            // RPCs. Required in production; mode=all leaves
            // `grpc_listen_addr=None`.
            let grpc_task = host_server.map(|server| {
                let local_for_grpc = local_host.clone();
                let peer_for_grpc = peer_serve.clone();
                // ADR 0079: the per-session fencing-epoch high-water,
                // durable under work_dir like the binding records.
                let epochs =
                    session_epochs::SessionEpochStore::open(self.cfg.work_dir.join("epochs"))
                        .expect("open session epoch store under work_dir (ADR 0079)");
                tokio::spawn(async move {
                    if let Err(e) = server.serve(local_for_grpc, epochs, peer_for_grpc).await {
                        tracing::error!(error = %e, "gRPC server terminated with error");
                    }
                })
            });

            // ADR 0045 C2: the post-copy page server (TCP 9102). Auth is
            // the per-export token, so the listener is inert until a C2
            // capture registers an export. The handle rides on the pooled
            // backend so the capture path can reach the registry.
            let _migrate_peer_task = self.cfg.migrate_peer_listen_addr.map(|addr| {
                let server = migrate_peer::PeerServer::new(addr.port());
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
            let stages_images = stages_images_gate(self.fill.is_some(), self.chunk_cache.is_some());
            let (enabled_images_tx, image_disks_tx) = match (
                self.fill.clone(),
                self.chunk_cache.clone(),
            ) {
                (Some(fill), Some(chunk_cache)) => {
                    // ADR 0022 Option A / ADR 0092: a fresh create always
                    // restores on the File backend, so the supervisor
                    // materializes each enabled image's contiguous
                    // per-template base memfile at residency — at the
                    // SAME path a base session.create restore reads
                    // (`pooled.snapshot_path_for(base_snapshot_id)`), so
                    // they agree by construction. Resumes take the
                    // substrate (UFFD + the lazily-populated base shm).
                    let base_memfile_dir: Option<image_prefetch::SnapshotDirResolver> = {
                        let p = pooled.clone();
                        let resolver: image_prefetch::SnapshotDirResolver =
                            std::sync::Arc::new(move |id| p.snapshot_path_for(id));
                        Some(resolver)
                    };
                    let (tx, _handle) = image_prefetch::spawn_supervisor(
                        fill.clone(),
                        chunk_cache,
                        readiness.clone(),
                        base_memfile_dir,
                        ram_ledger.clone(),
                        self.base_shm_protected.clone(),
                    );
                    let disks_tx = session_volumes.as_ref().map(|volumes| {
                        let (disks_tx, disks_rx) = tokio::sync::watch::channel(Vec::new());
                        image_volumes::spawn(volumes.images().clone(), disks_rx);
                        disks_tx
                    });
                    (Some(tx), disks_tx)
                }
                _ => {
                    tracing::warn!(
                        "image_prefetch supervisor disabled: chunk_store / chunk_cache \
                         missing — host will never report ready_images and coord will 503 every session",
                    );
                    (None, None)
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
            let live_bundles_tx = self.chunk_store.as_ref().map(|cs| {
                bundles::spawn_supervisor(
                    bundles::BundleStore::new(
                        cs.blob_storage().clone(),
                        bundle_dir.clone(),
                        bundle_ext,
                    ),
                    current_bundles.clone(),
                )
            });
            // ADR 0035 amendment D1: durable-at-birth — publish the stamp's
            // generations to BlobStorage now, not at first snapshot
            // reference. Closes the create→first-checkpoint window in
            // which a stamp rotation + sweep destroys the only copy of
            // a generation a running VM has attached (the 2026-08-10
            // `chain_poisoned` firing). Fire-and-forget: must never
            // gate readiness or heartbeating.
            if let Some(cs) = self.chunk_store.as_ref() {
                bundles::spawn_startup_publish(
                    bundles::BundleStore::new(
                        cs.blob_storage().clone(),
                        bundle_dir.clone(),
                        bundle_ext,
                    ),
                    current_bundles.clone(),
                );
            }

            // ADR 0068: every heartbeat tick's `capabilities::probe_all`
            // re-runs the TCP self-connect as the `grpc_self_connect`
            // capability, and placement (`host_meets_capabilities`)
            // requires it `Ok`, so the coordinator places nothing on this
            // host before its listener answers.

            // ADR 0013: HTTP heartbeat loop. Posts
            // {capacity, local_snapshots, running_sandboxes,
            // draining, ready_images} every `heartbeat_interval`, and at
            // once after a checkpoint record lands, to any coord pod via
            // the L4-LB. Coord pod that receives it updates
            // host_registry, runs ADR 0009 reconcile, and persists to PG.
            // Idempotent across pods. The ack carries `enabled_images`,
            // which the supervisor diffs against the local ready set.
            let heartbeat_interval = self.cfg.heartbeat_interval;
            let coord_for_heartbeat = coord_client.clone();
            let harness_hub_for_heartbeat = harness_hub.clone();
            let pooled_for_heartbeat = pooled.clone();
            let checkpoint_recorded = pooled.checkpoint_recorded();
            let capture_jobs_for_heartbeat = capture_jobs.clone();
            let host_addr_for_heartbeat = self.cfg.grpc_advertise_addr.clone();
            let readiness_for_heartbeat = readiness.clone();
            let session_volumes_for_heartbeat = session_volumes.clone();
            let enabled_for_heartbeat = enabled_images_tx.as_ref().map(|tx| tx.subscribe());
            let util_work_dir = self.cfg.work_dir.clone();
            let probe_inputs_for_heartbeat = probe_inputs.clone();
            let mut last_pool_state = probe_inputs.session_disk.clone();
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
            let managed_list_host = local_host.clone();
            // The enabled-images channel starts empty, which is not an ack.
            // The first ack always notifies, even when the set is empty, so
            // the supervisors can tell "no images" from "not told yet".
            let mut enabled_acked = false;
            // The last capturing set a coordinator sent. An ack without the
            // field (a failed read, or an older coordinator) keeps it.
            let mut capturing_disks = Vec::new();
            let heartbeat_task = tokio::spawn(async move {
                let mut tick = engram_core::periodic(heartbeat_interval);
                // Observed disk/mem/cpu for the fleet view. Holds the
                // prior `/proc/stat` sample so CPU is a true interval
                // delta; the first tick reports cpu_pct=0.
                let mut util_probe = crate::util::UtilizationProbe::new();
                loop {
                    crate::heartbeat::next_beat(&mut tick, &checkpoint_recorded).await;
                    // Issue #215: a `list()` error is "no information",
                    // not "no sandboxes running". Reporting empty would
                    // make the coord's ADR 0009 reconcile strike every
                    // active session on this host that tick. Carry an
                    // explicit `running_sandboxes_known = false` so the
                    // coord skips reconcile for this heartbeat instead of
                    // mistaking the empty set for a real running set.
                    // Residents include running and tearing-down sandboxes.
                    let (running_sandboxes, running_sandboxes_known) = match managed_list_host
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
                    // ADR 0035 amendment D2: per-sandbox aux attachments, so the
                    // coord's `bundle_pin_set` covers running-but-
                    // unsnapshotted sandboxes (their generations must
                    // survive the host sweep + bundle GC until the
                    // first snapshot pins them durably).
                    let sandbox_bundles = pooled_for_heartbeat.aux_bundles_all().await;
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
                        Some(dir) => {
                            checkpoint::CheckpointRecord::load_all(&engram_host_core::TokioFs, &dir)
                                .await
                        }
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
                            swap_manifest: r.swap_manifest,
                            disk_manifest: r.disk_manifest,
                            memory_manifest: r.memory_manifest,
                            aux_bundles: r.aux_bundles.clone(),
                            paused_at: r.paused_at,
                            captured_at: r.captured_at,
                            kind: r.kind,
                        })
                        .collect();
                    let mut utilization = util_probe.sample(&util_work_dir, &ram_snapshot);
                    let mut probe_inputs = probe_inputs_for_heartbeat.clone();
                    if let Some(volumes) = session_volumes_for_heartbeat.as_deref() {
                        reap_kept_checkpoints(volumes).await;
                    }
                    let (pool_state, pool) =
                        probe_pool(session_volumes_for_heartbeat.as_deref().map(|v| v.disk()))
                            .await;
                    if let Some(cap) = pool {
                        const MIB: u64 = 1024 * 1024;
                        utilization.pool_data_free_mib = cap.data_free_bytes / MIB;
                        utilization.pool_data_total_mib = cap.data_total_bytes / MIB;
                        utilization.pool_metadata_free_mib = cap.metadata_free_bytes / MIB;
                        utilization.pool_metadata_total_mib = cap.metadata_total_bytes / MIB;
                    }
                    if pool_state != last_pool_state {
                        match &pool_state {
                            capabilities::SessionDiskState::Faulted(e) => {
                                tracing::warn!(error = %e, "session-disk pool probe failed")
                            }
                            _ => tracing::info!(?pool_state, "session-disk pool state changed"),
                        }
                        last_pool_state = pool_state.clone();
                    }
                    probe_inputs.session_disk = pool_state;
                    // Every probe re-runs each tick, so a withdrawn
                    // capability returns by itself.
                    let capabilities = capabilities::probe_all(&probe_inputs).await;
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
                        ready_images: match (&session_volumes_for_heartbeat, &enabled_for_heartbeat)
                        {
                            (Some(volumes), Some(enabled)) => {
                                let enabled = enabled.borrow().clone();
                                image_volumes::ready_on_disk(
                                    volumes.disk(),
                                    &enabled,
                                    readiness_for_heartbeat.snapshot(),
                                )
                                .await
                            }
                            _ => readiness_for_heartbeat.snapshot(),
                        },
                        current_bundles: current_bundles.clone(),
                        sandbox_bundles,
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
                                let first = !std::mem::replace(&mut enabled_acked, true);
                                if let Some(capturing) = resp.capturing_disk_manifests {
                                    capturing_disks = capturing;
                                }
                                if let Some(disks_tx) = image_disks_tx.as_ref() {
                                    let disks =
                                        image_volumes::desired_disks(&union, &capturing_disks);
                                    disks_tx.send_if_modified(|cur| {
                                        let changed = first || *cur != disks;
                                        *cur = disks;
                                        changed
                                    });
                                }
                                tx.send_if_modified(|cur| {
                                    let changed = first || *cur != union;
                                    *cur = union;
                                    changed
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
                                        &engram_host_core::TokioFs,
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
                            // ADR 0116 A-D5: destroy tombstoned sandboxes
                            // — the coordinator's durable "this VM is
                            // disowned; its host must destroy it" fact
                            // (bindings cleared without host-affirmed
                            // absence). Each destroy is its own task so a
                            // slow teardown never delays this tick; the
                            // coordinator deletes the row once the sandbox
                            // leaves our reported running set
                            // (ack-by-absence), so failures simply retry
                            // on the next advertise. Placed BEFORE the
                            // capture-assignment arm: its `None` case
                            // `continue`s the loop.
                            for &sandbox_id in &resp.tombstoned_sandboxes {
                                let host = tombstone_host.clone();
                                tokio::spawn(async move {
                                    match host.destroy_tombstoned(sandbox_id).await {
                                        Ok(()) => tracing::info!(
                                            %sandbox_id,
                                            "destroyed tombstoned sandbox (coordinator disowned it)",
                                        ),
                                        // Already gone: the next heartbeat's
                                        // running set acks the row.
                                        Err(engram_core::SandboxError::NotFound) => {}
                                        Err(e) => tracing::warn!(
                                            %sandbox_id,
                                            error = %e,
                                            "tombstoned sandbox destroy failed; retrying on next advertise",
                                        ),
                                    }
                                });
                            }
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
            // Quiesce captures first. From here no capture may START —
            // one that did would consume the KVM dirty bitmap and then die
            // in the runtime teardown, poisoning its checkpoint chain (the
            // surviving VM pays a FULL re-chunk under the successor). The
            // periodic driver task is deliberately NOT aborted: an abort
            // cancels an in-flight capture at an await point, which is the
            // exact post-bitmap failure this quiesce exists to prevent.
            // In-flight captures are drained (bounded) below instead.
            pooled.quiesce_captures_for_shutdown();
            if let Some(boot) = &managed_boot_host {
                boot.shutdown().await;
            }
            // ADR 0116 A-D2: declare the handoff BEFORE the heartbeat
            // task dies — the coordinator must learn "this exit is
            // planned; hold the lease" while our channel to it is still
            // warm. Two writers, cheapest-first: (1) the durable marker
            // in the bindings dir (the successor adopts + reports it),
            // (2) a best-effort POST bounded to 2 s + one retry — never
            // blocks the ladder (the operator's authoritative gRPC
            // declaration covers operator-driven rolls; this belt covers
            // kubelet-initiated restarts the operator never sees).
            let handoff_ttl = engram_host_core::shutdown::shutdown_handoff_ttl_secs(
                std::env::var("ENGRAM_SHUTDOWN_HANDOFF_TTL_SECS")
                    .ok()
                    .as_deref(),
            );
            let resident = pooled.list().await.map(|v| v.len() as u32).unwrap_or(0);
            declare_shutdown_handoff(
                &self.cfg.work_dir,
                &coord_client,
                host_id,
                handoff_ttl,
                resident,
            )
            .await;
            heartbeat_task.abort();
            if let Some(t) = grpc_task {
                t.abort();
            }
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
            // A capture already past FC's snapshot create has consumed the
            // KVM dirty bitmap: its post-processing (sparse re-chunk, upload,
            // chain-head persist) must finish before this process exits or
            // the chain poisons. Bounded by
            // `ENGRAM_SHUTDOWN_CAPTURE_DRAIN_BUDGET_SECS`.
            let deadline = engram_host_core::capture_drain_budget(
                std::env::var("ENGRAM_SHUTDOWN_CAPTURE_DRAIN_BUDGET_SECS")
                    .ok()
                    .and_then(|v| v.parse::<f64>().ok()),
            );
            let stragglers = pooled.drain_captures_for_shutdown(deadline).await;
            if stragglers > 0 {
                tracing::warn!(
                    stragglers,
                    "SIGTERM: captures still in flight at the capture-drain \
                     deadline; their chains will poison at process exit",
                );
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

/// Await either SIGINT (ctrl-c) or SIGTERM (Kubernetes shutdown).
/// On non-unix platforms, falls back to ctrl-c only.
/// ADR 0116 A-D2: the SIGTERM ladder's handoff declaration. Writes the
/// durable marker first (atomic tmp+rename+fsync in the bindings dir —
/// the successor adopts and reports it), then makes ONE bounded (2 s)
/// belt POST with one retry. Never fails the ladder: the marker is
/// best-effort durability, the POST is best-effort reach, and the
/// operator's authoritative gRPC declaration covers operator-driven
/// rolls regardless.
async fn declare_shutdown_handoff(
    work_dir: &std::path::Path,
    coord_client: &coord_client::HttpCoordClient,
    host_id: engram_core::HostId,
    ttl_secs: u64,
    resident_sandboxes: u32,
) {
    use engram_core::traits::Clock;
    // ADR 0098 D1: the injected production clock, same construction as
    // the event sink's — a marker timestamp, never a decision input.
    let now = engram_core::traits::SystemClock::new().now_utc();
    let marker = engram_host_core::shutdown::HandoffMarker {
        declared_at_unix_ms: now.timestamp_millis(),
        ttl_secs,
        resident_sandboxes,
    };
    let dir = work_dir.join("bindings");
    let final_path = dir.join(engram_host_core::shutdown::HANDOFF_MARKER_FILE);
    let tmp = dir.join(".handoff.json.tmp");
    let write = || -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(&marker)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, bytes)?;
        std::fs::File::open(&tmp)?.sync_all()?;
        std::fs::rename(&tmp, &final_path)?;
        std::fs::File::open(&dir)?.sync_all()?;
        Ok(())
    };
    match write() {
        Ok(()) => tracing::info!(%host_id, ttl_secs, resident_sandboxes,
            "shutdown handoff: durable marker written for the successor"),
        Err(e) => tracing::warn!(%host_id, error = %e,
            "shutdown handoff: marker write failed; successor registers as fresh"),
    }
    for attempt in 0..2u8 {
        match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            coord_client.handoff(host_id, ttl_secs),
        )
        .await
        {
            Ok(Ok(accepted)) => {
                tracing::info!(%host_id, ttl_secs, accepted, "shutdown handoff: belt POST landed");
                return;
            }
            Ok(Err(e)) => {
                tracing::warn!(%host_id, attempt, error = %e, "shutdown handoff: belt POST failed");
            }
            Err(_) => {
                tracing::warn!(%host_id, attempt, "shutdown handoff: belt POST timed out (2s)");
            }
        }
    }
}

/// ADR 0116 A-D2: successor-side adoption of the predecessor's handoff
/// marker — read + DELETE (a marker must never be reported twice; a
/// crash between read and delete re-reports once, which is harmless
/// observability). `None` = fresh start or unreadable marker.
fn adopt_handoff_marker(
    work_dir: &std::path::Path,
) -> Option<engram_host_core::shutdown::HandoffMarker> {
    let path = work_dir
        .join("bindings")
        .join(engram_host_core::shutdown::HANDOFF_MARKER_FILE);
    let bytes = std::fs::read(&path).ok()?;
    let marker = match serde_json::from_slice(&bytes) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "handoff marker unreadable; removing + registering fresh");
            let _ = std::fs::remove_file(&path);
            return None;
        }
    };
    let _ = std::fs::remove_file(&path);
    Some(marker)
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
            std::sync::Arc::new(|_, _, _, _| Box::pin(async { Ok(()) })),
            bindings::BindingStore::open(dir).expect("open binding store"),
        )
    }

    /// ADR 0073 replacement for the retired `rebind_survivor_sessions`
    /// coverage (#447): a host-agent restart must
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

    /// ADR 0116 A-D2: the ladder writes a durable marker; the successor
    /// adopts it exactly once (read + delete). The belt POST is aimed at
    /// an unroutable loopback port so it fails fast without a live coord.
    #[tokio::test]
    async fn shutdown_handoff_marker_round_trips_and_is_adopted_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("bindings")).expect("bindings dir");
        let cc = coord_client::HttpCoordClient::new("http://127.0.0.1:1".to_string(), None);
        let host_id = engram_core::HostId::new();
        declare_shutdown_handoff(dir.path(), &cc, host_id, 600, 3).await;

        let adopted = adopt_handoff_marker(dir.path()).expect("marker adopted");
        assert_eq!(adopted.ttl_secs, 600);
        assert_eq!(adopted.resident_sandboxes, 3);
        assert!(adopted.declared_at_unix_ms > 0);
        // Adoption is consume-once.
        assert!(adopt_handoff_marker(dir.path()).is_none());
    }
}

pub mod managed_policy;

pub mod managed_tls;

mod managed_assets;
pub mod managed_boot;

#[cfg(test)]
mod managed_startup_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct StartupBoundary(AtomicBool);
    #[async_trait::async_trait]
    impl CloudBackend for StartupBoundary {
        async fn host_metadata(
            &self,
        ) -> Result<engram_core::types::host::HostMetadata, engram_core::BackendError> {
            self.0.store(true, Ordering::SeqCst);
            std::future::pending().await
        }
        async fn provision_host(
            &self,
            _: engram_core::types::host::HostSpec,
        ) -> Result<engram_core::HostId, engram_core::BackendError> {
            unreachable!()
        }
        async fn deprovision_host(
            &self,
            _: engram_core::HostId,
        ) -> Result<(), engram_core::BackendError> {
            unreachable!()
        }
    }

    async fn rejects_before_startup(malformed: bool) {
        let temp = tempfile::tempdir().unwrap();
        let ca = engram_egress_proxy::Ca::load_or_generate(&temp.path().join("ca")).unwrap();
        let certificate = temp.path().join("host.pem");
        let key = temp.path().join("host.key");
        std::fs::write(
            &certificate,
            if malformed {
                "not a certificate"
            } else {
                &ca.cert_pem
            },
        )
        .unwrap();
        std::fs::write(&key, ca.key_pair.serialize_pem()).unwrap();
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = occupied.local_addr().unwrap();
        // Malformed TLS must fail even with an available listener.
        let occupied = if malformed {
            drop(occupied);
            None
        } else {
            Some(occupied)
        };
        let cloud = Arc::new(StartupBoundary(AtomicBool::new(false)));
        let config = HostAgentConfig {
            managed_tls: Some(managed_tls::ManagedHostTls {
                certificate: certificate.clone(),
                key,
                client_ca: certificate,
                coordinator_pins: std::collections::BTreeSet::from(["a".repeat(64)]),
            }),
            work_dir: temp.path().join("host"),
            grpc_listen_addr: Some(addr),
            grpc_advertise_addr: Some(format!("https://{addr}")),
            coordinator_endpoint: Some("http://127.0.0.1:1".into()),
            ..HostAgentConfig::default()
        };
        let agent = HostAgent::new(
            config,
            Arc::new(engram_sandbox_process::ProcessBackend::new(
                temp.path().join("ordinary"),
            )),
            cloud.clone(),
        )
        .with_host_id("00000000-0000-0000-0000-000000000001".parse().unwrap())
        .with_egress(Arc::new(egress::HostEgress::new(
            Arc::new(proxyd_client::ProxydHandle::new(
                temp.path().join("absent.sock"),
            )),
            ca.cert_pem,
            None,
        )));
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), agent.run()).await;
        drop(occupied);
        assert!(
            outcome.is_ok_and(|result| result.is_err()),
            "invalid managed listener must fail startup"
        );
        assert!(
            !cloud.0.load(Ordering::SeqCst),
            "failure must precede startup work and coordinator registration"
        );
    }
    #[tokio::test]
    async fn malformed_managed_tls_cannot_reach_registration() {
        rejects_before_startup(true).await;
    }
    #[tokio::test]
    async fn occupied_managed_listener_cannot_reach_registration() {
        rejects_before_startup(false).await;
    }
}
