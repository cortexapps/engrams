# ADR 0125: The kernel owns the session disk — dm-thin volumes replace the NBD disk daemon

- Status: Proposed (2026-11-03). When accepted, this ADR supersedes
  ADR 0110 (honest writes) and Decision 3 of ADR 0090 (the quarantined
  survivor channel), and retires ADR 0049 (the NBD slot allocator).
- Date: 2026-11-03
- Related: ADR 0007 (chunk store — unchanged, it stays the durable and
  portable format), ADR 0036 (per-chunk image artifacts and fleet
  prestage — the staging target changes), ADR 0028 / 0039 / 0077
  (checkpoints — the disk half changes, the memory half does not),
  ADR 0079 and ADR 0116 (fencing epochs and binding leases — unchanged,
  and after this ADR they are the only ownership mechanism), ADR 0084
  (capture job — reads from a snapshot instead of the daemon), ADR 0112
  (guest swap — becomes a plain thin volume), ADR 0123 (teleport — the
  manifest path is unchanged), ADR 0098 / 0099 (simulation and
  invariants).

## Terms

- **Guest**: the VM a session runs in.
- **Host-agent**: our process on each node. It starts VMs and, today,
  serves their disks. It is redeployed often.
- **Chunk store** (ADR 0007): cloud object storage that holds disk and
  memory data as 16 MiB content-addressed chunks. A **manifest** lists
  the chunks of one disk at one version.
- **Thin pool**: a Linux device-mapper object (`dm-thin`) that carves
  one data partition into many **thin volumes**. A thin volume is a
  block device whose blocks are allocated on first write. A
  **snapshot** of a thin volume is a new thin volume that shares all of
  its blocks until one side writes to them. The pool's **metadata**
  partition records which volume maps which block.
- **Block**: the pool's unit of allocation and sharing. This ADR uses
  64 KiB.
- **thin_delta**: a tool on the node that lists the blocks that differ
  between two thin volumes, from metadata alone, without reading data.

## Summary, in plain English

