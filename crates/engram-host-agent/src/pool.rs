//! Warm pool. Per (repo, image_version), keep N microVMs idle so
//! session checkout is sub-second. Background reconciliation replaces
//! VMs as they're checked out.
//!
//! The pool stores a `SandboxSpec` per key — that's the recipe
//! `replenish` uses to call `backend.create` for missing slots. Spec
//! must NOT include session-specific fields (e.g. `ENGRAM_SESSION_ID`
//! env var); pooled sandboxes are anonymous until checkout, at which
//! point the per-session env is layered on at exec time.

use std::collections::HashMap;
use std::sync::Arc;

use engram_core::traits::SandboxBackend;
use engram_core::types::ids::SandboxId;
use engram_core::types::sandbox::SandboxSpec;
use engram_core::SandboxError;
use parking_lot::Mutex;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct PoolKey {
    pub repo: String,
    pub image_version: String,
}

#[derive(Clone)]
pub struct Pool {
    backend: Arc<dyn SandboxBackend>,
    inner: Arc<Mutex<PoolInner>>,
}

#[derive(Default)]
struct PoolInner {
    ready: HashMap<PoolKey, Vec<SandboxId>>,
    cfg: HashMap<PoolKey, PoolEntry>,
}

#[derive(Clone)]
struct PoolEntry {
    target: u32,
    spec: SandboxSpec,
}

impl Pool {
    pub fn new(backend: Arc<dyn SandboxBackend>) -> Self {
        Self {
            backend,
            inner: Arc::new(Mutex::new(PoolInner::default())),
        }
    }

    /// Declare or update the desired warm-pool state for `key`. Spec
    /// is the recipe used by `replenish` to spawn replacement
    /// sandboxes. Calling `configure` with the same key updates both
    /// target and spec — useful when an image is refreshed (new
    /// `image_version`) and we want subsequent replenishes to use the
    /// new spec without disturbing already-ready sandboxes.
    pub fn configure(&self, key: PoolKey, target: u32, spec: SandboxSpec) {
        let mut g = self.inner.lock();
        g.cfg.insert(key, PoolEntry { target, spec });
    }

    /// Pre-1.0 helper kept for tests that don't care about replenish:
    /// set just the target without a spec. `replenish` will skip a
    /// key that has no spec (it has no recipe to use).
    pub fn set_target(&self, key: PoolKey, target: u32) {
        let mut g = self.inner.lock();
        let entry = g.cfg.entry(key).or_insert_with(|| PoolEntry {
            target: 0,
            spec: stub_spec(),
        });
        entry.target = target;
    }

    pub fn ready_count(&self, key: &PoolKey) -> u32 {
        self.inner
            .lock()
            .ready
            .get(key)
            .map(|v| v.len() as u32)
            .unwrap_or(0)
    }

    pub fn target(&self, key: &PoolKey) -> u32 {
        self.inner
            .lock()
            .cfg
            .get(key)
            .map(|e| e.target)
            .unwrap_or(0)
    }

    /// Build the per-pool view the heartbeat ships to the coordinator.
    /// Iterates configured pool entries (the ones for which `replenish`
    /// has a recipe); a pool that has only ever had `ready` items
    /// pushed without `configure` would be omitted, but that path
    /// isn't used in production.
    pub fn snapshot_reports(&self) -> Vec<engram_protocol::WarmPoolReport> {
        let g = self.inner.lock();
        g.cfg
            .iter()
            .map(|(key, entry)| {
                let ready = g.ready.get(key).map(|v| v.len() as u32).unwrap_or(0);
                engram_protocol::WarmPoolReport {
                    repo: key.repo.clone(),
                    image_version: key.image_version.clone(),
                    ready,
                    target: entry.target,
                }
            })
            .collect()
    }

    /// Take a warm sandbox if one is ready. The caller is responsible
    /// for triggering replenish to top the pool back up.
    pub fn checkout(&self, key: &PoolKey) -> Option<SandboxId> {
        self.inner.lock().ready.get_mut(key).and_then(|v| v.pop())
    }

