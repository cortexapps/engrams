//! The simulated teardown-reconcile world (ADR 0098 Phase 2, P3 — Flow C).
//!
//! [`SimReconcileBackend`] implements the host-agent's
//! [`ReconcileBackend`](engram_host_agent::teardown_reconcile::ReconcileBackend)
//! seam so the simulator drives the **REAL**
//! [`reconcile_once`](engram_host_agent::teardown_reconcile::reconcile_once)
//! tick against a modelled host. It is the RAM/durable split the pooled
//! backend actually has:
//!
//! * `live` — the FC VMs present on this host. A host-agent crash/restart
//!   does NOT reap them (pidfd reattach), so this SURVIVES a
//!   [`CrashProcess`](crate::Step::CrashProcess).
//! * `local_bindings` / `migration_roles` / `live_captures` — the in-RAM
//!   `PooledBackend`/`CaptureJobExecutor` tables. These DIE on a crash (the
//!   `DashMap`s live in process memory); the reconcile loop rebuilds
//!   `local_bindings` from the coordinator (ADR 0090) after a restart — the
//!   very None-arm path P3 pins.
//! * `destroyed` / `binding_repairs` — the oracle's memory: every local
//!   destroy and every binding repair reconcile performed, so Oracle #9 can
//!   prove no coord-owned sandbox was ever reaped and every unbound-but-owned
//!   sandbox was repaired.
//!
//! Coordinator ownership is NOT modelled here — it lives in
//! [`SimCoordClient`](crate::SimCoordClient) (a different process; it
//! survives the host crash), which is exactly the seam `reconcile_once` calls.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use engram_core::error::SandboxError;
use engram_core::{SandboxId, SessionId};
use engram_host_agent::teardown_reconcile::ReconcileBackend;
use parking_lot::Mutex;

/// One recorded local destroy — the oracle's memory of what reconcile reaped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DestroyRecord {
    pub sandbox_id: SandboxId,
    /// The local binding the sandbox had at destroy time (if any) — context
    /// for a failure artifact.
    pub had_binding: Option<SessionId>,
}

#[derive(Default)]
struct Inner {
    /// FC VMs present on the host (survive a host-agent crash).
    live: BTreeSet<SandboxId>,
    /// The in-RAM `sandbox → session` binding table (dies on crash).
    local_bindings: BTreeMap<SandboxId, SessionId>,
    /// In-RAM migration role notes (dies on crash).
    migration_roles: BTreeSet<SandboxId>,
    /// In-RAM live-capture registry (dies on crash).
    live_captures: BTreeSet<SandboxId>,
    /// Every local destroy reconcile performed, in order.
    destroyed: Vec<DestroyRecord>,
    /// Every `record_session_binding` repair reconcile performed, in order.
    binding_repairs: Vec<(SandboxId, SessionId)>,
    /// When set, the NEXT `destroy` fails (models a transient VMM error), so
    /// a test can prove the strike-debounce retries next tick. Cleared on use.
    fail_next_destroy: bool,
}

/// The simulator's [`ReconcileBackend`]. `parking_lot::Mutex`-backed interior
/// mutability, exactly like `PooledBackend`'s `DashMap`s — the trait methods
/// take `&self`.
#[derive(Default)]
pub struct SimReconcileBackend {
    inner: Mutex<Inner>,
}

