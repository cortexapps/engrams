//! Oracle #1 — the acked-write durability oracle (ADR 0098 Phase 2,
//! invariant #1; the session-85e0298a corruption RCA, PR #712; oracle-honesty
//! pass P4.5).
//!
//! **Property (the honest, bidirectional form):** *every guest-acked write
//! that had a durable HANDOFF before a crash — a flush published it, OR the
//! shutdown spool captured it — is recoverable after restart, read back
//! through the REAL recovery path (`from_blob(published_ref)` + spool
//! `adopt_unflushed`). A write acked from the RAM dirty tier and lost to
//! abrupt process death BEFORE any flush or spool is an accepted, bounded loss
//! (bounded by the flush cadence + the periodic checkpoint, ADR 0028), NOT a
//! violation.* The oracle verifies the durability PIPELINE
//! (flush→publish→spool→adopt) never loses a write it took responsibility for
//! — it does not claim omniscient recovery of every RAM-only ack.
//!
//! The ledger ([`AckedWriteLedger`]) is the oracle's memory. Per
//! `(sandbox, chunk_idx)` it holds the LATEST acked tag AND the published-tier
//! FLOOR (the highest tag a flush published to the durable/uploaded tier,
//! observed from the REAL published manifest — see [`crate::world`]). For each
//! chunk with a live backend, the check reads the chunk back and requires the
//! decoded tag to fall in the honest RANGE `[published_floor, latest_ack]` (by
//! tag order). Everything in that range is legitimate:
//!
//! * `== latest_ack` — a live backend that never crashed still holds the newest
//!   write; and a rebuild where the latest write WAS itself published has
//!   floor == latest.
//! * `== published_floor` — a rebuild that dropped newer, un-published writes;
//!   those newer writes are an ACCEPTED, bounded loss (flush cadence + periodic
//!   checkpoint), never demanded back.
//! * strictly between — a transiently-durable intermediate that a STANDING
//!   shutdown spool adopted at recovery (the spool preserves an un-published
//!   write across one roll; it is not a permanent floor).
//!
//! Only two reads are violations: OLDER than the published floor (a durable
//! published write rolled back — the 85e0298a corruption class) or NEWER than
//! the latest ack (a never-acked tag). The published floor is the ONLY
//! permanent durability promise; spool RECOVERY is asserted separately by the
//! regression seeds that crash with a STANDING spool. Reading through the live
//! backend IS the recovery path: `from_blob(published_ref)` resolves the
//! uploaded/published tier and spool adoption seeds the dirty tier — never a
//! raw blob-existence check.
//!
//! When a sandbox has NO live backend (crashed, not yet restarted), its
//! chunks are SKIPPED — the property is about recoverability *after* recovery,
//! so it becomes checkable only once the successor rebuilds. Run after every
//! step, the check is meaningful exactly at the moments that matter
//! (post-write, post-flush, post-adopt, and the headline cases:
//! `CrashProcess → Restart`, and — P4.5 — `AbruptCrash → Restart`, where a
//! post-ack/pre-handoff write is correctly TOLERATED as lost).
//!
//! A FAILURE here is a real finding in the shipped flush/spool/adopt machinery
//! — a write the pipeline DID hand off but cannot recover. Pin the seed, do
//! not weaken the oracle. The regression seed
//! `post_ack_pre_handoff_crash_is_honest_loss` pins the honest boundary from
//! the other side: an un-handed-off write is lost and the oracle does NOT cry
//! wolf.
//!
//! # Oracle #9 — the reconcile None-arm stays fixed (ADR 0098 P3)
//!
//! **Property:** *teardown reconcile NEVER destroys a sandbox the coordinator
//! still owns.* The 2026-07-11 mis-reap was a missing LOCAL binding read as
//! "orphan" and SIGKILLing a legitimately-owned, pidfd-reattached survivor
//! mid-build (ADR 0090); the fix routes the unbound arm through
//! `sandbox_owner` so ONLY a coordinator-confirmed absence reaps, and repairs
//! the binding otherwise. This oracle pins that fix: after every step, every
//! sandbox in the reconcile world's destroy log must be one the coordinator
//! genuinely no longer owns. A FAILURE means `reconcile_once` reaped a live,
//! owned VM — a real bug, not a test to update.

use crate::world::{decode_tag, SimHost, CHUNK_SIZE};

#[derive(Debug)]
pub struct Violation {
    pub invariant: &'static str,
    pub detail: String,
}

/// Check all host-internal oracles against the host's current state. Async
/// because the acked-write check reads back through the live backends
/// (in-memory tier resolution — microseconds).
pub async fn check(host: &SimHost) -> Result<(), Violation> {
    acked_writes_recoverable(host).await?;
    reconcile_none_arm_fixed(host)?;
    slot_accounting(host).await?;
    device_serving(host)
}

