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
//! decoded tag to be a MEMBER of that chunk's acked-tag set (or the tag-0
//! base), bounded below by `published_floor`. Legitimate members include:
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
//! Three reads are violations: OLDER than the published floor (a durable
//! published write rolled back — the 85e0298a corruption class), NEWER than
//! the latest ack (a never-acked tag), or in-range but NEVER ACKED for this
//! chunk (a misdirected read serving another chunk's write). The published floor is the ONLY
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

use engram_host_agent::durable_record::record_path;
use engram_host_agent::eviction_finalize::EvictionFinalizeRecord;
use engram_host_core::{FinalizeStage, TokioFs};

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
    device_serving(host)?;
    quarantine_reconnectable(host)?;
    finalize_stage_monotone(host).await?;
    migration_decision_table(host)?;
    no_plane_leak(host)
}

/// Wave 7b (ADR 0098 §Phase 3, #784 layers 2–3): the classification barrier's
/// promise. A device the barrier QUARANTINED — a record-invisible live survivor
/// (#769 gap A), recorded in
/// [`quarantined_unknown`](crate::SimHost::quarantined_unknown) — must stay
/// RECOVERABLE: it is either re-served by this generation OR left kernel-bound
/// (RECONNECTABLE) while its guest still holds it. It is NEVER left both
/// unserved AND kernel-unbound with a live holder — quarantine PARKS + ALERTS,
/// it never skips-and-severs. (The broader "no live holder is ever severed"
/// invariant is `device_serving`'s `severed-live-holder` arm; this pins that the
/// specific quarantine bookkeeping stays consistent as the barrier evolves.)
fn quarantine_reconnectable(host: &SimHost) -> Result<(), Violation> {
    let gen = host.generation;
    for (idx, slot) in host.sandboxes.iter().enumerate() {
        if !host.quarantined_unknown.contains(&slot.sandbox_id) {
            continue;
        }
        let served = slot.served_by == Some(gen);
        if !served && slot.guest_holds_device && slot.kernel_owner.is_none() {
            return Err(Violation {
                invariant: "quarantine-reconnectable",
                detail: format!(
                    "sandbox {idx} was classified QuarantinedUnknown but is now unserved, \
                     kernel-unbound, and still guest-held — a quarantined survivor was \
                     severed instead of left reconnectable (#769 gap A)"
                ),
            });
        }
    }
    Ok(())
}

/// Oracle #7 — the migration decision table (ADR 0098 P8, issue #216):
/// **`state_served` ⇒ the dumb-host TTL sweep never abort-unpauses.**
/// Once `state.bin` has shipped, the dest may be running this state; a
/// TTL-driven self-resume of the frozen source is the split-brain. The
/// SWEEP records any abort-unpause it applies to a served export, so this
/// fires exactly when `ttl_verdict`'s `state_served` arm regresses. An
/// EXPLICIT coordinator abort is deliberately exempt: the prod RPC allows
/// it even post-ship (the coordinator carries the postcopy-never-loaded
/// knowledge — ADR 0045 C2), and the P9 lane's first CI run caught the
/// sim over-claiming there (calm seeds 14/16).
fn migration_decision_table(host: &SimHost) -> Result<(), Violation> {
    if let Some(id) = host.split_brain_unpauses.first() {
        return Err(Violation {
            invariant: "migration-decision-table",
            detail: format!(
                "sandbox {id}: an abort-unpause was applied to an export whose                  state.bin had shipped — the #216 split-brain ttl_verdict forbids"
            ),
        });
    }
    Ok(())
}

/// Oracle #2 — no-plane-leak (ADR 0098 P8 tightening): the frozen/pending
/// lifecycle states stay bidirectionally accounted. A `migrating` slot
/// must have an open export (a frozen guest with nothing to ever end it
/// is a leaked plane) and a live backend (the source IS a page server);
/// an open export must belong to a `migrating` slot. A pending finalize
/// must have its job state (in RAM or re-drivable on disk — checked via
/// the idempotency map the resume leg rebuilds).
fn no_plane_leak(host: &SimHost) -> Result<(), Violation> {
    for (idx, slot) in host.sandboxes.iter().enumerate() {
        let has_export = host.migrations.export_id_of(slot.sandbox_id).is_some();
        if slot.migrating != has_export {
            return Err(Violation {
                invariant: "no-plane-leak",
                detail: format!(
                    "sandbox {idx}: migrating={} but open-export={} — a frozen                      guest with no export to end it (or an export on an                      un-frozen guest)",
                    slot.migrating, has_export
                ),
            });
        }
        if slot.migrating && slot.backend.is_none() {
            return Err(Violation {
                invariant: "no-plane-leak",
                detail: format!(
                    "sandbox {idx}: migrating without a live backend — the                      frozen source must keep serving its tiers"
                ),
            });
        }
    }
    Ok(())
}

