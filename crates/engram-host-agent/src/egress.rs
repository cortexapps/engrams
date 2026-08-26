//! The host-agent's face of the egress proxy.
//!
//! ADR 0006 put the proxy in this process; ADR 0121 moved it into
//! `engram-egress-proxyd`, a node-local daemon that outlives host-agent
//! pods so in-flight guest streams survive a roll. What remains here is
//! the control-plane facade: apply/remove/sync policies over the
//! daemon's UDS, the CA PEM the guest trust stores need, and the
//! supervision pieces `lib.rs` arms once the backend exists.

use std::sync::Arc;

use engram_core::SessionId;

use crate::proxyd_client::{PolicyReplay, ProxydHandle, ProxydProcess, ProxydSpawnConfig};

/// Per-host egress handle: the daemon's control client plus the CA
/// cert PEM (handed to `ensure_harness_ext4` so every guest substrate
/// this host builds trusts the daemon's MITM leaves).
pub struct HostEgress {
    pub ca_cert_pem: String,
    handle: Arc<ProxydHandle>,
    /// The live process + respawn config, stashed by `main.rs` until
    /// `lib.rs` can arm the supervisor (the replay needs the pooled
    /// backend, which does not exist yet at ensure time). `None` in
    /// test harnesses that run the daemon as an in-process task.
    supervision: std::sync::Mutex<Option<(ProxydProcess, ProxydSpawnConfig)>>,
}

impl HostEgress {
    pub fn new(
        handle: Arc<ProxydHandle>,
        ca_cert_pem: String,
        supervision: Option<(ProxydProcess, ProxydSpawnConfig)>,
    ) -> Self {
        Self {
            ca_cert_pem,
            handle,
            supervision: std::sync::Mutex::new(supervision),
        }
    }

    /// The raw control client — the test harnesses' assertion surface
    /// (`lookup_guest` / `decide` / `health`).
    pub fn handle(&self) -> Arc<ProxydHandle> {
        self.handle.clone()
    }

    /// Register (or replace) one session's policy with the daemon.
    /// The ack stays honest: an error here fails the caller's apply
    /// (ADR 0111 — never ack a policy that is not enforced).
    pub async fn apply_policy(
        &self,
        policy: engram_core::types::egress::SessionEgressPolicy,
    ) -> Result<(), crate::proxyd_client::ProxydError> {
        self.handle.apply_policy(policy).await
    }

    /// Drop a session's registration AND its tunnel state (the daemon
    /// fans `session_closed` to every tunnel upstream server-side).
    /// Fire-and-forget: removal is idempotent, callers sit on sync
    /// (destroy) paths that must not block on the daemon, and the next
    /// `SyncPolicies` prunes anything a lost removal left behind.
    pub fn unregister_session(&self, session_id: SessionId) {
        let handle = self.handle.clone();
        tokio::spawn(async move {
            if let Err(e) = handle.remove_session(session_id).await {
                tracing::warn!(%session_id, error = %e, "egress remove_session failed (next sync prunes it)");
            }
        });
    }

    /// Full replace of the daemon's registry (ADR 0111 replay +
    /// stale-entry prune in one idempotent op).
    pub async fn sync_policies(
        &self,
        policies: Vec<engram_core::types::egress::SessionEgressPolicy>,
    ) -> Result<(), crate::proxyd_client::ProxydError> {
        self.handle.sync_policies(policies).await
    }

    /// Arm the event-driven supervisor (respawn + replay on daemon
    /// exit). Called once from `lib.rs` when the replay closure can be
    /// built; a no-op for handles with no stashed process (tests).
    pub fn spawn_supervisor(&self, replay: PolicyReplay) {
        let stashed = self
            .supervision
            .lock()
            .expect("supervision stash lock never poisons")
            .take();
        if let Some((process, cfg)) = stashed {
            crate::proxyd_client::spawn_supervisor(process, self.handle.clone(), cfg, replay);
        }
    }
}
