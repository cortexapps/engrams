//! Oracle #1 — the acked-write durability oracle (ADR 0098 Phase 2,
//! invariant #1; the session-85e0298a corruption RCA, PR #712).
//!
//! **Property:** *every guest-acked write is recoverable from (the rebuilt
//! backend's tier resolution ∪ the shutdown spool ∪ the published/uploaded
//! chunks in the store) — at every crash point.*
//!
//! The ledger ([`AckedWriteLedger`]) is the oracle's memory: the LATEST
//! acked `content_tag` per `(sandbox, chunk_idx)`. The check reads each such
//! chunk back through the sandbox's **live** backend and asserts the decoded
//! tag equals the acked tag. Reading through the live backend IS the union:
//! `from_blob(published_ref)` resolves the uploaded/published tier, and
//! spool adoption seeds the dirty tier — so a post-restart backend that
//! reads the acked tag proves recoverability across all three sources.
//!
//! When a sandbox has NO live backend (crashed, not yet restarted), its
//! chunks are SKIPPED — the property is about recoverability *after*
//! recovery, so it becomes checkable only once the successor rebuilds. Run
//! after every step, the check is therefore meaningful exactly at the
//! moments that matter (post-write, post-flush, post-adopt, and — the
//! headline case — post `CrashProcess → Restart`).
//!
//! In P2 this **passes by construction**: `CrashProcess` completes the
//! shutdown spool before dropping RAM, so `Restart`'s rebuild+adopt recovers
//! every acked write. A FAILURE here is a real finding in the shipped
//! flush/spool/adopt machinery — pin the seed, do not weaken the oracle.

use crate::world::{decode_tag, SimHost, CHUNK_SIZE};

#[derive(Debug)]
pub struct Violation {
    pub invariant: &'static str,
    pub detail: String,
}

/// Check the acked-write durability oracle against the host's current state.
/// Async because it reads back through the live backends (in-memory tier
/// resolution — microseconds).
pub async fn check(host: &SimHost) -> Result<(), Violation> {
    acked_writes_recoverable(host).await
}

async fn acked_writes_recoverable(host: &SimHost) -> Result<(), Violation> {
    // Snapshot the recoverable set (latest tag per chunk) under a
    // deterministic BTreeMap order, then read each back through its live
    // backend.
    for ((idx, chunk_idx), entry) in host.ledger.latest_by_chunk() {
        let Some(slot) = host.sandboxes.get(idx) else {
            continue;
        };
        let Some(backend) = slot.backend.clone() else {
            // Crashed, not yet restarted — recoverability is checkable only
            // after the successor rebuilds.
            continue;
        };
        let bytes = backend
            .read(chunk_idx * CHUNK_SIZE, CHUNK_SIZE)
            .await
            .map_err(|e| Violation {
                invariant: "acked-write-durability",
                detail: format!("sandbox {idx} chunk {chunk_idx} read failed: {e}"),
            })?;
        let got = decode_tag(&bytes);
        if got != entry.content_tag {
            return Err(Violation {
                invariant: "acked-write-durability",
                detail: format!(
                    "sandbox {idx} chunk {chunk_idx}: acked tag {} not recoverable (read {got}); \
                     lineage-at-ack {}v{}",
                    entry.content_tag,
                    entry.lineage_at_ack.manifest_id,
                    entry.lineage_at_ack.version
                ),
            });
        }
    }
    Ok(())
}