    /// Hand a fresh / recycled sandbox back into the pool. Called by
    /// `replenish` after a successful `backend.create`, and also by
    /// the host agent when a session ends and the underlying VM is
    /// returned to the pool instead of being destroyed.
    pub fn push_ready(&self, key: PoolKey, id: SandboxId) {
        self.inner.lock().ready.entry(key).or_default().push(id);
    }

    /// Reconcile `ready_count(key)` toward `target(key)` by calling
    /// `backend.create(spec)` for each missing slot. Returns the
    /// number of new sandboxes pushed into the pool. If the key has
    /// no configured spec (e.g. `set_target` was used without
    /// `configure`), this is a no-op.
    ///
    /// Errors mid-loop terminate replenish for this key — partial
    /// progress is preserved (sandboxes already created stay in the
    /// pool). Caller is expected to retry on a future reconciliation
    /// tick.
    pub async fn replenish(&self, key: &PoolKey) -> Result<usize, SandboxError> {
        // Snapshot config + current state under one lock acquisition;
        // we don't want to hold the mutex across `backend.create`.
        let (need, spec) = {
            let g = self.inner.lock();
            let entry = match g.cfg.get(key) {
                Some(e) if !is_stub_spec(&e.spec) => e.clone(),
                // No spec → nothing to create; be silent so callers
                // can call replenish unconditionally.
                _ => return Ok(0),
            };
            let ready = g.ready.get(key).map(|v| v.len() as u32).unwrap_or(0);
            let need = entry.target.saturating_sub(ready);
            (need, entry.spec)
        };

        let mut created = 0;
        for _ in 0..need {
            let id = self.backend.create(spec.clone()).await?;
            self.push_ready(key.clone(), id);
            created += 1;
        }
        Ok(created)
    }
}

/// Sentinel spec — placeholder used by `set_target` when no real spec
/// has been configured. `replenish` skips entries whose spec equals
/// this so callers can opt into "warm pool" without a recipe.
fn stub_spec() -> SandboxSpec {
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit};
    SandboxSpec {
        image: String::new(),
        rootfs_source: None,
        cpu: CpuLimit { vcpus: 0 },
        memory: MemoryLimit { max_mib: 0 },
        disk: DiskLimit { max_gib: 0 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        agent: None,
    }
}

