//! The simulated coordinator control plane (ADR 0098 Phase 2, P2).
//!
//! [`SimCoordClient`] implements [`CoordControlPlane`] — the three
//! decision-feeding coordinator calls the host-agent's lifecycle flows make
//! (`publish_live_manifest`, `sandbox_ownership`, `sandbox_owner`). It is a
//! BTreeMap-backed ownership model plus a recorded log of publishes, and it
//! SURVIVES a host [`CrashProcess`](crate::Step::CrashProcess): the
//! coordinator is a different process from the host-agent, so its beliefs
//! about who-owns-what outlive the host RAM.
//!
//! **The adversarial capability is wired but benign by default.** A
//! [`ScriptedResponse`] queue lets a future (P3+) scheduler script the
//! nasty cases the seam exists for — ownership that flips mid-export, a lost
//! publish ack, a vanishing coordinator — without any of them firing in P2.
//! With an empty queue every call answers from the honest ownership model.

use std::collections::{BTreeMap, VecDeque};

use async_trait::async_trait;
use engram_core::{HostId, SandboxId, SessionId};
use engram_host_core::{
    CoordControlPlane, CoordError, LiveManifestPublishOutcome, LiveManifestPublishRequest,
    LiveManifestPublishResponse,
};
use parking_lot::Mutex;

/// One recorded `publish_live_manifest` call — the sim's audit log of the
/// survivor-publish traffic Flow A/D generates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedPublish {
    pub host_id: HostId,
    pub session_id: SessionId,
    pub sandbox_id: SandboxId,
    pub manifest_id: uuid::Uuid,
    pub manifest_version: u64,
    pub outcome: LiveManifestPublishOutcome,
}

/// A scripted override the scheduler can push to make the NEXT matching call
/// adversarial. Benign-by-default: with none queued, calls answer from the
/// honest ownership model. Wired in P2, exercised in P3+.
#[derive(Clone, Debug)]
pub enum ScriptedResponse {
    /// Next `publish_live_manifest` returns `Stale` regardless of ownership
    /// (a rebind the host has not learned yet).
    PublishStale,
    /// Next control-plane call fails as if the coordinator is unreachable.
    Unreachable(&'static str),
    /// Next `sandbox_ownership` answers with this boolean (an ownership flip
    /// mid-export).
    Ownership(bool),
}

#[derive(Default)]
struct Inner {
    /// sandbox → owning session (the coordinator's binding table).
    ownership: BTreeMap<SandboxId, SessionId>,
    /// Every publish, in call order.
    publishes: Vec<RecordedPublish>,
    /// Scripted overrides consumed front-to-back. Empty ⇒ benign.
    scripted: VecDeque<ScriptedResponse>,
}

/// A BTreeMap-backed stand-in for the coordinator's control plane.
#[derive(Default)]
pub struct SimCoordClient {
    inner: Mutex<Inner>,
}

impl SimCoordClient {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the honest ownership model: sandbox is bound to session.
    pub fn set_owner(&self, sandbox_id: SandboxId, session_id: SessionId) {
        self.inner.lock().ownership.insert(sandbox_id, session_id);
    }

    /// Drop a sandbox from the honest ownership model — the coordinator no
    /// longer binds it (a terminal/idle/rebound session). `sandbox_owner`
    /// then answers `None`; `sandbox_ownership` answers `false`. Used by the
    /// reconcile scenarios (ADR 0098 P3) to model a genuinely-departed
    /// ownership that reconcile SHOULD reap.
    pub fn revoke_owner(&self, sandbox_id: SandboxId) {
        self.inner.lock().ownership.remove(&sandbox_id);
    }

    /// Read the honest ownership model WITHOUT consuming a scripted override
    /// — the invariant oracle's view of who-really-owns-what. (The
    /// `CoordControlPlane` methods consume the scripted queue; the oracle
    /// must not perturb it.)
    pub fn honest_owner(&self, sandbox_id: SandboxId) -> Option<SessionId> {
        self.inner.lock().ownership.get(&sandbox_id).copied()
    }

    /// Push a scripted override for the next matching call (adversarial
    /// capability — unused in P2).
    pub fn script(&self, resp: ScriptedResponse) {
        self.inner.lock().scripted.push_back(resp);
    }
}

#[async_trait]
impl CoordControlPlane for SimCoordClient {
    async fn publish_live_manifest(
        &self,
        host_id: HostId,
        req: &LiveManifestPublishRequest,
    ) -> Result<LiveManifestPublishResponse, CoordError> {
        let mut inner = self.inner.lock();
        // Honor a single scripted override at the front of the queue.
        match inner.scripted.front().cloned() {
            Some(ScriptedResponse::Unreachable(what)) => {
                inner.scripted.pop_front();
                return Err(CoordError::Transport(what.to_string()));
            }
            Some(ScriptedResponse::PublishStale) => {
                inner.scripted.pop_front();
                inner.publishes.push(RecordedPublish {
                    host_id,
                    session_id: req.session_id,
                    sandbox_id: req.sandbox_id,
                    manifest_id: req.manifest_id,
                    manifest_version: req.manifest_version,
                    outcome: LiveManifestPublishOutcome::Stale,
                });
                return Ok(LiveManifestPublishResponse {
                    outcome: LiveManifestPublishOutcome::Stale,
                });
            }
            _ => {}
        }
        // Honest model: `applied` iff coord still binds this sandbox to the
        // publishing session (or has no belief yet — a fresh sandbox).
        let outcome = match inner.ownership.get(&req.sandbox_id) {
            Some(owner) if *owner != req.session_id => LiveManifestPublishOutcome::Stale,
            _ => LiveManifestPublishOutcome::Applied,
        };
        inner.publishes.push(RecordedPublish {
            host_id,
            session_id: req.session_id,
            sandbox_id: req.sandbox_id,
            manifest_id: req.manifest_id,
            manifest_version: req.manifest_version,
            outcome,
        });
        Ok(LiveManifestPublishResponse { outcome })
    }

    async fn sandbox_ownership(
        &self,
        _host_id: HostId,
        session_id: SessionId,
        sandbox_id: SandboxId,
    ) -> Result<bool, CoordError> {
        let mut inner = self.inner.lock();
        match inner.scripted.front().cloned() {
            Some(ScriptedResponse::Unreachable(what)) => {
                inner.scripted.pop_front();
                return Err(CoordError::Transport(what.to_string()));
            }
            Some(ScriptedResponse::Ownership(v)) => {
                inner.scripted.pop_front();
                return Ok(v);
            }
            _ => {}
        }
        Ok(inner.ownership.get(&sandbox_id) == Some(&session_id))
    }

    async fn sandbox_owner(
        &self,
        _host_id: HostId,
        sandbox_id: SandboxId,
    ) -> Result<Option<SessionId>, CoordError> {
        let mut inner = self.inner.lock();
        if let Some(ScriptedResponse::Unreachable(what)) = inner.scripted.front().cloned() {
            inner.scripted.pop_front();
            return Err(CoordError::Transport(what.to_string()));
        }
        Ok(inner.ownership.get(&sandbox_id).copied())
    }
}
