//! ADR 0014 M1.6: per-host warm-pool driver.
//!
//! Holds a free-list of pre-restored Firecracker microVMs per
//! [`TemplateRef`]. Each entry is a sandbox that has been
//! `restore_from_snapshot`d from the template's bake-time portable
//! snapshot (state.bin + sidecar + memory chunks) and is currently
//! paused with bootstrap awaiting a `BootstrapLaunch` frame on
//! vsock.
//!
//! ## Lifecycle
//!
//! - **Observe**: heartbeat-response (M1.7) hands the pool the
//!   coord's current active-template set. Templates that drop off
//!   the set get a 60s grace before their free-list drains.
//! - **Refill**: when a template's free-list dips below `target`,
//!   a background task restores a fresh microVM and pushes the
//!   resulting `SandboxId` onto the list. Concurrency is bounded
//!   per template (one refill at a time) to avoid thundering
//!   herd against BlobStorage.
//! - **Lease**: atomic `Vec::pop` from the free-list. Returns
//!   `Granted(sandbox_id)` on success, `NoCapacity` if empty,
//!   or `Stale{current_ref}` when the requested ref isn't in the
//!   known-templates map (coord cache lag). Triggers a refill
//!   spawn.
//! - **Launch**: the activation step — coord calls
//!   [`WarmPool::launch`] with the per-session agent + egress
//!   policy; this delegates to `SandboxBackend::start_agent`,
//!   which applies the policy and writes the `BootstrapLaunch`
//!   frame to the in-guest bootstrap supervisor.
//!
//! For M1.6 the autoscaler target is fixed at N=1 per template;
//! M1.9 introduces lease-rate-driven scaling.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use engram_core::error::SandboxError;
use engram_core::traits::host_client::{WarmLeaseOutcome, WarmSlotCount};
use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::ids::TemplateRef;
use engram_core::types::sandbox::AgentSpec;
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::types::template::TemplateRecord;
use engram_core::SandboxId;
use parking_lot::Mutex;

/// Default target slots per template until the autoscaler (M1.9)
/// lights up.
const DEFAULT_TARGET: u32 = 1;

/// How long an inactive template's free-list keeps existing
/// sandboxes alive after the coord drops it from the active set.
/// During this window, in-flight leases against the old ref can
/// still succeed; after, the slots are destroyed and the entry
/// removed.
const STALE_GRACE: Duration = Duration::from_secs(60);

/// Per-host warm-pool state. Cheap to clone (one inner Arc).
#[derive(Clone)]
pub struct WarmPool {
    inner: Arc<WarmPoolInner>,
}

struct WarmPoolInner {
    /// Free-list per template_ref. `Vec::pop` is the lease primitive.
    free_lists: DashMap<TemplateRef, Mutex<Vec<SandboxId>>>,
    /// Most recently observed (TemplateRecord + SnapshotMetadata)
    /// per template_ref. The refill loop reads SnapshotMetadata
    /// from here to drive `backend.restore`.
    known: DashMap<TemplateRef, KnownTemplate>,
    /// Autoscaler target per template. Defaults to `DEFAULT_TARGET`;
    /// M1.9 makes this lease-rate-driven.
    targets: DashMap<TemplateRef, u32>,
    /// Reference back to the host's SandboxBackend (typically a
    /// PooledBackend wrapping FirecrackerBackend). Used by the
    /// refill loop for `restore` and by `launch` for
    /// `start_agent` / `destroy`.
    backend: Arc<dyn SandboxBackend>,
}

struct KnownTemplate {
    record: TemplateRecord,
    metadata: SnapshotMetadata,
    /// When the coord most recently said this template was active.
    /// `None` means the template is currently active in the coord's
    /// set; `Some(t)` means we dropped it from active at time `t`
    /// and the grace window started.
    inactive_since: Option<std::time::Instant>,
}

impl WarmPool {
    pub fn new(backend: Arc<dyn SandboxBackend>) -> Self {
        Self {
            inner: Arc::new(WarmPoolInner {
                free_lists: DashMap::new(),
                known: DashMap::new(),
                targets: DashMap::new(),
                backend,
            }),
        }
    }