impl SimReconcileBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a live, coord-owned, locally-bound sandbox (the steady state:
    /// reconcile is a no-op on it).
    pub fn seed_sandbox(&self, id: SandboxId, session: SessionId) {
        let mut inner = self.inner.lock();
        inner.live.insert(id);
        inner.local_bindings.insert(id, session);
    }

    /// Drop the LOCAL binding for a sandbox (models the ADR 0090 survivor:
    /// a pidfd-reattached VM whose NBD rehydrate bailed before the insert, or
    /// a fresh generation after a crash). The sandbox stays live and
    /// coord-owned; reconcile must REPAIR the binding, never reap.
    pub fn drop_local_binding(&self, id: SandboxId) {
        self.inner.lock().local_bindings.remove(&id);
    }

    /// Mark a sandbox as carrying a migration role (exempt).
    pub fn set_migration_role(&self, id: SandboxId) {
        self.inner.lock().migration_roles.insert(id);
    }

    /// Mark a sandbox as a live base-capture VM (exempt).
    pub fn set_live_capture(&self, id: SandboxId) {
        self.inner.lock().live_captures.insert(id);
    }

    /// Arm a one-shot destroy failure for the next reap.
    pub fn fail_next_destroy(&self) {
        self.inner.lock().fail_next_destroy = true;
    }

    /// Wipe the in-RAM tables a host-agent crash loses (`local_bindings`,
    /// `migration_roles`, `live_captures`) while leaving the live FC set and
    /// the oracle logs intact. The successor's reconcile loop rebuilds the
    /// bindings from the coordinator.
    pub fn crash_ram(&self) {
        let mut inner = self.inner.lock();
        inner.local_bindings.clear();
        inner.migration_roles.clear();
        inner.live_captures.clear();
    }

    /// Is this sandbox still live (not reaped)?
    pub fn is_live(&self, id: SandboxId) -> bool {
        self.inner.lock().live.contains(&id)
    }

    /// The current local binding, if any (test/oracle read).
    pub fn binding(&self, id: SandboxId) -> Option<SessionId> {
        self.inner.lock().local_bindings.get(&id).copied()
    }

    /// The destroy log (oracle memory).
    pub fn destroyed(&self) -> Vec<DestroyRecord> {
        self.inner.lock().destroyed.clone()
    }

    /// The binding-repair log (oracle/test read).
    pub fn binding_repairs(&self) -> Vec<(SandboxId, SessionId)> {
        self.inner.lock().binding_repairs.clone()
    }

    /// Snapshot of the currently-live sandboxes (deterministic order).
    pub fn live_ids(&self) -> Vec<SandboxId> {
        self.inner.lock().live.iter().copied().collect()
    }
}

#[async_trait]
impl ReconcileBackend for SimReconcileBackend {
    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        // BTreeSet iteration → deterministic order (never a HashSet).
        Ok(self.inner.lock().live.iter().copied().collect())
    }

    fn migration_role_present(&self, id: SandboxId) -> bool {
        self.inner.lock().migration_roles.contains(&id)
    }

    fn is_live_capture(&self, id: SandboxId) -> bool {
        self.inner.lock().live_captures.contains(&id)
    }

    fn capture_in_flight(&self, _id: SandboxId) -> bool {
        // The host-internal sim keeps the teardown-reconcile world
        // decoupled from the eviction-finalize model by design (P3: a
        // reconcile destroy never touches a `SandboxSlot`); this sim never
        // co-simulates a D5 capture racing a reconcile tick. The
        // finalize-in-flight-vs-reconcile race is the R-CoSim boundary
        // scenario (`engram-dst-cosim`), where a real capture lock drives
        // this signal. Here it is always `false` — honest for a model
        // with no in-flight capture concept.
        false
    }

    fn session_for_sandbox(&self, id: SandboxId) -> Option<SessionId> {
        self.inner.lock().local_bindings.get(&id).copied()
    }

    fn record_session_binding(&self, id: SandboxId, session: SessionId) {
        let mut inner = self.inner.lock();
        inner.local_bindings.insert(id, session);
        inner.binding_repairs.push((id, session));
    }

    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
        let mut inner = self.inner.lock();
        if inner.fail_next_destroy {
            inner.fail_next_destroy = false;
            // A transient VMM error — the strike ledger keeps the count at
            // threshold and retries next tick.
            return Err(SandboxError::InvalidSpec(
                "sim: injected destroy failure".to_string(),
            ));
        }
        let had_binding = inner.local_bindings.remove(&id);
        inner.live.remove(&id);
        inner.destroyed.push(DestroyRecord {
            sandbox_id: id,
            had_binding,
        });
        Ok(())
    }
}
