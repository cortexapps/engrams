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
    pub async fn complete(self, outcome: DrainOutcome, control_sock: &Path) {
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
                let seed = crate::pooled_backend::ChainSeed::PostCopy {
                    source,
                    sealed: &sealed,
                };
                let _ = self.finisher.seed_chain(self.id, seed).await;
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

/// The drain's terminal report on the handler's control socket. A failed
/// dial or read dials again: the handler replays its terminal report to
/// each new subscriber, and the destination role stays set until a
/// report arrives. The caller cancels this future (destroy).
pub(crate) async fn observe_drain_until_reported(sock: &Path) -> DrainOutcome {
    let mut delay = std::time::Duration::from_millis(100);
    loop {
        match observe_drain(sock).await {
            Ok(outcome) => return outcome,
            Err(error) => {
                tracing::warn!(sock = %sock.display(), %error,
                    "post-copy drain observer failed; dialing again");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_secs(5));
            }
        }
    }
}

async fn observe_drain(sock: &Path) -> std::io::Result<DrainOutcome> {
    use engram_migrate_proto::HandlerControl;
    let mut stream = tokio::net::UnixStream::connect(sock).await?;
    loop {
        let body = crate::migrate_peer::read_body_async(&mut stream).await?;
        let frame = engram_migrate_proto::decode_control(&body).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bincode: {e}"))
        })?;
        match frame {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A subscription that ends before the terminal report does not end
    /// the observation: the observer dials again and reads the report
    /// the handler replays, so the finish still clears the role.
    #[tokio::test]
    async fn a_failed_subscription_dials_again_until_the_report_arrives() {
        use engram_migrate_proto::HandlerControl;
        use tokio::io::AsyncWriteExt;
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("control.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let handler = tokio::spawn(async move {
            let (dropped, _) = listener.accept().await.unwrap();
            drop(dropped);
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            engram_migrate_proto::write_frame(
                &mut bytes,
                &HandlerControl::DrainDone {
                    pulled: 3,
                    alt_sourced: 0,
                    zero_chunks: 0,
                    ms: 1,
                    faults: 0,
                    fault_us: 0,
                    fault_max_us: 0,
                    fault_around_chunks_installed: 0,
                    hot_leading: 0,
                    hot_hint_received: false,
                },
            )
            .unwrap();
            stream.write_all(&bytes).await.unwrap();
        });
        let outcome = observe_drain_until_reported(&sock).await;
        assert!(
            matches!(outcome, DrainOutcome::Done { pulled: 3, .. }),
            "{outcome:?}"
        );
        handler.await.unwrap();
    }
}