/// Oracle #3 — slot accounting (ADR 0098 P7, Flow B). The allocator's device
/// universe partitions cleanly at every step: `free + warm + held == capacity`,
/// with no slot handed to two owners (the reserved-bit protocol enforces the
/// latter synchronously; this identity catches a leak/double-count).
///
/// `yield_now` FIRST so any in-flight async `release` — a dropped lease's
/// `Drop`-spawned task — has drained before we read the pool counters. The sim
/// is single-threaded, so one yield runs every ready task to completion, which
/// keeps the accounting deterministic across replays.
async fn slot_accounting(host: &SimHost) -> Result<(), Violation> {
    tokio::task::yield_now().await;
    let free = host.nbd_pool.free_count().await;
    let warm = host.nbd_pool.warm_count().await;
    let held = host.leases_held();
    let capacity = host.nbd_capacity as usize;
    if free + warm + held != capacity {
        return Err(Violation {
            invariant: "slot-accounting",
            detail: format!(
                "free {free} + warm {warm} + held {held} != capacity {capacity} \
                 (a slot leaked or was double-counted)"
            ),
        });
    }
    Ok(())
}

/// Oracle #5 + the 731df805 property (ADR 0098 P7, Flow B): **single-device
/// ownership** and **no resident VM's served device is ever left dead**. These
/// are structural consistency invariants the Flow B methods maintain; the
/// 731df805 corruption broke the last one — a rung-parked survivor's live
/// served device was stale-swept/disconnected, then an un-pause landed on the
/// dead plane. A FAILURE here is that class recurring, not a test to update.
fn device_serving(host: &SimHost) -> Result<(), Violation> {
    let gen = host.generation;
    let mut seen = std::collections::BTreeSet::new();
    for (idx, s) in host.sandboxes.iter().enumerate() {
        // No two sandboxes claim the same `/dev/nbdN`.
        if !seen.insert(s.nbd_device.clone()) {
            return Err(Violation {
                invariant: "single-device-ownership",
                detail: format!(
                    "device {} is owned by more than one sandbox (idx {idx})",
                    s.nbd_device.display()
                ),
            });
        }
        // "served by THIS generation" ⟺ a lease is held (we actually serve it).
        let served = s.served_by == Some(gen);
        if served != s.lease.is_some() {
            return Err(Violation {
                invariant: "single-device-ownership",
                detail: format!(
                    "sandbox {idx}: served_by_current={served} but lease_held={} — \
                     serving state and the slot lease disagree",
                    s.lease.is_some()
                ),
            });
        }
        // A device we serve must have the kernel record US as its owner. If a
        // served device's `kernel_owner` is cleared/another gen, its plane was
        // torn down under a live serve — the 731df805 dead-plane class.
        if served && s.kernel_owner != Some(gen) {
            return Err(Violation {
                invariant: "served-device-never-dead",
                detail: format!(
                    "sandbox {idx} is served by generation {gen} but kernel_owner={:?} — \
                     a live served device was disconnected (the 731df805 dead-plane class)",
                    s.kernel_owner
                ),
            });
        }
    }
    Ok(())
}

/// Oracle #9: no sandbox reconcile ever destroyed is still coord-owned. Reads
/// the honest ownership model (never consuming a scripted override).
fn reconcile_none_arm_fixed(host: &SimHost) -> Result<(), Violation> {
    for record in host.reconcile.destroyed() {
        if let Some(owner) = host.coord.honest_owner(record.sandbox_id) {
            return Err(Violation {
                invariant: "reconcile-none-arm-fixed",
                detail: format!(
                    "reconcile destroyed sandbox {} but the coordinator still owns it \
                     (session {owner}); had_binding={:?} — the 2026-07-11 mis-reap",
                    record.sandbox_id, record.had_binding
                ),
            });
        }
    }
    Ok(())
}

async fn acked_writes_recoverable(host: &SimHost) -> Result<(), Violation> {
    // Snapshot the latest-acked set under a deterministic BTreeMap order, then
    // read each chunk back through its live backend and check it against BOTH
    // the durable-handoff floor and the latest ack (the honest, bidirectional
    // form — ADR 0098 P4.5).
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
        let latest = entry.content_tag;
        // The published-tier floor: `0` (base) if this chunk was never flushed
        // to the durable tier — its acked writes are all RAM-only or
        // spool-transient, droppable by abrupt death.
        let floor = host.ledger.handed_off_tag(idx, chunk_idx).unwrap_or(0);
        // Honest range (ADR 0098 P4.5): a legitimate read is anywhere in
        // `[published_floor, latest_ack]` by tag order — the live newest write
        // (== latest), the permanent published floor (a rebuild that dropped
        // newer un-published writes — an accepted, bounded loss), or a
        // transiently-durable intermediate a standing spool adopted. Only two
        // things are violations:
        //   * a read OLDER than the published floor — a DURABLE published write
        //     rolled back (the 85e0298a corruption class); or
        //   * a read NEWER than the latest ack — a never-acked future tag.
        if floor <= got && got <= latest {
            continue;
        }
        let why = if got < floor {
            "a durable published write rolled back below the floor"
        } else {
            "a read newer than the latest ack (a never-acked tag)"
        };
        return Err(Violation {
            invariant: "acked-write-durability",
            detail: format!(
                "sandbox {idx} chunk {chunk_idx}: read {got} outside [published_floor {floor}, \
                 latest_ack {latest}] — {why}; lineage-at-ack {}v{}",
                entry.lineage_at_ack.manifest_id, entry.lineage_at_ack.version
            ),
        });
    }
    Ok(())
}