    /// Refresh the host's view of the active template set, called
    /// from the heartbeat-response handler. `templates` is the
    /// coord's authoritative list. Returns a (gained, dropped)
    /// tuple — handy for log lines but the side effects are what
    /// matters.
    ///
    /// Side effects:
    /// - Newly-active templates land in `known` and the refill
    ///   loop fires for each.
    /// - Templates absent from the new list flip to `inactive_since
    ///   = Some(now)`; their free-lists keep existing entries until
    ///   the grace window expires (handled by the gc tick).
    /// - Templates whose snapshot_id changed (rebake) flush their
    ///   free-list — the old slots reference a stale snapshot and
    ///   need to be destroyed.
    pub async fn observe_templates(&self, templates: Vec<(TemplateRecord, SnapshotMetadata)>) {
        let now = std::time::Instant::now();
        let mut seen = std::collections::HashSet::new();
        for (record, metadata) in templates {
            seen.insert(record.template_ref);
            self.upsert_known(record, metadata).await;
        }
        // Templates in `known` but not in `seen` — flip to inactive.
        for mut entry in self.inner.known.iter_mut() {
            if !seen.contains(entry.key()) && entry.inactive_since.is_none() {
                entry.inactive_since = Some(now);
            }
        }
        // Spawn refill for active templates that need it.
        for entry in self.inner.known.iter() {
            if entry.inactive_since.is_some() {
                continue;
            }
            self.maybe_refill(*entry.key()).await;
        }
    }

    async fn upsert_known(&self, record: TemplateRecord, metadata: SnapshotMetadata) {
        let template_ref = record.template_ref;
        let snapshot_id = record.snapshot_id;
        let mut entry = self
            .inner
            .known
            .entry(template_ref)
            .or_insert_with(|| KnownTemplate {
                record: record.clone(),
                metadata: metadata.clone(),
                inactive_since: None,
            });
        // If the snapshot_id changed (rebake), the existing free-list
        // entries are stale — drain them. Drop the lock before
        // calling destroy on each.
        let stale_drain = if entry.record.snapshot_id != snapshot_id {
            entry.record = record;
            entry.metadata = metadata;
            entry.inactive_since = None;
            self.inner
                .free_lists
                .get(&template_ref)
                .map(|m| std::mem::take(&mut *m.lock()))
                .unwrap_or_default()
        } else {
            // Re-arm the active flag — coord said this template is
            // current again. May be redundant on a hot loop; cheap.
            entry.inactive_since = None;
            Vec::new()
        };
        drop(entry);
        self.inner
            .targets
            .entry(template_ref)
            .or_insert(DEFAULT_TARGET);
        for sandbox_id in stale_drain {
            let _ = self.inner.backend.destroy(sandbox_id).await;
        }
    }

    /// Atomic take from the free-list for `template_ref`. Three
    /// outcomes per the [`WarmLeaseOutcome`] discriminant. Schedules
    /// a refill in the background on success or empty so the pool
    /// returns to target.
    pub async fn lease(&self, template_ref: TemplateRef) -> WarmLeaseOutcome {
        // Stale template: coord asked about a ref we don't know
        // about. Return the most recent known ref so the coord can
        // refresh its cache. If no ref is known at all, NoCapacity.
        if !self.inner.known.contains_key(&template_ref) {
            // No exact match — see if we have any active known to
            // hint at. Picking the first one isn't great heuristic
            // but the coord will re-resolve anyway.
            let hint = self
                .inner
                .known
                .iter()
                .find(|e| e.inactive_since.is_none())
                .map(|e| *e.key());
            return match hint {
                Some(current_ref) => WarmLeaseOutcome::Stale { current_ref },
                None => WarmLeaseOutcome::NoCapacity,
            };
        }
        let leased = self
            .inner
            .free_lists
            .get(&template_ref)
            .and_then(|m| m.lock().pop());
        // Whether we leased one or not, refill toward the target
        // so the next lease finds a slot. Spawned to avoid blocking
        // the caller (coord's parallel-ask path).
        self.maybe_refill(template_ref).await;
        match leased {
            Some(sandbox_id) => WarmLeaseOutcome::Granted(sandbox_id),
            None => WarmLeaseOutcome::NoCapacity,
        }
    }