/// Oracle #6 — `FinalizeStage` monotonicity + resume-at-persisted-stage
/// (ADR 0098 P5, Flow D). Reads the ON-DISK finalize records back through
/// the real torn-tolerant `load_all` after every step and asserts:
///
/// * **Monotone**: a record's persisted stage never regresses below the
///   highest stage ever observed for that snapshot (the watermark survives
///   crashes — it is oracle memory). A regression means a redrive re-ran a
///   leg the record said was durable, or a crash rolled the record back
///   past its stage-bump persist.
/// * **Stage ⇒ fields**: every sim capture stages a disk-pending set, so a
///   record at `DiskUploaded` or beyond MUST carry `disk_manifest` — a
///   stage claiming the disk leg is durable without its output is exactly
///   the #743 disk_manifest=None shape.
///
/// (The memory leg's field implication is vacuous here — the sim stages no
/// `memory.bin`, so `memory_manifest` is legitimately `None` at every
/// stage.) Terminal completion is oracle #8's side: the record is DELETED,
/// which the watermark deliberately does not treat as a regression.
async fn finalize_stage_monotone(host: &SimHost) -> Result<(), Violation> {
    let finalize_dir = host.fs.root().join("finalize");
    let records: Vec<EvictionFinalizeRecord> =
        EvictionFinalizeRecord::load_all(&TokioFs, &finalize_dir).await;
    let mut seen = host.finalize_stage_seen.lock();
    for record in records {
        let watermark = seen
            .get(&record.snapshot_id)
            .copied()
            .unwrap_or(FinalizeStage::Captured);
        if record.stage < watermark {
            return Err(Violation {
                invariant: "finalize-stage-monotone",
                detail: format!(
                    "snapshot {}: persisted stage {:?} regressed below the observed                      watermark {:?} — a durable leg was un-done",
                    record.snapshot_id, record.stage, watermark
                ),
            });
        }
        seen.insert(record.snapshot_id, record.stage);
        if record.stage >= FinalizeStage::DiskUploaded && record.disk_manifest.is_none() {
            return Err(Violation {
                invariant: "finalize-stage-monotone",
                detail: format!(
                    "snapshot {}: stage {:?} claims the disk leg is durable but                      disk_manifest is None (the #743 silent-skip shape)",
                    record.snapshot_id, record.stage
                ),
            });
        }
    }
    Ok(())
}

