# ADR 0110: Honest writes — acked disk writes survive process death

Status: Proposed (2026-08-02)

## Summary, in plain English

Every disk makes one promise. When a program asks it to make data safe,
the disk must not answer "done" until the data is truly safe. Every
filesystem is built on this promise. If the power dies, the filesystem
loses only data it was never promised. It recovers on its own. This is
normal and safe.

Our disk server breaks this promise. It acknowledges guest writes into
its own process memory, and answers guest flushes with "done" while
doing nothing. We redeploy that process about 11 times a day. Each
deploy can destroy data the guest was told was safe.

We built a large recovery apparatus to manage the damage: quarantine,
retry ladders, a shutdown spool, a destroy-and-rewind arm, and a red
"guest disk rolled back" card in the UI. All of it compensates for
acked data living in process memory.

The fix: the dirty tier stops being process memory. It becomes a plain
sparse file on the node's local SSD, written through on every guest
write. Files belong to the kernel, not to the process. A killed process
loses nothing it wrote to a file. After this change, a host-agent death
looks to the guest like a power cut on an honest disk — the exact event
its filesystem is designed to survive. The recovery apparatus becomes
deletable.

## Context

### The defect

Two halves of one problem:

1. Acked writes live only in process RAM.
   `ChunkedDiskBackend.dirty` (`backend.rs:346`) is
   `Arc<Mutex<HashMap<usize, Vec<u8>>>>` — the only copy of every acked
   guest write between the 30s flush-scheduler uploads.
2. FLUSH is a lie. `runtime.rs:1509`:

```rust
// FLUSH: ack at the wire level but defer durable-flush to the
// snapshot path (inline per-FLUSH flush-to-chunk-store would
// multiply object-storage cost ~100×; we trade strict-FUA for cost).
NbdCommand::Flush | NbdCommand::Trim => (NbdReply::ok(req.handle), None),
```

The comment prices honesty against the chunk store (GCS). That is a
false choice. The third option — node-local disk — costs microseconds
and was never considered.

### The cost

A host-agent pod roll kills the process. The kernel NBD device survives
(ADR 0017 netlink; guest I/O parks 300s). The VM survives (ADR 0044 K2
VM-detach). Only the RAM dirty tier dies. Everything downstream copes
with that:

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

### Prior art

e2b (e2b-dev/infra) runs the same architecture — an in-process NBD
server inside the node orchestrator, chunked lazy base images from
GCS — and does not have this bug class. Their write cache
(`block/cache.go`) is a file-backed `MAP_SHARED` mmap from block zero:
every guest write lands in the kernel page cache at ACK time. They do
not even negotiate `NBD_FLAG_SEND_FLUSH`, because a write-through cache
has nothing left for FLUSH to add. This ADR adopts that primitive and
keeps what e2b lacks: our VMs survive host-agent restarts (e2b SIGKILLs
every orphaned Firecracker on startup — acceptable for disposable
sandboxes, wrong for long-lived sessions).

### Rejected alternatives

- **Drain rolls (roll = evict everything):** slow deploys, interrupts
  active sessions.
- **Out-of-process disk daemon (ADR 0076):** keeps state alive by
  keeping a process alive; still loses everything on a daemon crash.
  Gated, now superseded.
- **Sync flush to GCS:** ~100ms per guest fsync. Correctly rejected in
  the original comment.
