# ADR 0110: Honest writes — acked disk writes survive process death

Status: Proposed (2026-08-02)

Terms used in this document:

- **Guest**: the VM a user's session runs in.
- **Host-agent**: our program on each node. It runs the VMs and serves
  their disks. We redeploy it about 11 times a day.
- **NBD**: a Linux feature (Network Block Device). It lets a normal
  program act as a disk. The kernel forwards the guest's disk reads
  and writes to that program.
- **Ack**: the "done" answer the disk sends back after a write.
- **Dirty data**: guest writes we have not yet uploaded to cloud
  storage.
- **Chunk store**: our cloud storage (GCS). It holds disk data in
  pieces called chunks.
- **Manifest**: a small record that lists which chunks make up the
  disk, at one numbered version.
- **Page cache**: the kernel's own memory for file contents. It
  belongs to the machine, not to any process.

## Summary, in plain English

Every disk makes one promise. When a program asks it to make data
safe, the disk must not answer "done" until the data is truly safe.
Every filesystem is built on this promise. If the power dies, the
filesystem loses only data it was never promised. It recovers on its
own. This is normal and safe.

Our disk server breaks this promise. It acks guest writes into its own
process memory. When we redeploy the host-agent, that memory vanishes.
Data the guest was told was safe is destroyed.

We built a large recovery apparatus to manage the damage: quarantine,
retry ladders, a shutdown spool, a destroy-and-rewind arm, and a red
"guest disk rolled back" card in the UI. All of it exists because
acked data lives in process memory.

The fix: move the dirty data out of process memory. Put it in a plain
file on the node's own SSD, written on every guest write. Files belong
to the kernel, not to the process. A killed process loses nothing it
wrote to a file. After this change, a host-agent death looks to the
guest like a power cut on an honest disk. That is the exact event its
filesystem is designed to survive. The recovery apparatus becomes
deletable.

## Context

### The defect

Two halves of one problem:

1. **Acked writes live only in process RAM.** The dirty data sits in a
   map in host-agent memory (`ChunkedDiskBackend.dirty`,
   `backend.rs:346`). It is the only copy until the next upload. The
   uploads run every 30 seconds.
2. **FLUSH is a lie.** FLUSH is the guest's "make my data safe now"
   command. Our server answers "done" and does nothing
   (`runtime.rs`, the NBD command match):

```rust
// FLUSH: ack at the wire level but defer durable-flush to the
// snapshot path (inline per-FLUSH flush-to-chunk-store would
// multiply object-storage cost ~100×; we trade strict-FUA for cost).
NbdCommand::Flush | NbdCommand::Trim => (NbdReply::ok(req.handle), None),
```

The comment weighs honesty against cloud storage only. That is a false
choice. A third option — the node's own SSD — costs microseconds. It
was never considered.

### The cost

When a host-agent pod rolls, the process dies. The kernel NBD device
survives (ADR 0017). The VM survives (ADR 0044 K2). Only the RAM dirty
data dies. Everything below exists to cope with that:

- On 2026-08-02, 12 sessions showed the "disk rolled back" event in
  one day. They came in bursts — up to 5 sessions in 12 seconds, one
  burst per host-agent roll. Two sessions lost real user work.
- Since 2026-05-20, about 106 commits patched this failure class. That
  is ~10% of all repo activity. The quarantine retry ladder alone was
  rewritten four times.
- 13 ADRs carry load for it (0015-0018, 0028, 0044, 0050, 0069, 0090,
  0091, 0098, 0099, 0101). ADR 0090 carries three addenda.
- About 2,600 lines of production code and 3,750 lines of tests exist
  only to manage it.
