# ADR 0110: Honest flush — acked disk writes survive process death

Status: Proposed (2026-08-02)

## Summary, in plain English

Every disk makes one promise. When a program says "flush," the disk must
not answer "done" until the data is truly safe. Every filesystem is built
on this promise. If the power dies, the filesystem loses only the data it
never flushed. It recovers on its own. This is normal and safe.

Our disk server breaks this promise. When a guest VM says "flush," the
server answers "done" and does nothing. The data stays in the server's
process memory. We redeploy that process about 11 times a day. Each
deploy can destroy data the guest was told was safe.

We built a large recovery apparatus to manage the damage: quarantine,
retry ladders, a shutdown spool, a destroy-and-rewind arm, and a red
"guest disk rolled back" card in the UI. All of it compensates for one
dishonest ACK.

The fix: keep the promise. On flush, write the dirty data to plain files
on the node's local SSD, then answer "done." Files belong to the kernel,
not to the process. A killed process loses nothing it wrote to a file.
After this change, a host-agent death looks to the guest like a power
cut on an honest disk — the exact event its filesystem is designed to
survive. The recovery apparatus becomes deletable.

## Context

### The defect

`crates/engram-host-agent/src/disk_daemon/runtime.rs:1509`:

```rust
// FLUSH: ack at the wire level but defer durable-flush to the
// snapshot path (inline per-FLUSH flush-to-chunk-store would
// multiply object-storage cost ~100×; we trade strict-FUA for cost).
NbdCommand::Flush | NbdCommand::Trim => (NbdReply::ok(req.handle), None),
```

The comment prices "honest flush" against the chunk store (GCS). That is
a false choice. The third option — node-local NVMe — costs microseconds
and was never considered. Acked writes live only in
`ChunkedDiskBackend.dirty` (`backend.rs:346`), a RAM map inside the
host-agent process, until the 30s flush scheduler uploads them.

### The cost of the lie

