//! The post-copy destination's finish. When the handler reports the
//! memory drain done, the destination publishes its own memory lineage:
//! the source's chain manifest with the peer-supplied chunks written
//! over it. That manifest is the guest's memory at load, and the KVM
//! dirty log has run since load, so the first checkpoint is a diff.
//!
//! The persisted destination role is cleared at once: a restart reaps a
//! sandbox whose manifest still names it, and after the drain the guest
//! no longer depends on the source. The in-memory role stays until the
//! seed is done, and `capture_phase` refuses a capture while it is set:
//! a capture that ran first would consume the dirty log the seed needs.
//! A restart during the seed loses only the seed (no chain-head record
//! exists yet), so the first checkpoint is Full.

use std::path::Path;
use std::sync::Arc;

use dashmap::DashMap;
use engram_core::traits::SandboxBackend;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::snapshot::DrainOutcome;
use engram_core::SandboxId;

use crate::migration::MigrationRole;
use crate::pooled_backend::SnapshotFinisher;

/// Everything the finish needs, owned, so it runs detached from the
/// restore that spawned it.
pub(crate) struct PostCopyFinish {
    pub id: SandboxId,
    pub inner: Arc<dyn SandboxBackend>,
    pub finisher: SnapshotFinisher,
    /// The source chain manifest the destination restored against.
    pub source_manifest: Option<ManifestRef>,
    pub migration_roles: Arc<DashMap<SandboxId, MigrationRole>>,
    pub drained_dests: Arc<DashMap<SandboxId, DrainOutcome>>,
}

impl PostCopyFinish {
    /// Act on the drain outcome read from `control_sock`. On `Done`, clear
    /// the persisted role, seed the chain, then clear the in-memory role.
    /// `PeerLost` leaves the role set: the coordinator destroys this
    /// destination.
    pub async fn complete(self, outcome: std::io::Result<DrainOutcome>, control_sock: &Path) {
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::warn!(sandbox_id = %self.id, %error,
                    "post-copy drain observer failed; the destination keeps its role");
                return;
            }
        };
        if matches!(outcome, DrainOutcome::PeerLost { .. }) {
            return;
        }
        self.drained_dests.insert(self.id, outcome);
        if !self.clear_persisted_role().await {
            return;
        }
        let sealed = control_sock.with_file_name(engram_sandbox_firecracker::POST_COPY_SEALED_FILE);
        match self.source_manifest {
            Some(source) if sealed.exists() => {
                let started = crate::time_source::metrics_now();
                match self
                    .finisher
                    .seed_checkpoint_chain_post_copy(self.id, source, &sealed)
                    .await
                {
                    Ok(fork) => tracing::info!(
                        sandbox_id = %self.id,
                        %source,
                        %fork,
                        seed_ms = started.elapsed().as_millis() as u64,
                        "post-copy chain seeded; the first checkpoint diffs against it",
                    ),
                    Err(error) => tracing::warn!(
                        sandbox_id = %self.id,
                        %error,
                        "post-copy chain seed failed; the first checkpoint is Full",
                    ),
                }
            }
            _ => tracing::info!(sandbox_id = %self.id,
                "no complete sealed output; the first checkpoint is Full"),
        }
        let _ = tokio::fs::remove_file(&sealed).await;
        self.migration_roles.remove(&self.id);
    }

    /// Retries until the write lands (`true`) or the sandbox is gone
    /// (`false`); destroy cancels this task.
    async fn clear_persisted_role(&self) -> bool {
        let mut delay = std::time::Duration::from_millis(100);
        loop {
            match self.inner.set_manifest_migration_role(self.id, None).await {
                Ok(()) => return true,
                Err(engram_core::SandboxError::NotFound) => return false,
                Err(error) => {
                    tracing::warn!(sandbox_id = %self.id, %error,
                        "post-copy role clear failed; retrying");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(std::time::Duration::from_secs(30));
                }
            }
        }
    }
}

/// The drain's terminal report on the handler's control socket.
pub(crate) async fn observe_drain(sock: &Path) -> std::io::Result<DrainOutcome> {
    use engram_migrate_proto::HandlerControl;
    let mut stream = tokio::net::UnixStream::connect(sock).await?;
    loop {
        match crate::migrate_peer::read_frame_async(&mut stream).await? {
            HandlerControl::DrainDone {
                pulled,
                alt_sourced,
                zero_chunks,
                ms,
                ..
            } => {
                return Ok(DrainOutcome::Done {
                    pulled,
                    alt_sourced,
                    zero_chunks,
                    ms,
                })
            }
            HandlerControl::PeerLost { remaining, detail } => {
                return Ok(DrainOutcome::PeerLost { remaining, detail })
            }
            HandlerControl::Sealed { .. } | HandlerControl::DrainProgress { .. } => {}
        }
    }
}