- The latest patches (#971, #972) landed on 2026-08-02. They stop the
  worst outcome. They do not remove the cause.

### Prior art

e2b (e2b-dev/infra) runs the same design: an NBD server inside the
node agent, and lazy chunked base images from GCS. They do not have
this bug class. Their write cache is a real file from the start
(`block/cache.go`). Every guest write lands in the kernel page cache
at ack time. They do not even offer FLUSH to the guest, because a
write-through cache leaves nothing for FLUSH to add.

We adopt their primitive. We keep one thing they lack: our VMs survive
host-agent restarts. e2b kills every leftover VM when its agent
starts. That fits their product (throwaway sandboxes). It does not fit
ours (long-lived work sessions).

### Rejected alternatives

- **Drain on every roll** (park all sessions, then deploy): slow
  deploys, and every deploy interrupts active sessions.
- **A separate disk-server process per VM** (ADR 0076): keeps the data
  alive by keeping a process alive. A crash of that process still
  loses everything. Now superseded.
- **Upload to cloud on every FLUSH**: ~100ms per guest fsync. Too
  slow. The original comment was right to reject it.
- **A safety journal written at FLUSH time** (this ADR's first draft):
  correct, but it protects only flushed writes. It also needs its own
  file format, version counters, and cleanup logic. Write-through
  makes all of that unnecessary.

## Decision

**Rule: an acked write is in a node-local file before the ack.**

The dirty data becomes a plain file per sandbox. Every guest write
goes into that file before we answer "done." No acked byte ever exists
only in process memory.

**What this defends against.** Process death: deploys, panics,
SIGKILL, out-of-memory kills. When a write lands in a file, the kernel
page cache holds it from that moment. The writing process can die any
way at all; the data stays.

**What this does not defend against.** Node death (the whole machine
vanishes). That is covered by the existing 30-second uploads to the
chunk store, unchanged.

This split is what keeps the code small. Hard journal engineering —
fsync ordering, torn-write recovery, write barriers — exists to
survive power loss *without a backup*. We have a backup: the chunk
store. We only need to survive the event that actually recurs — the
process dying. The kernel gives us that for free.

## Design

### The dirty file

One file per sandbox, on the node's SSD, next to the chunk cache:

```
/var/lib/engram/dirty/<sandbox_id>.cache
```

We create it at sandbox start as a **sparse file**: a file with holes.
Empty parts use no disk space. It has no format. Byte N of the file is
byte N of the guest's disk. Next to it lives one tiny sidecar,
`<sandbox_id>.ref` — the last manifest ref this sandbox published
(see the punch rules below). Both are removed at sandbox destroy, and
a startup sweep reaps the files of sandboxes that died without one.

### Data path

1. **Guest write**: the first write to a chunk *materializes* it —
   fetch the full base chunk, patch the guest's bytes into it, and
   `pwrite` the whole merged chunk into the file at its offset
   (`pwrite` is a system call: "write to a file at this position").
   Later writes to the same chunk patch the file directly. Mark the
   touched chunks in an in-memory "dirty" bitmap. Then ack. The old
   RAM map is deleted. This whole-chunk rule is load-bearing: the
   file's data extents are always complete chunks, so recovery can
   never mistake hole-zeros next to a partial write for real data.
   The base fetch was already in the write path (the RAM map did the
   same read-modify-write); the new cost is one chunk-size `pwrite`
   into the page cache on first touch.
2. **Guest read**: if the bitmap says the range is dirty, read the
   dirty file with `pread`. Otherwise read the base image path,
   unchanged.
3. **Guest FLUSH**: ack, as today — but now the ack is honest. Acked
   writes are already safe against process death. There is nothing
   left for FLUSH to do. (This is why e2b does not offer FLUSH at
   all.)
4. **No fsync.** `fsync` forces file data onto the physical disk. We
   do not need it: the page cache already survives process death, and
   node death falls back to the chunk store. An env flag
   (`ENGRAM_DIRTY_FSYNC=1`, default off) turns it on if we ever want
   kernel-panic protection.
5. **Disk errors stay small.** A failed `pwrite`/`pread` returns an
   error code. We turn it into an error reply for that one request.
   One sandbox degrades; the host-agent lives. (This is why we use
   file calls instead of e2b's mmap. A bad mmap access crashes the
   process with a SIGBUS signal and needs a special guard. A bad
   `pwrite` is just an error value.)

### Uploads (unchanged rhythm, new source)

The existing upload loop still runs every 30 seconds (or at 256 MiB of
dirty data). It now reads dirty chunks from the file, uploads them to
the chunk store, and publishes a new manifest — as today.

After a successful publish, we can punch holes in the uploaded ranges
(`fallocate(PUNCH_HOLE)`: "turn this part of the file back into empty
space"). That keeps the file small. If a chunk was re-written during
the upload, we keep it. That check is a simple in-memory flag. Losing
the flag in a crash only means we upload a chunk twice. The chunk
store is content-addressed, so a double upload is harmless.

**One strict rule protects the file: never delete dirty data because a
publish *said* it worked.** Before punching a hole, fetch the
published manifest back and check it really contains those chunks.
This is the same "verify the occupant" discipline PR #897 established.
With this rule, a bug in the publish layer costs a redundant upload,
never data. (Incident E below is why this rule exists.)

**A second strict rule covers the coordinator's lag: never punch a
hole before recording the published ref beside the file.** The
coordinator learns a new manifest ref *after* the flush returns (an
async publisher). A crash in that window would make the successor
attach at the coordinator's older ref — and a punched chunk would then
resolve to its pre-write base hash. So each publish first writes a
tiny sidecar (`<sandbox_id>.ref`, an atomic temp-write + rename) with
the published ref. Recovery attaches at the sidecar's ref when it is
newer than the coordinator's. If the sidecar write fails, we skip the
punches; the chunks stay in the file and upload again later. This
also covers the ADR 0077 fork window: the first flush after a
restore publishes under a brand-new private manifest id, which no
"latest version" lookup on the coordinator's id could find — the
sidecar records it. (The old shutdown spool recorded its ref in
`meta.json` for the same reason; this is that idea, kept.)

### Recovery after a host-agent death

The new host-agent, for each surviving sandbox:

1. Pick the attach ref: the ref sidecar's, if it is newer than the
   coordinator's (the store-ahead rule above).
2. Open the dirty file. Ask the kernel which parts have data
   (`lseek` with `SEEK_DATA`/`SEEK_HOLE`: "find the next data / next
   hole"). Those parts are the dirty set. Rebuild the bitmap from
   them. If the filesystem reports a data range that only touches
   part of a chunk, mark the whole chunk dirty. The whole-chunk write
   rule above makes this rounding safe, and the worst case is one
   extra harmless upload. (The exact-extent scan needs ext4 semantics;
   recovery is Linux-only, and the tests for it are Linux-gated.)
3. Hand the surviving kernel NBD device a fresh socket
   (`NBD_CMD_RECONFIGURE`, a kernel command for exactly this).
4. Done. No spool adoption. No probe reads. No quarantine.

**A node reboot cannot feed us a torn file.** Without `fsync`, a
rebooted node's dirty file may hold an arbitrary subset of writes.
That file is never trusted: recovery runs only for a *surviving VM*
(the startup pass reattaches to live Firecracker processes), and no
VM survives a reboot. A post-reboot file has no surviving owner, so
nothing ever opens it. A startup sweep deletes it — together with
every dirty-root file whose sandbox is not in the live set, and every
`.pending-*` temp file from a dead process. Sandbox ids never recur,
so a swept file can never be wanted again.

If step 2 fails (a kernel error, an identity mismatch): upload the
dirty file's chunks, publish the manifest, destroy the VM, and let the
next prompt resume from that manifest. **No acked write is lost.** A
failed reattach becomes a normal park-and-resume, not a rollback.

### Snapshots without a live disk

The published base plus the dirty file fully describe the disk. So
eviction can build its snapshot from host files alone. It no longer
needs a live NBD device. This is what lets us delete
`RefuseUntracked` — the code that refused to snapshot a survivor
because the snapshot would have dropped its acked writes. There is
nothing left to drop.

### The guest's view

Unchanged. The guest already assumes a disk that loses nothing it
acked. We now actually are one, across process death. Node death keeps
power-cut behavior, backed by the chunk store, as today.

## Would this have prevented the real incidents?

We replayed the four most recent fixes in this area against this
design, step by step. Four are prevented. One (E) is only partly
covered — it is flagged, not hidden, and the rule above closes it.

### A. The 2026-08-02 roll cascade (this ADR's trigger)

Today:

1. A deploy replaces the host-agent pod.
2. The old process dies. The RAM dirty data dies with it.
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
3. The new process opens the file, rebuilds its bitmap, and reconnects
   the device.
4. There is nothing to rescue. No quarantine, no destroy, no rewind,
   no red card.

**Verdict: prevented.**

### B. PR #971 — a panic in the shutdown sequence

The bug (now patched): shutdown ran a careful sequence — upload, then
dump leftover RAM writes to a spool file, then detach. A panic in the
middle skipped the dump AND disconnected every disk device on the way
down. The RAM-only writes were gone. The successor could not even
reconnect the devices.

With this ADR: shutdown has no disk work to do. The data is already in
the file before shutdown starts. A panic at shutdown has nothing left
to break. Even a wrongly disconnected device only costs the reconnect.
A failed reconnect rebuilds from the file with zero loss.

**Verdict: prevented — the failing code is deleted, not fixed.**

### C. PR #972 — destroy after three failed rescues

The bug (now patched): the rescue retry budget was built for flaky
failures. But quarantine fails the same way every time. After 3 tries,
the give-up arm destroyed the VM — the only copy of the acked writes.

With this ADR: the ladder, the budget, and the destroy arm are all
deleted. A stuck reconnect has no clock on it. The data is safe on
disk, so the system can retry calmly forever — or rebuild the session
from the file with nothing lost.

**Verdict: prevented — this PR's machinery is literally what gets
deleted.**

### D. PR #843 — session 61a03b7e, 93 events rewound

The bugs (now patched):

1. A session was parked (VM paused in place). A pod roll landed 50
   seconds later.
2. A hand-written SQL list had never learned the new "parked" state.
   So the new pod was never told to reattach that session's disk.
3. The host's local fallback then mixed up two record types (a memory
   manifest vs a disk manifest). The reattach failed. Worse: it threw
   away the spool file — the only durable copy of the disk writes —
   as "foreign."
4. Quarantine → destroy → the resume rewound 7 minutes and 93 events.

With this ADR:

1. Same park, same roll.
2. The coordinator's list no longer matters for safety. Recovery finds
   the dirty file by sandbox id on the local disk. There is no
   manifest lookup to get wrong. There is no "foreign" check to
   wrongly refuse. Those concepts are gone from the recovery path.
3. The reconnect succeeds. Even if it did not, the file rebuilds the
   disk with zero loss.

**Verdict: prevented — one bug becomes harmless, the other becomes
impossible.** Honest caveat: this ADR protects the *disk*. A parked
VM whose *memory* is not yet checkpointed can still lose conversation
state if something destroys it (that is ADR 0101's domain). Under this
design, nothing needs to destroy it. But the memory floor itself is
unchanged here.

### E. PR #897 — a stale manifest published as the floor. NOT fully fixed — flagged.

The bug (now patched): an upload crashed halfway. It had durably
written its manifest to the store, but crashed before updating its own
memory of that fact. Later, the snapshot code hit a "someone already
published my version number" conflict. It assumed — without looking —
that the occupant was its own earlier work. So it kept the stale
manifest as the official record. The newest write's only copy sat in
the spool file. The version gate then refused that spool as "old."
(Caught by the nightly simulator — ten failing seeds — never confirmed
in prod.)

Why this ADR alone does not fix it: the manifest/publish layer stays.
It is our defense against node death. A bug in that layer can still
record a wrong "official" version. What changes: the spool and its
version gate are gone, and the newest write lives in the dirty file
instead. The loss could only recur if cleanup deleted the dirty file
while trusting a lying publish result. The rule in the Design section
exists to close exactly that: verify the published manifest really
contains the data before punching it out of the file. With that rule,
this bug class costs a redundant upload, not data.

**Verdict: the #897 fix (verify the occupant) stays load-bearing and
is NOT in the deletion list. This ADR extends the same discipline to
dirty-file cleanup.**

## What this deletes

Removed outright (≈1,800+ production lines):

| Code | Location |
|---|---|
| The RAM dirty map and its lock choreography | `disk_daemon/backend.rs` |
| The quarantine rescue ladder: the 3-try budget, the park-and-recover arm (#972), `quarantine_reap_unevictable` | `session_verbs.rs`, `idle_evictor.rs` |
| The `quarantined_survivors` map, its heartbeat advertising, and the coordinator code that consumes it | `pooled_backend.rs`, `heartbeat.rs`, `host_http.rs`, `engram-protocol` |
| `RefuseUntracked` / `CaptureDrainPlan` (the snapshot refusal) | `engram-host-core/src/survivor.rs` (whole file) |
| The SIGTERM spool: the shutdown dump, the all-or-nothing adoption, the final-flush sequence (#971's hardening included) | `disk_daemon/spool.rs`, `runtime.rs` adoption arms |
| The `VerifySeed` probe-read machinery | `runtime.rs` reattach path |
| The NBD slot's quarantine state and its state-machine arms | `disk_daemon/slot.rs` |
| The "disk rolled back" event on the process-death path. The event, metric, and web card stay only for true node loss | `state.rs`, `session_verbs.rs`, `idle_evictor.rs`, `SystemMessage.tsx`, `buildMessages.ts` |

Replaced: ~3,750 lines of recovery tests (`nbd_startup_recovery`,
`nbd_shutdown_abandon_race`, `nbd_shutdown_final_flush`,
`chain_rehydrate`, …) collapse into a small dirty-file suite (see
Testing). Simplified: `rehydrate_sandbox` / `reattach_manifest` shrink
to open-file → rebuild-bitmap → reconnect → lossless fallback.

Unchanged: the kernel NBD survival work (ADR 0017), VM survival across
restarts (ADR 0044 K2), the upload loop and manifest publish (now the
node-death floor only), the coordinator session state machine,
`dead_host` for true node loss.

New code, in full: the file-backed dirty tier (~150-250 lines), the
extent-scan recovery (~50), hole-punching after verified publishes
(~30). No file format. No journal. No fsync ordering.

## Tradeoffs

1. **Every dirty byte is written twice on the host.** Once into the
   dirty file (the kernel writes it back to SSD in the background),
   once via the chunk upload. Real bandwidth and SSD wear. Bounded by
   the hole-punching. Accepted.
2. **Memory accounting changes.** Dirty-file pages live in the page
   cache, not in process memory. The kernel writes them back and
   frees them under pressure. That is better behavior than today's
   pinned RAM map, but it is a different meter. Watch it in rollout.
3. **A kernel panic** (the OS dies, the disk survives) loses unsynced
   page cache. We fall back to the chunk store, same as node death.
   Rare. The fsync flag exists if we ever care.
4. **Node death is unchanged.** Up to ~30 seconds of writes lost, as
   today. The red card stays for that case — where "host failure" is
   finally the truth.
5. **Sparse-file behavior becomes load-bearing.** The
   `SEEK_DATA`/`SEEK_HOLE` and `PUNCH_HOLE` calls on ext4 get their
   own tests. Both are old, stable kernel features.
6. **This protects the disk, not guest memory.** A VM destroyed before
   its memory checkpoint can still rewind conversation state
   (ADR 0101's domain). This design removes every *reason* to destroy
   such a VM in a hurry — the disk is safe, so recovery has no clock.
   But the memory floor itself is out of scope here.
7. **Publish-layer bugs remain possible** (the #897 class). The
   verify-before-punch rule bounds their cost to a redundant upload
   instead of data loss. The nightly simulator oracle that caught #897
   stays.

## Testing

- **Crash tests (ADR 0099 H5 style):** run a live server under a
  random write schedule. SIGKILL it at random moments, with no
  cooperation. Assert the successor's extent scan reproduces every
  acked write. The crash states are plain files — easy to build, no
  fault-injection hooks needed.
- **Property test:** random write/flush/kill/recover schedules against
  a model disk. After recovery, every acked byte must read back. The
  command match has no wildcard arm, so a new NBD command is a compile
  error, not a silent gap.
- **Extent tests:** dedicated coverage for the `SEEK_DATA`/`SEEK_HOLE`
  rebuild and for hole-punch racing a concurrent write, on ext4.
- **Simulator (DST/cosim):** the existing quarantine flows (ADR 0098
  Flow A/B) invert: a roll must produce zero "disk rolled back" events
  and zero rewound event indexes.
- **Firecracker integration (wired into CI per AGENTS.md):** the
  smallest possible proof — write, SIGKILL the host-agent, recover,
  read back. No throughput measurement.

## Rollout

1. Land the dirty-file backend and recovery behind the existing
   reattach flow. The old scaffolding stays in place. To be plain
   about what that buys: the spool is still *written* at every orderly
   shutdown, but recovery ignores it whenever a dirty file exists — so
   during the soak the new path is the live recovery path, and the
   spool is an on-disk copy an operator can adopt by hand, not an
   automatic net.
2. Prove one week of zero-rollback rolls at normal deploy cadence
   (`engram_durability_rollback_total` stays flat). Watch page-cache
   use and SSD bandwidth (tradeoffs 1 and 2). Watch two more gates:
   **p99 write-ack latency** (under memory pressure the kernel can
   throttle `pwrite` into writeback — the RAM map never did that) and
   **node disk headroom** (during a chunk-store outage the files grow
   until publishes succeed again; a full disk turns into per-sandbox
   EIO, which degrades one guest at a time — better than the RAM
   map's OOM, but it must be visible before it happens).
3. Delete the scaffolding (the table above) in one retirement PR
   chain.
4. Flip this ADR to Accepted with the commit list. Add the closing
   addendum to ADR 0090 (quarantine retired). Mark ADR 0076
   Superseded by this document.

## Relationship to other ADRs

- **ADR 0007** (chunked storage): untouched. The dirty file is a
  staging layer under it, not a replacement.
- **ADR 0017** (NBD lifecycle): its kernel-survival work is what makes
  recovery a reconnect instead of a device rebuild. Kept.
- **ADR 0028 / 0101** (eviction durability): the durable floor they
  define becomes the *node-death* floor only.
- **ADR 0044 K2** (VM survival): kept. Rolls stay instant. This ADR
  makes the surviving VM's disk state actually safe.
- **ADR 0076** (one shared disk daemon): superseded. It kept state
  alive by keeping a process alive. We keep state alive by putting it
  in files.
- **ADR 0090** (sandbox ownership): quarantine, its ladder, and the
  destroy arm retire. The ownership model itself stands.