Today the host-agent process contains the disk server for every VM on
the node. The kernel forwards each guest read and write over NBD to that
server, which keeps a cache of base chunks and a per-session dirty file.
When the host-agent restarts, each VM's disk must be handed from the old
process to the new one. That handover is where both of the disk
subsystem's production incidents lived: a lost dirty tier that rolled
sessions back (2026-07, the reason for ADR 0110), and a recovery
pre-mark that evicted every running session on every deploy (2026-10,
PR #1728). Three startup passes, a quarantine map, a shutdown spool, a
retry ladder, and a coordinator-side consumer exist only to make the
handover safe, and each was added after an incident.

After this change, a session's disk is a block device the kernel owns.
The host-agent creates it, gives its path to Firecracker, and does not
touch it again until the session checkpoints or ends. A host-agent
restart does not touch disks at all. There is nothing to hand over, so
the handover machinery is deleted rather than fixed.

The chunk store and its manifests do not change. A checkpoint still
produces a manifest. A session still resumes from a manifest on any
host. Only the node-local representation of a disk changes: from "a
daemon plus a dirty file" to "a thin volume".

## Context

### The data path today

`disk_daemon/runtime.rs` runs an NBD server per sandbox inside the
host-agent. `disk_daemon/backend.rs` maps reads to chunk fetches from a
node-local cache (filled before an image is marked ready, ADR 0036) and
writes to a per-sandbox sparse dirty file (ADR 0110). A first write into
a 16 MiB region copies the whole base chunk into the dirty file before
the write is acknowledged. `flush_scheduler.rs` uploads dirty chunks
about every 30 s. On host-agent restart, `rehydrate_sandbox` rebuilds
the daemon's state from the dirty file, the spool, or the last published
manifest, and reconfigures the kernel device to the new process.

### The failure class is the handover

| Date | Incident | Mechanism |
|---|---|---|
| 2026-07 | Sessions rolled back to an old manifest after a deploy | Acked writes lived in daemon RAM; the SIGTERM flush overran its budget (ADR 0110 context) |
| 2026-07 | A healthy VM was killed after a failed reconfigure | Host-local ownership state was empty after restart (ADR 0090 context) |
| 2026-10 | Every running session evicted on every deploy, mid-build | A pre-recovery quarantine mark reached the coordinator before the recovery finished (PR #1728) |

Each fix added a mechanism beside the handover instead of removing the
handover. ADR 0110 decided that most of this machinery "becomes
deletable" once writes are durable, and its retirement PR has been open
since 2026-08. Later work extended the condemned path instead. The
lesson of this ADR is structural: a process that restarts several times
a week must not own a resource whose lifetime is a session.

### What the production disk can do

All numbers below come from one rig that matches the production node
shape: a c3-standard-22 with a 750 GB Hyperdisk Balanced provisioned at
25k IOPS and 1200 MB/s, measured with `fio --direct=1 --fsync=1
--iodepth=1 --bs=4k --rw=randwrite` unless stated. "Today" numbers are
from production NBD metrics on a busy host.

| Measure | Today (NBD daemon) | XFS reflink file | dm-thin, 64 KiB blocks |
|---|---|---|---|
| Write ack, p50 / p99 | 0.05 ms (page cache) / 5 s (chunk copy) | fsync p50 1.7 ms | fsync p50 2.4 ms on shared blocks; 0.3 ms once private (raw disk) |
| Point-in-time copy of a live disk | freeze + capture | FICLONE 2–2.6 s at 67k extents, grows with age | 20 ms at any age (suspend 3–5 ms, message 3 ms, resume 11 ms) |
| Diff for upload | sparse re-chunk | FIEMAP 0.3 s | thin_delta 106 ms, exact at block granularity |
| Read after 30 point-in-time copies | n/a | n/a | flat, equal to the base |
| Stage a 26 GiB image | prefetch at disk speed | 1.26 GB/s | 1.26 GB/s |
| Activate 50 session disks | per-session NBD attach | n/a | 1.6 s total |

The reflink file was the first candidate. Its point-in-time copy walks
the extent map under the inode lock, so the cost grows with the file's
write history, and a long-lived, heavily written session is exactly the
session a checkpoint must not stall. dm-thin's snapshot copies the root
of a mapping tree and is O(1). That decided the primitive.

Three secondary results fix the pool's parameters:

- **64 KiB blocks.** First-write latency rises with block size (median
  1.5, 2.5, 3.2 ms at 64 KiB, 256 KiB, 1 MiB). At 1 MiB the block layer
  splits a 16 MiB staging write below the block size, so the pool loses
  its full-block overwrite path and staging a version delta is 2.5×
  slower. 64 KiB is also finer than the 16 MiB chunk we upload, so the
  delta never inflates what we send.
- **Block zeroing stays on.** Without it, a partial write into a block
  provisioned from the pool's free space exposes the previous owner's
  data, which may be another session's. Zeroing costs about 2.4× on
  writes to never-used blocks only (median 0.7 ms against 0.3 ms) and
  nothing on the copy-on-first-write path or on staging. A write that
  covers a whole block skips zeroing.
- **Snapshots are transient.** A chain of 30 snapshots, each after
  64 MiB of random 4 KiB writes, used 69 GiB of pool space. A checkpoint
  therefore creates a snapshot, uploads from it, and deletes it.

### What the production node image can do

The GKE node image for the Firecracker pool (Container-Optimized OS)
has `dm_thin_pool` as a signed in-tree module, and `dmsetup` and
`thin_check` in its userland. Loading the module, building a pool, and
creating thin volumes and snapshots were verified end to end on that
image. The chart's node-prep init container is privileged with
`/lib/modules` mounted and already runs `modprobe nbd`. The node
contributes only the kernel module: the host-agent image bundles
`dmsetup` and the thin-provisioning tools (`thin_check`, `thin_delta`,
`thin_repair`) it calls, so the design does not depend on the node
userland. Nothing outside the in-tree kernel is needed.

## Decision

1. **One thin pool per node.** Node-prep creates it on a dedicated
   data disk with two partitions: metadata (8 GiB; the estimate for
   750 GiB of data and 500 volumes at 64 KiB blocks is 383 MiB) and
   data. Block size 64 KiB, block zeroing on, discard passdown on.
   Node-prep runs `thin_check` before activation and refuses to
   activate a pool that fails it. The pool is created once and
   re-activated on every node boot; it is never recreated while any
   session volume exists.
2. **An image version is a thin volume.** Staging an image writes its
   manifest's chunks into a new thin volume. Staging a version whose
   predecessor is already on the node snapshots the predecessor and
   writes only the chunks whose hash differs at the same offset. The
   image is marked ready when the volume is complete, exactly where the
   prefetch supervisor marks it ready today. Placement keeps gating on
   readiness.
3. **A session disk is a thin snapshot.** A fresh session's root disk
   is a snapshot of its image volume. A resumed session's root disk is
   a snapshot of its base image volume with the session's dirty chunks
   written in. Guest swap (ADR 0112) is a fresh thin volume with no
   origin; it is never captured. Every volume a session owns is
   recorded in its `sandbox.json`.
4. **Firecracker opens the device path.** The guest's virtio-blk drive
   is `/dev/mapper/<volume>`, opened as buffered host I/O as it is
   today. No userspace process is in the data path. A write is acked
   from the host page cache; a flush is a page-cache writeback into the
   pool.
5. **A checkpoint is a snapshot.** Pause the VM, sync the device's page
   cache, suspend the volume, create a snapshot, resume the volume,
   resume the VM. The guest pause is the sync of writes since the last
   writeback plus about 20 ms. The memory half of the checkpoint (ADR
   0039 sparse memory, ADR 0092 lazy base) is unchanged and is taken
   inside the same pause, so memory and disk stay coherent. The upload
   reads from the snapshot: `thin_delta` against the previous
   checkpoint's snapshot gives the dirty blocks; the uploader hashes
   every 16 MiB chunk that contains a dirty block and publishes a
   manifest per ADR 0007. The snapshot is deleted once the manifest is
   durable. The previous checkpoint's snapshot is kept only until the
   next checkpoint's delta is computed, so a session holds at most two
   snapshots at any moment.
6. **The host-agent owns no disk state.** There is no dirty file, no
   spool, no in-memory dirty tier, no slot FSM, no quarantine map. On
   restart the host-agent reattaches each VM by pidfd (ADR 0009, as
   today) and reads its volume names from `sandbox.json`. It does not
   open, reconfigure, suspend or verify the volume. The only
   disk-related startup work is `dmsetup ls` to confirm the recorded
   volumes exist; a missing volume is a corruption-class invariant
   failure (ADR 0099 H6), not a recovery case.
7. **The heartbeat reports facts, never events.** Per node: pool data
   and metadata free space. Per sandbox: the volumes exist. The
   `quarantined_sandboxes` field, its host-side map, the coordinator's
   advertise consumer, the quarantine flavour of the idle evictor and
   the retry pass are deleted. The coordinator places new sessions and
   resumes only on hosts whose pool free space exceeds the session's
   disk reservation, the same filter shape as memory and image
   readiness in `host_passes_filters`.
8. **The node-death tier is unchanged.** Periodic checkpoints bound
   the loss from a node that disappears to one checkpoint interval, as
   today. There is no third tier: a process death loses nothing and
   needs no remediation.
9. **This ADR is documented as facts.** On acceptance, DESIGN.md gains
   a "Session disk" fact page written in the present tense with a dated
   "verified against" line naming the tests and metrics that prove each
   fact. ADR 0110, ADR 0090 and ADR 0049 are collapsed to a stub: their
   status line, three sentences on what they decided and why it stopped
   being true, and a link to the fact page; the full text stays in git
   history. AGENTS.md names the fact pages as the current truth and the
   ADRs as the record of how it came to be. Code comments in the new path say what the code does; they cite this
   ADR only as a pointer to the reasoning, never as a lineage. Every
   phase PR updates the fact page in the same change.

## Design

### Objects on a node

```text
/dev/mapper/engram-pool                  the pool (metadata + data partitions)
/dev/mapper/img-<manifest_id>-v<N>       one volume per staged image version
/dev/mapper/sb-<sandbox_id>-root         session root disk (snapshot of an image volume)
/dev/mapper/sb-<sandbox_id>-swap         session swap (fresh volume, no origin)
/dev/mapper/sb-<sandbox_id>-ckpt-<K>     a checkpoint snapshot, transient
<work_dir>/<sandbox_id>/sandbox.json     records the volume names and the thin ids
```

Thin ids are allocated by the host-agent from a counter persisted under
the work dir, the same way NBD slots were recorded, and are written into
`sandbox.json` before the volume is created. A volume whose id is not
recorded anywhere is garbage and the image GC deletes it.

### Lifecycles

**Node bring-up.** Node-prep: `modprobe dm_thin_pool`; find the data
disk by its device symlink; partition it on first use; `thin_check` the
metadata; `dmsetup create engram-pool`. A failed check marks the node
not ready and surfaces in the heartbeat. The host-agent activates image
and session volumes it finds recorded; it never activates a volume it
has no record of.

**Stage an image.** Triggered by the heartbeat-ack `enabled_images`
set, as today. Create the volume (snapshot of the predecessor version if
present, else a fresh volume), stream the chunks whose hash is new at
their offsets, `fdatasync`, record the version as staged, mark ready.
The chunk download path and the GCS client are unchanged; only the sink
changes from the chunk cache to a device offset. The chunk cache that
backed the daemon is deleted; it has no other reader.

**Create a session.** Snapshot the image volume, create the swap
volume, write both names into `sandbox.json`, start Firecracker with
the device paths. Cost: two `dmsetup` calls, about 30 ms each.

**Checkpoint.** Decision 5. The driver (`checkpoint.rs`) and the
durable `CheckpointRecord` are unchanged; only the disk capture
primitive changes.

**Evict.** A final checkpoint, then the VM is destroyed and every
session volume is deleted (`dmsetup message delete`, which returns
the volume's exclusive blocks to the pool's free space). The eviction finalize
(`eviction_finalize.rs`) keeps its shape: a host-owned job over durable
artifacts, now the checkpoint snapshot instead of a frozen dirty file.

**Resume.** Decision 3. The dirty chunks are written before the VM
starts; nothing is faulted lazily from the chunk store on the disk path.
The memory path keeps its lazy base (ADR 0092).

**Teleport (ADR 0123).** The source takes a checkpoint and publishes a
manifest; the target resumes from it. The chunk-level protocol is
unchanged, so a source on an NBD-era host and a target on a thin-pool
host interoperate during rollout.

**Destroy.** Delete the volumes. A volume that is still open by a
Firecracker process cannot be deleted; the host-agent kills the process
first, as today.

**Host-agent restart.** Decision 6. Nothing touches a disk.

**Node death.** The coordinator's lease path (ADR 0116) marks the
session Idle at its last checkpoint, as today.

**Pool pressure.** The pool reports free space in the heartbeat and the
scheduler stops placing on a node below a low-water mark. When the data
partition fills, dm-thin queues guest I/O rather than failing it (the
`error_if_no_space` feature is off), so a session stalls rather than
corrupts; the idle evictor's existing eviction ordering drains the
node. Metadata exhaustion is prevented by sizing, and the heartbeat
reports it separately so an alert can fire long before it happens.

**Image GC.** Desired: the versions referenced by any session on this
node plus the enabled set. Observed: the image volumes that exist.
Delete the difference. Deleting an image volume whose blocks are shared
with session snapshots keeps those blocks allocated; the pool reclaims
them when the last sharer goes.

### The macOS backend

The VZ backend already follows this design: `engram-sandbox-vz/disk.rs`
clones the bake image per sandbox with APFS `clonefile` and the
hypervisor opens the file. This ADR gives the host-core seam a name for
what both backends do: a `SessionDisk` provider that stages a manifest
into a device, snapshots it, and lists the dirty 16 MiB chunks since a
previous snapshot. The Linux implementation is the thin pool. The macOS
implementation keeps its clone and computes the dirty set by comparing
the clone to the staged base, which is a development-only cost.
Production drives the design; the trait exists so that VZ exercises the
same capture code, not so that it can diverge.

### Development and CI

The Linux dev VM and the KVM CI runners build the pool on two loop
devices over sparse files under the work dir. The `dmsetup` calls are
identical. Firecracker integration tests (`engram-sandbox-firecracker/
tests/`) get one small test per lifecycle step above, sized to the
property, wired into `ci.yml` per AGENTS.md.

## What this deletes

| Today | Why it existed | After |
|---|---|---|
| `disk_daemon/runtime.rs` (NBD server, netlink connect / reconfigure) | serve the disk from userspace | gone; the kernel serves it |
| `disk_daemon/backend.rs` (chunk cache + dirty file data plane) | lazy base + dirty tier | staging writes chunks into a volume; no data plane |
| `disk_daemon/slot.rs` (NBD slot allocator, ADR 0049) | `/dev/nbdN` is a scarce handle | a thin id counter |
| `disk_daemon/spool.rs` (shutdown spool) | save the RAM dirty tier at SIGTERM | nothing is in RAM |
| `disk_daemon/flush_scheduler.rs` | upload dirty chunks every 30 s | the checkpoint driver, which already exists |
| `device_sync.rs` | force page cache into the daemon before a freeze | one `fdatasync` in the checkpoint primitive |
| `rehydrate_sandbox`, `track_rehydrate`, the three startup recovery passes in `pooled_backend.rs` | rebuild daemon state after restart | reattach by pidfd only (`live_attach.rs`) |
| `quarantined_survivors`, the heartbeat field, `quarantined_survivor_advertise_core`, the quarantine flavour of `idle_evictor.rs`, the retry pass | react to a failed handover | no handover |
| The 16 MiB copy-before-ack on first write | dirty file granularity | 64 KiB copy-on-first-write in the kernel |
| ADR 0110's dirty-file format and its `SEEK_DATA` bitmap rebuild | durable acked writes | the pool is durable by construction |

The deletion is the point. Each phase below retires its rows; a phase
that adds code without retiring the row it replaces is incomplete.

## What does not change

- The chunk store, chunk size, manifest schema and GCS layout (ADR
  0007). Every stored snapshot resumes unchanged.
- Memory snapshots, UFFD paging and the lazy base (ADR 0039, 0092).
- Session ownership: one Postgres row, fencing epochs on every
  destructive host action (ADR 0079), the binding lease and dead-host
  path (ADR 0116). This ADR removes the one side channel that bypassed
  them; it adds no new ownership mechanism.
- The `spawn()` / `run_once()` driver pattern, the simulator and the
  conformance rule (ADR 0098).
- The image bake pipeline and the hourly delta it produces. Staging
  cost per node per version is proportional to that delta under this
  design exactly as it is under the chunk cache today; the stable-layout
  work in the packer reduces it for both.

## Tradeoffs

- **The pool is one failure domain per node.** A full pool stalls every
  session on the node. Mitigated by the heartbeat free-space fact, the
  placement gate, and 8 GiB of metadata. Today's equivalent is the
  node's stateful partition filling with dirty files, which has no gate
  at all.
- **Copy-on-first-write is a per-block tax.** A small write into a
  shared block costs about 1.5 ms once, after which the block is private
  and runs at disk speed. Today the same first touch copies 16 MiB. A
  workload that does many small synchronous writes scattered across a
  cold base (a database writing into never-written files) pays about
  2 ms per commit until its blocks go private; this is to be measured in
  a real session during phase 3.
- **No discard from the guest.** The Firecracker fork does not
  advertise `VIRTIO_BLK_F_DISCARD`, so space the guest frees is
  reclaimed only when a volume is deleted. This is true today as well. A
  fork patch mapping discard to the device is a contained follow-up and
  becomes worthwhile under this design because the pool then reclaims
  the blocks.
- **Metadata is a B-tree on a partition, not files.** Inspection and
  repair use `thin_check`, `thin_dump` and `thin_repair`. `thin_check`
  on a 750 GiB pool takes about 4 s. Operators lose `ls` on a dirty
  file and gain `dmsetup status`.
- **Staging precedes the first boot on a node.** The prefetch
  supervisor already downloads and pins the full base before marking an
  image ready, so this is the same cost moved from a cache directory to
  a volume. It is listed here because it is now structural: there is no
  lazy fallback.

## Testing

- **Unit (host-agent, Linux-gated):** the `dmsetup` command builder and
  the `thin_delta` XML parser, against recorded fixtures; the thin id
  counter's crash states (ADR 0099 H5: every torn write of the counter
  file is externally constructed).
- **Firecracker integration (KVM runners, `ci.yml`):** create → write →
  checkpoint → verify the manifest's chunks equal the guest's bytes;
  checkpoint under a guest writing at full rate, assert the pause is
  bounded; host-agent restart with a running guest, assert zero disk
  calls and zero lost writes; resume on a second pool from the
  manifest, assert equality. Each sized to its property.
- **Simulation (engram-sim):** the heartbeat no longer carries a
  quarantine field; the cosim oracle from the stalled ADR 0110
  retirement PR, "a host-agent roll produces zero evictions and zero
  rewinds", is adopted as a permanent invariant.
- **Production evidence for acceptance:** one week of deploys with the
  survivor-eviction and durability-rollback counters at zero, and the
  NBD write-ack histogram gone from the dashboard because the metric no
  longer exists.

## Rollout

Each phase is one PR on a worktree, each retires the code it replaces,
and each updates the DESIGN.md fact page. The chunk-store manifest is
the interchange format, so hosts on either side of any phase
interoperate: a session evicted from an NBD-era host resumes on a
thin-pool host and the reverse.

1. **Node substrate.** Data disk in the fleet module; node-prep builds
   and checks the pool; the host-agent heartbeat reports pool free
   space; the scheduler gates on it. No session uses the pool yet. Dev
   VM and CI get the loop-device pool.
2. **Image staging into volumes.** The prefetch supervisor stages into
   a thin volume beside the chunk cache; readiness requires both.
   `thin_delta`-based diff and the chunk hasher land here with their
   fixtures, exercised against staged volumes.
3. **Sessions on the pool.** New sessions and resumes create thin
   snapshots and start Firecracker on the device path; checkpoint,
   evict and teleport use the snapshot primitive. Existing NBD sessions
   finish their lives on the daemon. The small-synchronous-write
   measurement from Tradeoffs runs here on a real image.
4. **Retire the daemon.** Once no NBD-backed session exists, delete
   every row of "What this deletes", the chunk cache, the heartbeat
   field, the coordinator consumer and the quarantine evictor flavour.
   Flip ADR 0110, ADR 0090 Decision 3 and ADR 0049 to superseded, write
   the fact page, flip this ADR to Accepted with the commit chain.

## Facts this ADR establishes

These sentences become the DESIGN.md fact page at phase 4. Each is
guarded by a test or a metric named beside it when it lands.

- A session's disk is a thin volume the kernel owns; no userspace
  process is in the data path.
- A host-agent restart does not open, reconfigure or verify any
  session disk. (`host_agent_restart_makes_no_disk_calls`)
- An acknowledged guest write is in the host page cache or the pool,
  never in host-agent memory. (`acked_writes_survive_host_agent_death`)
- A checkpoint pauses the guest for the page-cache sync plus about
  20 ms, independent of how much the session has written.
  (`checkpoint_pause_is_bounded_under_full_rate_writes`)
- The base image is fully staged on a node before the image is ready
  there. (`image_is_ready_only_when_its_volume_is_complete`)
- Placement never chooses a node whose pool cannot hold the session's
  disk reservation. (`scheduler_rejects_hosts_below_pool_low_water`)
- The chunk store manifest is the only cross-host representation of a
  disk; any host resumes any manifest.
  (`manifest_from_thin_host_resumes_on_any_host`)
- The heartbeat carries observed facts only; no field asks the
  coordinator to take an action.

## Relationship to other ADRs

- **ADR 0110** reached the right diagnosis and the wrong destination:
  it made the daemon's writes durable and kept the daemon. This ADR
  removes the daemon. Its "What this deletes" table is a subset of the
  one above.
- **ADR 0090 Decision 3** added the quarantine channel as a reaction to
  a failed handover. Decisions 1, 2, 4, 5 and 6 of that ADR stand.
- **ADR 0049** allocated NBD slots. There are no slots.
- **ADR 0036** stays the authority on what an image artifact is and
  when a host is ready; only the staging sink changes.
- **ADR 0079 / 0116** stay the authority on ownership. This ADR is the
  last step of the direction ADR 0116 set: "ownership is granted and
  released, never inferred" now holds for the disk too, because the
  disk no longer has a process to infer anything from.