- **Flush-time journal (this ADR's first draft):** tmp-then-rename
  chunk files written per guest FLUSH. Correct, but protects only
  flushed writes and needs rename discipline, generation counters, and
  compaction. Write-through makes all of that unnecessary.

## Decision

**Rule: an acked write is on a node-local file before the ack.** The
dirty tier is a per-sandbox sparse file, written through with
`pwrite(2)` on every NBD write. No acked byte ever exists only in
process memory.

**Scope of protection.** This defends against *process* death —
deploys, panics, SIGKILL, OOM. Data written to a file lives in the
kernel page cache the moment `pwrite` returns; the death of the writing
process cannot touch it. *Node* death remains covered by the existing
30s flush-to-GCS floor, unchanged. This split keeps the code small:
fsync ordering, torn-write recovery, and write barriers exist to
survive power loss without a backup. We have a backup (the chunk
store). We only defend against the event that actually recurs — the
process dying — and the kernel gives that defense away free.

## Design

### The dirty file

Per sandbox, on the same hostPath volume as the chunk cache:

```
/var/lib/engram/dirty/<sandbox_id>.cache   # sparse, sized to the device
```

Created (sparse, `ftruncate` to device size) at sandbox start. No
format: byte offset N of the file is byte offset N of the device.

### Data path

1. `NBD_CMD_WRITE`: `pwrite` the bytes into the dirty file at the
   request offset; set the covered chunks in the in-memory dirty
   bitmap; ack. The RAM `HashMap` dirty tier is deleted. Page-cache
   writes are memory-speed; ack latency is unchanged.
2. `NBD_CMD_READ`: bitmap says dirty → `pread` the dirty file
   (page cache, RAM-speed). Otherwise the base/chunk-store path,
   unchanged.
3. `NBD_CMD_FLUSH` / FUA: ack, unchanged — but now honest. There is no
   process-volatile state for a flush to make safe; within our threat
   model (process death), acked writes are already durable. (This is
   e2b's reasoning for not negotiating FLUSH at all.)
4. No fsync. `ENGRAM_DIRTY_FSYNC=1` (default off) fsyncs on FLUSH for
   kernel-panic paranoia; a kernel panic otherwise falls back to the
   GCS floor exactly like node death.
5. I/O errors surface as `EIO` results from `pwrite`/`pread` and turn
   into per-request NBD error replies — one sandbox degrades, the
   host-agent lives. (We deliberately use file syscalls, not mmap:
   e2b's mmap approach needs a SIGBUS-recovery guard, `RunFaultSafe`;
   `pwrite` gives the same page-cache semantics with ordinary error
   handling.)

### Flush scheduler (unchanged cadence, new source)

The 30s / 256 MiB flush scheduler `pread`s dirty chunks from the file,
uploads to the chunk store, and publishes the manifest — as today.
After a durable publish it may `fallocate(PUNCH_HOLE)` ranges not
re-dirtied since the drain, to bound file size. The re-dirtied check is
an in-memory flag; losing it to a crash means re-uploading a chunk to a
content-addressed store — harmless and idempotent. No generation
counters, no compaction machinery.

**One strict rule protects the file: never delete or hole-punch dirty
data because a publish *said* it worked.** Before punching, fetch the
published manifest back and check it really contains the covered
chunks — the same verify-the-occupant discipline PR #897 established.
A publish-layer bug then costs a redundant re-upload, never the data.
(See incident E below for why this rule exists.)

### Recovery (replaces rehydrate forensics)

Successor host-agent, per surviving sandbox:

1. Open the dirty file. Rebuild the dirty bitmap with
   `lseek(SEEK_DATA/SEEK_HOLE)` — allocated extents are the dirty set.
   (Any extent overlapping a chunk marks the whole chunk dirty; the
   over-approximation only costs a redundant idempotent upload.)
2. `NBD_CMD_RECONFIGURE` the surviving `/dev/nbdN` with a fresh socket.
3. Done. No spool adoption, no `VerifySeed` probe, no quarantine.

If step 2 fails (netlink error, identifier mismatch): flush the dirty
file to the chunk store, publish the manifest, destroy the VM, and let
the next prompt resume from that manifest. **Zero acked loss.**
Reattach failure degrades to a normal park/resume instead of a
rollback.

### Finalize without a data plane

The published base + the dirty file fully determine the disk, so
eviction finalize can build a capture manifest from host files alone —
no live NBD device needed. This is what deletes `RefuseUntracked`: the
refusal protected acked writes from a snapshot that would drop them;
there is no longer anything to drop.

### Guest contract

Unchanged for the guest: it already assumes a disk that may lose
nothing it acked. We now actually are one, across process death. Node
death keeps power-cut semantics backed by the GCS floor, as today.

## Would this have prevented the real incidents?

We replayed the four most recent fixes in this area against this
design, step by step. Four are prevented. One (E) is only partly
covered — it is flagged, not hidden, and the rule above closes it.

### A. The 2026-08-02 roll cascade (this ADR's trigger)

Today:

1. A deploy replaces the host-agent pod.
2. The old process dies. The RAM dirty tier dies with it.
3. The new process cannot reattach some disks. It quarantines them.
4. The coordinator tries to rescue each session 3 times. Every try
   fails the same way, because quarantine is exactly the state the
   rescue cannot handle.
5. It gives up and destroys the VM. The next resume rewinds the disk.
   12 sessions in one day; 2 lost real user work.

With this ADR:

1. A deploy replaces the host-agent pod.
2. The old process dies. Every acked write is already in the dirty
   file. Nothing of value dies with the process.
3. The new process opens the file, rebuilds its map from the file's
   extents, and reconnects the device.
4. There is nothing to rescue. No quarantine, no destroy, no rewind,
   no red card.

**Verdict: prevented.**

### B. PR #971 — a panic in the SIGTERM shutdown ladder

The bug (now patched): shutdown ran a careful ladder — flush, then
dump leftover RAM writes to the spool file, then detach. A panic in
the middle skipped the dump AND disconnected every disk device on the
way down. The RAM-only writes were gone, and the successor could not
even reconfigure the devices.

With this ADR: there is no shutdown ladder for disk data. There is
nothing the dying process must do to keep writes safe — they are
already in the file. A panic at shutdown has nothing left to break.
Even a wrongly disconnected device only costs the reattach, and a
failed reattach rebuilds from the file with zero loss.

**Verdict: prevented — the failing code is deleted, not fixed.**

### C. PR #972 — destroy after three failed rescues

The bug (now patched): the rescue retry budget was built for flaky
failures, but quarantine fails the same way every time. After 3 tries
the exhaustion arm destroyed the VM — the only copy of the acked
writes.

With this ADR: the ladder, the budget, and the destroy arm are all in
the deletion list. A stuck reattach has no clock on it — the data is
safe on disk, so the system can retry calmly forever, or rebuild the
session from the file with nothing lost.

**Verdict: prevented — this PR's machinery is literally what gets
deleted.**

### D. PR #843 — session 61a03b7e, 93 events rewound

The bugs (now patched):

1. A session was parked. A pod roll landed 50 seconds later.
2. A hand-written SQL list had never learned the new `parked` state,
   so the successor was never told to reattach that session's disk.
3. The host's local fallback then mixed up two manifest kinds (memory
   vs disk), failed the reattach, and threw away the spool file — the
   only durable copy of the disk writes — as "foreign lineage."
4. Quarantine → destroy → the resume rewound 7 minutes and 93 events.

With this ADR:

1. Same park, same roll.
2. The coordinator's list no longer matters for safety. Recovery finds
   the dirty file by sandbox id on local disk. There is no manifest
   lookup to get wrong and no lineage gate to wrongly refuse — those
   concepts are gone from the recovery path.
3. Reattach succeeds. Even if it did not, the file rebuilds the disk
   with zero loss.

**Verdict: prevented — one bug becomes harmless, the other becomes
impossible.** Honest caveat: this ADR protects the *disk*. A parked
VM whose *memory* has not yet settled to the durable floor (ADR 0101)
can still lose conversation state if something destroys it. Under
this design nothing needs to destroy it — but the memory floor itself
is ADR 0101's domain, unchanged here.

### E. PR #897 — a stale manifest published as the floor. NOT fully fixed — flagged.

The bug (now patched): a flush crashed halfway — after durably writing
its manifest to the store, before updating its own memory of it.
Later, finalize hit a "someone already published my version" conflict
and assumed, without looking, that the occupant was its own earlier
work. It pinned the stale manifest as the published floor. The newest
write's only copy sat in the spool file, and the version gate refused
it as "old." (Caught by the nightly simulator — ten failing seeds —
never confirmed in prod.)

Why this ADR alone does not fix it: the manifest/publish layer stays
(it is our node-death floor), so a bug there can still pin a wrong
floor. What changes: the spool and its version gate are gone, and the
newest write lives in the dirty file instead. The loss could only
recur if cleanup deleted the dirty file while trusting a lying publish
result. The rule in the Design section exists precisely to close this:
verify the published manifest actually contains the data before
punching it out of the file. With that rule, this class degrades from
"data loss" to "redundant re-upload."

**Verdict: the #897 fix (verify the occupant) stays load-bearing and
is NOT in the deletion list; this ADR extends the same discipline to
dirty-file cleanup.**

## What this deletes

Removed outright (≈1,800+ production LOC):

| Code | Location |
|---|---|
| The RAM dirty tier (`dirty` HashMap) and its lock choreography | `disk_daemon/backend.rs` |
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
dirty-file suite (see Testing). Simplified: `rehydrate_sandbox` /
`reattach_manifest` shrink to open-file → rebuild-bitmap →
RECONFIGURE → lossless fallback.

Unchanged: NBD netlink + dead-conn parking (ADR 0017), VM-detach and
pidfd reattach (ADR 0044 K2), the flush scheduler and manifest publish
(now the node-death floor only), the coordinator session FSM,
`dead_host` for genuine node loss.

New code, in full: the pwrite/pread dirty-file backend (~150-250
lines), extent-scan bitmap rebuild (~50), hole-punch after publish
(~30). No file format, no replay logic, no fsync ordering.

## Tradeoffs

1. **Double-write.** Every dirty byte hits host NVMe twice over its
   life: kernel writeback of the dirty file, then the chunk upload
   path. Bounded by the dirty window (hole-punched after each 30s
   flush); real bandwidth and SSD wear, accepted. The kernel schedules
   the writeback — no latency on the ack path.
2. **Page-cache memory pressure.** Dirty-file pages count as page
   cache, not process RSS. The kernel writes them back and reclaims
   under pressure — better behavior than today's pinned HashMap RAM,
   but different accounting; watch it during rollout.
3. **Kernel panic** (OS dies, disk intact) loses un-synced page cache
   and falls back to the GCS floor — same as node death. Rare; the
   `ENGRAM_DIRTY_FSYNC` flag exists if we ever care.
4. **Node death is unchanged.** Up to ~30s of writes lost, as today.
   The rollback card keeps that case — where "host failure" is true.
5. **Sparse-file semantics become load-bearing.** SEEK_DATA/SEEK_HOLE
   and PUNCH_HOLE behavior on the hostPath filesystem (ext4) must be
   covered by the test suite; both are old, stable ext4 features.
6. **This protects the disk, not guest memory.** A VM destroyed before
   its memory settles to the durable floor (ADR 0101) can still rewind
   conversation state. This design removes every *reason* to destroy
   such a VM in a hurry — the disk is safe, so recovery has no clock —
   but the memory floor itself is out of scope here.
7. **Publish-layer bugs remain possible** (the #897 class). The
   verify-before-punch rule bounds their cost to a redundant re-upload
   instead of data loss. The nightly DST oracle that caught #897 stays.

## Testing

- **Crash-state (ADR 0099 H5):** kill a live serve loop at random
  points under a random write schedule (SIGKILL, no cooperation);
  assert the successor's extent scan + pread reproduces every acked
  write. States are plain files — externally constructible, no fail
  points.
- **Property test:** random write/flush/kill/recover schedules against
  a model disk; after recovery, every acked byte reads back. Wildcard-
  free match over `NbdCommand` so a new command is a compile error.
- **Extent semantics:** dedicated tests for SEEK_DATA/SEEK_HOLE
  rebuild and PUNCH_HOLE-vs-write races on ext4.
- **DST/cosim:** the existing quarantine flows (ADR 0098 Flow A/B)
  invert — assert a roll produces zero `durability_rollback` events and
  zero rewound event indexes.
- **FC integration (CI-wired per AGENTS.md):** minimal write → SIGKILL
  host-agent → successor recovers → read back. Smallest data that
  proves the property; no throughput measurement.

## Rollout

1. Land the dirty-file backend + recovery behind the existing reattach
   flow (scaffolding still present, now unexercised).
2. Prove zero-rollback rolls in prod for one week of normal deploy
   cadence (`engram_durability_rollback_total` flat); watch page-cache
   and NVMe-bandwidth metrics.
3. Delete the scaffolding (the table above) in one retirement PR chain.
4. Flip this ADR to Accepted with the commit list; add the terminal
   addendum to ADR 0090 (quarantine concept retired) and mark ADR 0076
   Superseded by this document.

## Relationship to other ADRs

- **ADR 0007** (chunked-immutable storage): untouched; the dirty file
  is a staging tier under it, not a replacement.
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