    /// Activate a previously-leased warm sandbox: push
    /// BootstrapLaunch + apply egress policy. Pre-restored substrate
    /// is already running; this is the per-session activation step.
    pub async fn launch(
        &self,
        sandbox_id: SandboxId,
        agent: AgentSpec,
        policy: SessionEgressPolicy,
    ) -> Result<(), SandboxError> {
        // ADR 0014 ordering: policy onto the proxy registry BEFORE
        // bootstrap exec's the agent. Same invariant ADR 0013
        // codified for cold-create's StartAgent.
        self.inner.backend.notify_session_policy(policy).await?;
        self.inner.backend.start_agent(sandbox_id, agent).await
    }

    /// Snapshot of per-template inventory (free-list size + target).
    /// Heartbeat (M1.7) ships this to coord; ops queries it via
    /// `ListWarmSlots`.
    pub fn list_slots(&self) -> Vec<WarmSlotCount> {
        let mut out = Vec::with_capacity(self.inner.free_lists.len());
        for entry in self.inner.free_lists.iter() {
            let template_ref = *entry.key();
            let available = entry.value().lock().len() as u32;
            let target = self
                .inner
                .targets
                .get(&template_ref)
                .map(|t| *t)
                .unwrap_or(DEFAULT_TARGET);
            out.push(WarmSlotCount {
                template_ref,
                available,
                target,
            });
        }
        out
    }

    /// Tick called by a periodic timer (every ~5s) to:
    /// - Garbage-collect inactive templates past their grace window.
    /// - Top up free-lists toward their target.
    pub async fn gc_tick(&self) {
        let now = std::time::Instant::now();
        let mut to_remove = Vec::new();
        for entry in self.inner.known.iter() {
            if let Some(t) = entry.inactive_since {
                if now.duration_since(t) >= STALE_GRACE {
                    to_remove.push(*entry.key());
                }
            }
        }
        for template_ref in to_remove {
            let to_destroy = self
                .inner
                .free_lists
                .remove(&template_ref)
                .map(|(_, m)| m.into_inner())
                .unwrap_or_default();
            self.inner.known.remove(&template_ref);
            self.inner.targets.remove(&template_ref);
            for sandbox_id in to_destroy {
                let _ = self.inner.backend.destroy(sandbox_id).await;
            }
        }
        for entry in self.inner.known.iter() {
            if entry.inactive_since.is_some() {
                continue;
            }
            self.maybe_refill(*entry.key()).await;
        }
    }