fn is_stub_spec(spec: &SandboxSpec) -> bool {
    spec.image.is_empty() && spec.cpu.vcpus == 0 && spec.memory.max_mib == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit};
    use engram_sandbox_process::ProcessBackend;

    fn key(repo: &str, ver: &str) -> PoolKey {
        PoolKey {
            repo: repo.into(),
            image_version: ver.into(),
        }
    }

    fn pool() -> (Pool, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Pool::new(Arc::new(ProcessBackend::new(dir.path()))), dir)
    }

    fn live_spec() -> SandboxSpec {
        SandboxSpec {
            image: "warm-test".into(),
            rootfs_source: None,
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 256 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: HashMap::new(),
            workdir: None,
            agent: None,
        }
    }

    #[test]
    fn checkout_from_unknown_key_is_none() {
        let (p, _d) = pool();
        assert_eq!(p.checkout(&key("a", "v1")), None);
        assert_eq!(p.ready_count(&key("a", "v1")), 0);
    }

    #[test]
    fn push_ready_then_checkout_returns_lifo_order() {
        let (p, _d) = pool();
        let k = key("repo", "warm-1");
        let id1 = SandboxId::new();
        let id2 = SandboxId::new();
        let id3 = SandboxId::new();
        p.push_ready(k.clone(), id1);
        p.push_ready(k.clone(), id2);
        p.push_ready(k.clone(), id3);
        assert_eq!(p.ready_count(&k), 3);

        // LIFO: most-recently-pushed comes out first. Hot-cache wins.
        assert_eq!(p.checkout(&k), Some(id3));
        assert_eq!(p.checkout(&k), Some(id2));
        assert_eq!(p.checkout(&k), Some(id1));
        assert_eq!(p.checkout(&k), None);
        assert_eq!(p.ready_count(&k), 0);
    }

    #[test]
    fn pool_keys_are_isolated() {
        let (p, _d) = pool();
        let a = key("repo-a", "warm-1");
        let b = key("repo-b", "warm-1");
        let c = key("repo-a", "warm-2");
        let id_a = SandboxId::new();
        let id_b = SandboxId::new();
        p.push_ready(a.clone(), id_a);
        p.push_ready(b.clone(), id_b);

        assert_eq!(p.ready_count(&a), 1);
        assert_eq!(p.ready_count(&b), 1);
        assert_eq!(
            p.ready_count(&c),
            0,
            "different image_version is its own pool"
        );

        // Checkout from one key must not drain another.
        assert_eq!(p.checkout(&a), Some(id_a));
        assert_eq!(p.ready_count(&b), 1);
        assert_eq!(p.checkout(&b), Some(id_b));
    }

    #[test]
    fn set_target_does_not_seed_ready_pool() {
        // Setting a target is a *desired* state; only replenish (via
        // the SandboxBackend) actually produces ready VMs. The two
        // remain decoupled.
        let (p, _d) = pool();
        let k = key("repo", "warm-1");
        p.set_target(k.clone(), 4);
        assert_eq!(p.ready_count(&k), 0);
        assert_eq!(p.target(&k), 4);
    }

    #[tokio::test]
    async fn replenish_creates_sandboxes_until_target_met() {
        let (p, _d) = pool();
        let k = key("repo", "warm-1");
        p.configure(k.clone(), 3, live_spec());
        let created = p.replenish(&k).await.unwrap();
        assert_eq!(created, 3);
        assert_eq!(p.ready_count(&k), 3);
    }

    #[tokio::test]
    async fn replenish_is_idempotent_when_target_is_met() {
        let (p, _d) = pool();
        let k = key("repo", "warm-1");
        p.configure(k.clone(), 2, live_spec());
        assert_eq!(p.replenish(&k).await.unwrap(), 2);
        // Second replenish at full pool: no-op.
        assert_eq!(p.replenish(&k).await.unwrap(), 0);
        assert_eq!(p.ready_count(&k), 2);
    }

    #[tokio::test]
    async fn checkout_then_replenish_tops_pool_back_up() {
        let (p, _d) = pool();
        let k = key("repo", "warm-1");
        p.configure(k.clone(), 2, live_spec());
        p.replenish(&k).await.unwrap();
        assert_eq!(p.ready_count(&k), 2);

        let _checked_out = p.checkout(&k).unwrap();
        assert_eq!(p.ready_count(&k), 1);

        let created = p.replenish(&k).await.unwrap();
        assert_eq!(created, 1, "replenish creates only the missing slot");
        assert_eq!(p.ready_count(&k), 2);
    }

    #[tokio::test]
    async fn replenish_skips_keys_without_a_configured_spec() {
        // `set_target` without `configure` leaves the pool key
        // recipe-less. Replenish silently skips so callers (e.g. the
        // background reconciler) can run unconditionally.
        let (p, _d) = pool();
        let k = key("repo", "warm-1");
        p.set_target(k.clone(), 5);
        let created = p.replenish(&k).await.unwrap();
        assert_eq!(created, 0);
        assert_eq!(p.ready_count(&k), 0);
    }

    #[tokio::test]
    async fn configure_updates_spec_for_subsequent_replenishes() {
        // Image refresh: new image_version published → coordinator
        // calls configure with the new spec. Already-ready sandboxes
        // (from the old spec) stay in the pool until checked out;
        // replenish slots get the new spec.
        let (p, _d) = pool();
        let k = key("repo", "warm-1");
        let mut spec_v1 = live_spec();
        spec_v1.image = "warm-v1".into();
        p.configure(k.clone(), 2, spec_v1);
        p.replenish(&k).await.unwrap();
        // Drain to expose new replenishes.
        let _ = p.checkout(&k);
        let _ = p.checkout(&k);

        let mut spec_v2 = live_spec();
        spec_v2.image = "warm-v2".into();
        p.configure(k.clone(), 2, spec_v2);
        let created = p.replenish(&k).await.unwrap();
        assert_eq!(created, 2);
        // The new sandboxes use spec_v2 — verifying via the backend's
        // own state is per-backend, but the configure call is what we
        // care about here.
        assert_eq!(p.ready_count(&k), 2);
    }
}