/// Oracle #8 — convergence at quiescence (ADR 0098 P5, Flow D's redrive
/// loop). Called by the scheduler AFTER the quiesce heal + a bounded
/// finalize drain: every finalize ever STARTED must have reached a terminal
/// outcome — completed (the terminal `EvictionFinal` checkpoint record
/// exists in `records/` and the finalize record is gone) or quarantined
/// (`finalize/failed/<id>.json` exists). A finalize still pending past the
/// drain budget is non-convergence — the "retries forever" class (#743's
/// checkpoint-driver shape). `plan_finalize_retry`'s attempts cap is what
/// guarantees termination under persistent fault; this oracle is what
/// notices if that guarantee breaks.
pub fn check_finalize_convergence(host: &SimHost) -> Result<(), Violation> {
    if !host.pending_finalizes.is_empty() {
        let stuck: Vec<String> = host
            .pending_finalizes
            .iter()
            .map(|e| format!("{}→{}", e.key(), e.value()))
            .collect();
        return Err(Violation {
            invariant: "finalize-convergence",
            detail: format!(
                "pending finalizes past the quiescence drain budget: {stuck:?} —                  a redrive loop that never terminates"
            ),
        });
    }
    let records_dir = host.fs.root().join("records");
    let failed_dir = host.fs.root().join("finalize").join("failed");
    for snapshot_id in &host.finalize_started {
        let completed = record_path(&records_dir, snapshot_id).exists();
        let quarantined = record_path(&failed_dir, snapshot_id).exists();
        if !completed && !quarantined {
            return Err(Violation {
                invariant: "finalize-convergence",
                detail: format!(
                    "started finalize {snapshot_id} reached neither the terminal                      EvictionFinal record nor quarantine — it was silently dropped"
                ),
            });
        }
    }
    Ok(())
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
        // R6 (#769 gap A): a device whose FC guest still holds it open must
        // never be fully severed — it is either served by us OR still
        // kernel-bound (RECONNECTABLE for a re-serve pass). Both unserved AND
        // kernel-unbound with a live holder is the stale sweep having
        // DISCONNECTed a live guest's rootfs (the exact 2026-07-18/19 firing
        // this layer prevents). Fails against the pre-R6 `sweep_verdict`
        // (dead-owner ⇒ Disconnect regardless of holder); passes with the Park
        // guard.
        if s.guest_holds_device && !served && s.kernel_owner.is_none() {
            return Err(Violation {
                invariant: "severed-live-holder",
                detail: format!(
                    "sandbox {idx}: a live guest still holds device {} open, but it is \
                     unserved AND kernel-unbound — the stale sweep severed a surviving \
                     guest's rootfs (#769 gap A)",
                    s.nbd_device.display()
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
        let acked = host.ledger.acked_tags(idx, chunk_idx);
        // A legitimate read is a tag actually acked for THIS chunk (or the tag-0
        // base), bounded below by the published floor. Membership implies
        // `got <= latest` (tags are globally monotone, so the chunk's latest ack
        // is its max member) — the interval alone was NOT sufficient: tags are
        // global, so another chunk's in-range tag must be a violation
        // (misdirection), not a pass.
        let member = got == 0 || acked.contains(&got);
        if member && got >= floor {
            continue;
        }
        let why = if got < floor {
            "a durable published write rolled back below the floor"
        } else if got > latest {
            "a read newer than the latest ack (a never-acked tag)"
        } else {
            "an in-range tag never acked for THIS chunk (a misdirected read \
             serving another chunk's write)"
        };
        return Err(Violation {
            invariant: "acked-write-durability",
            detail: format!(
                "sandbox {idx} chunk {chunk_idx}: read {got} not a member of this chunk's acked \
                 set within [published_floor {floor}, latest_ack {latest}] — {why}; \
                 lineage-at-ack {}v{}",
                entry.lineage_at_ack.manifest_id, entry.lineage_at_ack.version
            ),
        });
    }
    Ok(())
}

/// Quiescence-only tightening of oracle #1 (ADR 0098 Phase 3, R1.5): after
/// the quiesce pass has driven a final REAL flush through every live
/// backend, **no surviving chunk's content may sit above the published
/// floor** — read back through the live backend, every acked-written chunk
/// must decode at-or-below the floor (and with oracle #1's lower bound,
/// exactly AT it: everything that survived is published). This turns
/// "bounded loss" from an unenforced flush-cadence claim into a checked
/// guarantee: loss is bounded by "un-flushed at crash", never "we forgot
/// to ever flush". A write lost to an earlier abrupt crash is already gone
/// from the backend (its `latest_ack` legitimately exceeds the floor
/// forever — the accepted crash-window loss), so it does NOT fire; what
/// fires is a write that SURVIVED to quiescence and the final flush still
/// failed to publish — a flush-pipeline liveness hole. Sandboxes with no
/// live backend (destroyed at a finalize/migration terminal, or
/// quarantined) are exempt — their durability story is oracle #6/#8's.
pub async fn check_quiescent_floor(host: &SimHost) -> Result<(), Violation> {
    for ((idx, chunk_idx), _entry) in host.ledger.latest_by_chunk() {
        let Some(slot) = host.sandboxes.get(idx) else {
            continue;
        };
        let Some(backend) = slot.backend.clone() else {
            continue;
        };
        let bytes = backend
            .read(chunk_idx * CHUNK_SIZE, CHUNK_SIZE)
            .await
            .map_err(|e| Violation {
                invariant: "quiescent-floor",
                detail: format!("sandbox {idx} chunk {chunk_idx} read failed: {e}"),
            })?;
        let got = decode_tag(&bytes);
        let floor = host.ledger.handed_off_tag(idx, chunk_idx).unwrap_or(0);
        if got > floor {
            return Err(Violation {
                invariant: "quiescent-floor",
                detail: format!(
                    "sandbox {idx} chunk {chunk_idx}: live content {got} above the published \
                     floor {floor} after the quiescence flush — a surviving acked write the \
                     world never flushed (the loss bound is 'un-flushed at crash', not 'never \
                     flushed')"
                ),
            });
        }
    }
    Ok(())
}