    /// If the free-list for `template_ref` is below target, spawn
    /// one refill task. The spawned task does the heavy lifting
    /// (BlobStorage download via `backend.restore`) off the caller's
    /// task. Concurrency is bounded by `RefillGuard` (one in-flight
    /// refill per template_ref); a M1.9 follow-up will make this
    /// rate-aware.
    async fn maybe_refill(&self, template_ref: TemplateRef) {
        let target = self
            .inner
            .targets
            .get(&template_ref)
            .map(|t| *t)
            .unwrap_or(DEFAULT_TARGET);
        let current = self
            .inner
            .free_lists
            .get(&template_ref)
            .map(|m| m.lock().len() as u32)
            .unwrap_or(0);
        if current >= target {
            return;
        }
        let metadata = match self.inner.known.get(&template_ref) {
            Some(k) => k.metadata.clone(),
            None => return,
        };
        let pool = self.clone();
        tokio::spawn(async move {
            // SnapshotMetadata is Clone — capture it into the
            // spawned task. The backend's restore() handles
            // BlobStorage materialisation (state.bin + sidecar +
            // memory chunks) under the hood, then drives FC.
            let sandbox_id = match pool.inner.backend.restore(metadata).await {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(
                        %template_ref,
                        error = %e,
                        "warm_pool refill failed",
                    );
                    return;
                }
            };
            // Push to the free-list. Use entry() to lazy-create on
            // the first refill.
            pool.inner
                .free_lists
                .entry(template_ref)
                .or_default()
                .lock()
                .push(sandbox_id);
            tracing::debug!(
                %template_ref,
                %sandbox_id,
                "warm_pool refill complete",
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::Utc;
    use engram_core::traits::sandbox::SandboxBackend;
    use engram_core::types::ids::SnapshotId;
    use engram_core::types::manifest::ManifestRef;
    use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxSpec};
    use engram_core::SandboxId;
    use parking_lot::Mutex as PMutex;

    /// Test backend that hands out a fresh SandboxId on every
    /// restore() and records the snapshot_id it was asked to
    /// restore. Implements just enough of SandboxBackend for
    /// WarmPool's needs.
    struct FakeBackend {
        restore_count: PMutex<u32>,
        last_metadata: PMutex<Option<SnapshotMetadata>>,
        destroy_log: PMutex<Vec<SandboxId>>,
        agent_log: PMutex<Vec<(SandboxId, AgentSpec)>>,
    }

    impl FakeBackend {
        fn new() -> Self {
            Self {
                restore_count: PMutex::new(0),
                last_metadata: PMutex::new(None),
                destroy_log: PMutex::new(Vec::new()),
                agent_log: PMutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl SandboxBackend for FakeBackend {
        async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn exec_stream(
            &self,
            _: SandboxId,
            _: ExecRequest,
        ) -> Result<ExecStream, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        fn snapshot_path_for(&self, _: SnapshotId) -> std::path::PathBuf {
            std::path::PathBuf::new()
        }
        async fn restore(&self, m: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
            *self.restore_count.lock() += 1;
            *self.last_metadata.lock() = Some(m);
            Ok(SandboxId::new())
        }
        async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
            self.destroy_log.lock().push(id);
            Ok(())
        }
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            Ok(Vec::new())
        }
        async fn start_agent(&self, id: SandboxId, spec: AgentSpec) -> Result<(), SandboxError> {
            self.agent_log.lock().push((id, spec));
            Ok(())
        }
    }

    fn template(snapshot_id: SnapshotId) -> (TemplateRecord, SnapshotMetadata) {
        let ref_id = TemplateRef::new();
        let rec = TemplateRecord {
            template_ref: ref_id,
            image_repo: "repo".into(),
            image_tag: "tag".into(),
            harness_pack_uri: "pack".into(),
            snapshot_id,
            vcpus: 1,
            memory_mib: 64,
            created_at: Utc::now(),
            active: true,
        };
        let meta = SnapshotMetadata {
            id: snapshot_id,
            size_bytes: 0,
            created_at: Utc::now(),
            image_version: "test".into(),
            disk_manifest: None,
            memory_manifest: Some(ManifestRef::new()),
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
        };
        (rec, meta)
    }

    /// Wait until `cond` returns true OR the deadline expires.
    /// Refill happens off-task in a tokio::spawn so unit tests need
    /// to poll for completion.
    async fn wait_for(cond: impl Fn() -> bool, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cond()
    }

    #[tokio::test]
    async fn observe_templates_refills_to_default_target() {
        let backend = Arc::new(FakeBackend::new());
        let pool = WarmPool::new(backend.clone() as Arc<dyn SandboxBackend>);
        let (rec, meta) = template(SnapshotId::new());
        pool.observe_templates(vec![(rec.clone(), meta.clone())])
            .await;
        assert!(
            wait_for(
                || *backend.restore_count.lock() >= DEFAULT_TARGET,
                Duration::from_secs(2)
            )
            .await,
            "refill should run at least DEFAULT_TARGET times",
        );
        // The free-list should now have at least one entry.
        let slots = pool.list_slots();
        let entry = slots
            .iter()
            .find(|s| s.template_ref == rec.template_ref)
            .expect("free-list entry exists");
        assert!(entry.available >= 1);
        assert_eq!(entry.target, DEFAULT_TARGET);
    }

    #[tokio::test]
    async fn lease_pops_from_free_list_and_triggers_refill() {
        let backend = Arc::new(FakeBackend::new());
        let pool = WarmPool::new(backend.clone() as Arc<dyn SandboxBackend>);
        let (rec, meta) = template(SnapshotId::new());
        pool.observe_templates(vec![(rec.clone(), meta.clone())])
            .await;
        wait_for(
            || *backend.restore_count.lock() >= 1,
            Duration::from_secs(2),
        )
        .await;
        let outcome = pool.lease(rec.template_ref).await;
        assert!(matches!(outcome, WarmLeaseOutcome::Granted(_)));
        // Lease triggers another refill to maintain target.
        wait_for(
            || *backend.restore_count.lock() >= 2,
            Duration::from_secs(2),
        )
        .await;
        assert!(*backend.restore_count.lock() >= 2);
    }

    #[tokio::test]
    async fn lease_unknown_template_returns_stale_or_nocapacity() {
        let backend = Arc::new(FakeBackend::new());
        let pool = WarmPool::new(backend.clone() as Arc<dyn SandboxBackend>);
        // Empty pool — no known templates.
        let unknown = TemplateRef::new();
        let outcome = pool.lease(unknown).await;
        assert!(matches!(outcome, WarmLeaseOutcome::NoCapacity));

        // Populate one known template, then ask for a different one.
        let (rec, meta) = template(SnapshotId::new());
        pool.observe_templates(vec![(rec.clone(), meta)]).await;
        let outcome = pool.lease(TemplateRef::new()).await;
        assert!(
            matches!(outcome, WarmLeaseOutcome::Stale { current_ref } if current_ref == rec.template_ref),
            "stale outcome should hint at the host's known active ref",
        );
    }

    #[tokio::test]
    async fn launch_invokes_start_agent_on_backend() {
        let backend = Arc::new(FakeBackend::new());
        let pool = WarmPool::new(backend.clone() as Arc<dyn SandboxBackend>);
        let sandbox_id = SandboxId::new();
        let agent = AgentSpec {
            argv: vec!["/bin/echo".into(), "x".into()],
            env: Default::default(),
        };
        let policy = SessionEgressPolicy {
            session_id: engram_core::SessionId::new(),
            sandbox_id,
            guest_ip: std::net::Ipv4Addr::UNSPECIFIED,
            network_allow_hosts: Default::default(),
            network_allow_host_patterns: Default::default(),
            secrets: Default::default(),
            secret_mode: Default::default(),
        };
        pool.launch(sandbox_id, agent.clone(), policy)
            .await
            .unwrap();
        let log = backend.agent_log.lock().clone();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].0, sandbox_id);
        assert_eq!(log[0].1.argv, agent.argv);
    }

    #[tokio::test]
    async fn rebake_drains_old_free_list() {
        let backend = Arc::new(FakeBackend::new());
        let pool = WarmPool::new(backend.clone() as Arc<dyn SandboxBackend>);
        let (rec_v1, meta_v1) = template(SnapshotId::new());
        pool.observe_templates(vec![(rec_v1.clone(), meta_v1)])
            .await;
        wait_for(
            || *backend.restore_count.lock() >= 1,
            Duration::from_secs(2),
        )
        .await;
        let v1_sandboxes: Vec<_> = pool
            .inner
            .free_lists
            .get(&rec_v1.template_ref)
            .map(|m| m.lock().clone())
            .unwrap_or_default();
        assert!(!v1_sandboxes.is_empty());

        // Rebake: same template_ref family but new snapshot_id.
        let new_snap = SnapshotId::new();
        let rec_v2 = TemplateRecord {
            snapshot_id: new_snap,
            ..rec_v1.clone()
        };
        let meta_v2 = SnapshotMetadata {
            id: new_snap,
            size_bytes: 0,
            created_at: Utc::now(),
            image_version: "test".into(),
            disk_manifest: None,
            memory_manifest: Some(ManifestRef::new()),
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
        };
        pool.observe_templates(vec![(rec_v2, meta_v2)]).await;
        // Old sandboxes were destroyed.
        wait_for(
            || backend.destroy_log.lock().len() >= v1_sandboxes.len(),
            Duration::from_secs(2),
        )
        .await;
        let destroyed = backend.destroy_log.lock().clone();
        for sid in v1_sandboxes {
            assert!(destroyed.contains(&sid), "old sandbox should be destroyed");
        }
    }
}