A host-agent pod roll kills the process. The kernel NBD device survives
(ADR 0017's netlink work; guest I/O parks for 300s). The VM survives
(ADR 0044 K2 VM-detach). Only the RAM dirty tier dies. Everything
downstream exists to cope with that:

- 2026-08-02: 12 `durability_rollback` events in one day, in bursts of
  up to 5 sessions in 12 seconds, one burst per host-agent roll. 14
  events in 7 days; 2 sessions lost real user work (one lost 47 events).
- Since 2026-05-20: ~106 commits (~10% of all repo activity) patched
  this class. The quarantine ladder alone was rewritten four times.
- 13 ADRs are load-bearing for it (0015-0018, 0028, 0044, 0050, 0069,
  0090, 0091, 0098, 0099, 0101). ADR 0090 carries three addenda.
- ~2,600 lines of production scaffolding and ~3,750 lines of dedicated
  recovery tests exist to manage it.
- The latest round (#971 unwind-safe SIGTERM, #972 park-and-recover)
  landed 2026-08-02 — stopgaps that stop the destroy, not the loss.

### Why the simpler-sounding fixes were rejected

- **Drain rolls (roll = evict everything):** makes every deploy slow and
  interrupts active sessions. Rejected for the tradeoff.
- **Out-of-process disk daemon (ADR 0076):** keeps the state alive by
  keeping the process alive. Adds a supervised process per sandbox and
  still loses everything on a daemon crash. Gated, now mooted.
- **Sync flush to GCS:** ~100ms per guest fsync. Correctly rejected in
  the original comment.

## Decision

**Rule: never acknowledge durability you do not have.** Concretely: an
acked FLUSH means the covered writes are on node-local disk, outside
the process, before the reply goes out.

**Scope of protection.** This defends against *process* death only —
deploys, panics, SIGKILL, OOM. *Node* death remains covered by the
existing 30s flush-to-GCS floor, unchanged. This split is what keeps
the code small: all the hard parts of journal engineering (fsync
ordering, torn-write recovery, barriers) exist to survive power loss
without a backup. We have a backup. We only need the kernel's own
guarantee: file data survives the death of the process that wrote it.

## Design

### The journal: a folder of chunk files

Per sandbox, on the same hostPath volume the spool and chunk cache use:

```
/var/lib/engram/journal/<sandbox_id>/
    <chunk_index>            # complete chunk bytes, current version
    .tmp-<chunk_index>-<n>   # in-progress write, ignored by recovery
```

No record framing. No custom format. One file per dirty chunk, always
written as tmp-then-`rename(2)`. Rename is atomic: a reader sees the
complete old version or the complete new version, never a torn file.
Torn-write handling is inherited from the kernel, not written by us.

### Write path

1. `NBD_CMD_WRITE`: unchanged. Buffer into the RAM dirty map, ack.
2. `NBD_CMD_FLUSH`: for each chunk dirtied since the last flush, write
   its full bytes to a tmp file and rename into place. Then ack.
   No fsync — the data is in the kernel page cache, which survives
   process death. Writes land at memory speed.
3. FUA-flagged writes: journal that chunk, then ack (same mechanism).
4. `ENGRAM_JOURNAL_FSYNC=1` (default off): also fsync file + directory,
   for kernel-panic paranoia. Not required for correctness; kernel
   panic falls back to the GCS floor exactly like node death.

Each dirty chunk carries a generation counter (bumped on every write to
it). The journal file records the generation it captured (xattr or a
sidecar naming scheme — implementation's choice, it only gates cleanup).

### Cleanup (compaction)

After the flush scheduler drains chunk `i` at generation `g` to the
chunk store and the manifest publish succeeds, delete `journal/<sid>/i`
only if its captured generation is ≤ `g`. A chunk re-dirtied after the
drain keeps its newer journal file. The journal is therefore bounded by
the un-uploaded window — the same ~256 MiB dirty threshold that already
paces the flush scheduler.

### Recovery path (replaces rehydrate forensics)

Successor host-agent, per surviving sandbox:

1. List `journal/<sandbox_id>/`. Load every chunk file into the dirty
   tier, on top of the backend built from the last published manifest.
   Ignore `.tmp-*` files (delete them).
2. `NBD_CMD_RECONFIGURE` the surviving `/dev/nbdN` with a fresh socket.
3. Done. No spool adoption, no `VerifySeed` probe, no quarantine.

If step 2 fails (netlink error, identifier mismatch): flush the loaded
dirty tier to the chunk store, publish the manifest, destroy the VM,
and let the next prompt resume from that manifest. **Zero acked loss.**
Reattach failure degrades to a normal park/resume instead of a rollback.

### Finalize without a data plane

Because the journal + published base fully determine the disk, eviction
finalize can build a capture manifest from host files alone — no live
NBD device needed. This is what deletes `RefuseUntracked`: the refusal
protected acked writes from a snapshot that would drop them; there is
no longer anything to drop.

### Guest contract

Data the guest wrote but never flushed can still be lost on process
death. That is the standard power-cut contract every journaling
filesystem (ext4 in our guests) is designed for. We stop opting out of
the guest's own safety scheme; we do not add a new one.

## What this deletes

Removed outright (≈1,800+ production LOC):

| Code | Location |
|---|---|
| Quarantine evict ladder: `QUARANTINE_EVICT_MAX_ATTEMPTS`, `reap_quarantined_survivor`, the park-and-recover arm (#972), `quarantine_reap_unevictable` | `session_verbs.rs`, `idle_evictor.rs` |
| `quarantined_survivors` map, heartbeat advertise, coordinator consumption | `pooled_backend.rs`, `heartbeat.rs`, `host_http.rs`, `engram-protocol` |
| `RefuseUntracked` / `CaptureDrainPlan` | `engram-host-core/src/survivor.rs` (whole file) |
| SIGTERM spool: dump-at-shutdown, atomic adoption, the final-flush ladder (#971's hardening included) | `disk_daemon/spool.rs`, `runtime.rs` adoption arms |
| `VerifySeed` probe-read machinery | `runtime.rs` reattach path |
| NBD slot `quarantine()` state and its FSM arms | `disk_daemon/slot.rs` |
| `SessionEvent::DurabilityRollback` emission on the process-death class; the event, metric, and web card remain only for genuine node loss | `state.rs`, `session_verbs.rs`, `idle_evictor.rs`, `SystemMessage.tsx`, `buildMessages.ts` |

Replaced: ~3,750 lines of NBD-recovery integration tests
(`nbd_startup_recovery`, `nbd_shutdown_abandon_race`,
`nbd_shutdown_final_flush`, `chain_rehydrate`, …) collapse into a
journal suite (see Testing). Simplified: `rehydrate_sandbox` /
`reattach_manifest` shrink to load-folder → RECONFIGURE → lossless
fallback.

Unchanged: NBD netlink + dead-conn parking (ADR 0017), VM-detach and
pidfd reattach (ADR 0044 K2), the flush scheduler and manifest publish
(now the node-death floor only), the coordinator session FSM,
`dead_host` for genuine node loss.

## Tradeoffs

1. **Double-write.** Every dirty byte hits host NVMe twice (journal,
   then chunk upload staging). Bounded by the dirty window; real
   bandwidth and SSD wear, accepted.
2. **Flush latency.** Guest fsync now pays a memcpy-speed journal write
   for the chunks it dirtied. Typically sub-millisecond; a large burst
   costs a few ms. Today's flush is instant because it is fake.
3. **Kernel panic** (OS dies, disk intact) loses un-fsynced page cache
   and falls back to the GCS floor — same as node death. Rare; the
   `ENGRAM_JOURNAL_FSYNC` flag exists if we ever care.
4. **Node death is unchanged.** Up to ~30s of writes lost, as today.
   The rollback card keeps that case — where "host failure" is true.
5. **New load-bearing file code.** Small and single-process, but it
   must be tested with the full ADR 0099 H5 crash-state discipline.

## Testing

- **Crash-state (ADR 0099 H5):** externally construct every post-kill
  journal state — tmp files at every stage, renamed-but-stale
  generations, garbage siblings — and assert recovery loads exactly the
  complete chunk set. No fail-points needed; the states are plain files.
- **Property test:** random write/flush/kill schedules against a model
  disk; after recovery, every acked-flushed byte reads back. Wildcard-
  free match over `NbdCommand` so a new command is a compile error.
- **DST/cosim:** the existing quarantine flows (ADR 0098 Flow A/B)
  invert — assert a roll produces zero `durability_rollback` events and
  zero rewound event indexes.
- **FC integration (CI-wired per AGENTS.md):** minimal write → flush →
  SIGKILL host-agent → successor recovers → read back. Smallest data
  that proves the property; no throughput measurement.

## Rollout

1. Land the journal write path + recovery behind the existing reattach
   flow (scaffolding still present, now unexercised).
2. Prove zero-rollback rolls in prod for one week of normal deploy
   cadence (`engram_durability_rollback_total` flat).
3. Delete the scaffolding (the table above) in one retirement PR chain.
4. Flip this ADR to Accepted with the commit list; add the terminal
   addendum to ADR 0090 (quarantine concept retired) and mark ADR 0076
   Superseded by this document.

## Relationship to other ADRs

- **ADR 0007** (chunked-immutable storage): untouched; the journal is a
  staging tier under it, not a replacement.
- **ADR 0017** (NBD lifecycle): its netlink survivability is what makes
  recovery a RECONFIGURE instead of a device rebuild. Kept.
- **ADR 0028 / 0101** (eviction durability): the durable floor they
  define becomes the *node-death* floor only.
- **ADR 0044 K2** (VM-detach): kept — rolls stay instant; this ADR
  makes the surviving VM's disk state actually safe.
- **ADR 0076** (engram-substrated): superseded — it kept state alive by
  keeping a process alive; this keeps state alive by putting it in
  files.
- **ADR 0090** (sandbox ownership): quarantine, its ladder, and the
  destroy arm are retired. Ownership-is-coordinator-truth stands.
