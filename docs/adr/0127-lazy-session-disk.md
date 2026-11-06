# ADR 0127: The lazy session disk — a restore boots before its disk is complete

- Status: Proposed
- Date: 2026-11-06
- Amends: ADR 0125 (the kernel-owned session disk), its Resume
  lifecycle ("the dirty chunks are written before the VM starts;
  nothing is faulted lazily from the chunk store on the disk path") and
  its Tradeoff "staging precedes the first boot on a node … there is no
  lazy fallback". Every other decision of ADR 0125 stands.
- Related: ADR 0007 (chunk store), ADR 0036 (image readiness and
  prestage), ADR 0039 / 0092 (lazy memory), ADR 0044 K2 (VM processes
  outlive the host-agent pod), ADR 0079 / 0116 (ownership), ADR 0098 /
  0099 (simulation and invariants), ADR 0123 (teleport).

## Terms

- **Source**: a read-only block device whose bytes are one published
  disk manifest (ADR 0007). It holds no writes.
- **Source process**: the userspace process that serves one source
  through ublk. One per sandbox while that sandbox copies.
- **dm-clone**: a device-mapper target that presents a writable copy of
  a read-only source device. Reads of a region it has not copied go to
  the source; writes go to the destination; a background task copies
  the remaining regions. A **region** is its unit of copy (64 KiB here,
  the default block size of a production pool). Its **bitmap** records
  the copied regions, on a metadata device.
- **Hydration**: the background copy, from the source into the
  destination, until every region is copied.
- **Cutover**: replacing the dm-clone table with a `linear` table onto
  the destination, under a device-mapper suspend. The device node and
  every open handle stay valid.
- **Eager restore**: ADR 0125's restore. Write every chunk the volume
  needs, then boot.
- **Lazy restore**: this ADR's restore. Boot over dm-clone, then hydrate.

## Summary, in plain English

ADR 0125 made the kernel the only owner of a session disk. It also made
a restore write the whole disk before the guest boots, unless the host
already holds the session's base image version. In production that is
the usual case, not the rare one: images are baked often, hosts stage
only the enabled version, and most idle sessions point at an older
version. A restore of such a session writes the full disk from the
chunk store inside the restore deadline. On a busy or new host it does
not finish, each retry starts again from zero, and the session becomes
dead after the retry limit.

The target for every session operation is under 10 seconds in the worst
case. No eager design meets that: a 26 GiB root disk at 1 GB/s is 26
seconds before any contention. Time to boot must not grow with disk
size.

After this change, a restore that cannot finish eagerly in about two
seconds boots over dm-clone at once. The guest reads uncopied regions
through a read-only source and writes into a thin volume as before.
Hydration copies the rest in the background. When it ends, the device
is cut over to a plain thin volume and the session is exactly an ADR
0125 session. The only userspace process in the path serves read-only
data, holds no writes, and exists only while its sandbox copies.

## Context

### What failed

The first production deploy of ADR 0125 had three properties that
combined:

1. A restore with no staged base for the session's image version wrote
   every chunk of the root and swap disks before boot. For a large
   development image that is tens of GiB.
2. The restore ran inside the restore RPC deadline (240 s). The deadline
   cancels the host's work, so a retry starts the copy again. Memory
   prefetch that the failed attempt spawned kept running.
3. The host that took the restore was new and was still staging images
   and bundles. It pulled far less than its network and disk could
   carry, and its chunk cache evicted chunks under the combined load.

Each cause can be fixed. None of the fixes gives a time to boot that is
independent of disk size.

### What ADR 0125 protects

ADR 0125's lesson: "a process that restarts several times a week must
not own a resource whose lifetime is a session." Its incident table has
three classes:

| Class | Failure | Root |
|---|---|---|
| Durability | acknowledged writes lost on a restart | userspace held write state |
| Liveness | a healthy VM killed after a failed reconfigure | the kernel device was bound to the old process, and recovery reacted |
| Event channel | a recovery event reached the coordinator before the recovery finished | a side channel beside ownership |

A lazy disk must not bring any of the three back. The design below
addresses each one explicitly.

### What the kernel can do (spike, 2026-11-05)

A spike on the Linux dev VM (kernel 6.8) measured dm-clone with a dm-thin
destination over a read-only source behind dm-delay, 2 GiB, region 64
KiB. Every sample includes about 4–5 ms of tool overhead.

| Measure | 80 ms source | 1 ms source |
|---|---|---|
| create to first read | 175 ms | 12 ms |
| read of an uncopied region | 91 ms | 3.4 ms |
| second read of the same region (reads do not copy) | 85 ms | 3.0 ms |
| 4 KiB write to an uncopied region (copy, then write) | 95 ms | 6.4 ms |
| 64 KiB write to an uncopied region (no copy) | 5.3 ms | 5.4 ms |
| 4 KiB write to a copied region | 4.8 ms | 4.7 ms |
| 4 KiB write + fsync, dm-clone / plain thin | 11.3 / 8.2 ms | 10.9 / 8.1 ms |
| hydration of 2 GiB | 49.6 s (41 MiB/s) | 6.2 s (332 MiB/s) |
| cutover to linear with a handle open | 6.4 ms | 5.9 ms |
| destination equals source plus every write | pass | pass |

`ublk_drv` loads on the same kernel from the distribution's extra
modules. The production node image (Container-Optimized OS, kernel
6.12) builds dm-clone into the kernel (`CONFIG_DM_CLONE=y`) and ships
`ublk_drv` as a signed module (`CONFIG_BLK_DEV_UBLK=m`) that is not
loaded until something loads it.

A first source on the same dev VM (`engram-disk-source`, libublk)
served a manifest's exact bytes and survived a `SIGKILL` mid-read: a
new process attached with user recovery, the kernel reissued the
queued reads, and no read failed. The same work found a kernel rule
the design must respect: stopping or deleting a ublk device that has
a request in flight and no live server to finish it blocks in the
kernel until the node reboots.

### Where the fetch time goes

A host-agent profile of a cold resume on the development cluster found
that the chunk fetch paths drive every fetch from one task: TLS,
response parsing, the hash check and the copies for every chunk run on
one core. Raising the number of requests in flight did not raise the
throughput; it raised the latency of each request. A production host
in the failure above pulled 100–220 MB/s with its CPU, network and disk
far below their limits, which is the same single-core bound. A fetch
path that spreads this work across cores is a precondition for any
estimate in Decision 1.

With one task per fetch, the same profile moves the limit off the
network. Memory chunks then arrive at about 1 GB/s, and the disk diff
is bound by writes: of 64 fetches in flight, on average 3 wait on the
store and the rest wait on direct writes into the thin volume. The
memory prefetch writes its chunks to the chunk cache on the same disk,
so a faster memory stream makes the disk diff slower (about 130 MB/s
into the volume while both run). The rates in Decision 1 are therefore
write rates of the host's disk while a memory restore runs, not
network rates, and the estimate counts the bytes of both streams.

## Decision

1. **A restore picks eager or lazy before it starts, and can fall back
   to lazy.** The host computes the bytes the volume needs from the
   manifests (`Manifest::delta_from` against the staged base when one
   exists) and splits them into local bytes (a staged volume or the
   chunk cache holds them) and remote bytes. The estimate is
   `local / local_rate + remote / remote_rate`, with rates the host
   measures from its own recent stages and a conservative floor; no
   rate is a constant. At or under the eager budget, the restore is
   eager. The budget starts at 2 s and is set from measurement once
   the fetch path uses every core. Over it, the restore is lazy. An eager restore that passes
   its budget continues as a lazy restore over the partly written
   volume: the eager path writes exactly the manifest's bytes, so
   hydration rewrites those regions with identical data and the result
   is the same volume. A wrong estimate costs time, never data. Every
   restore records the path, the estimate and the actual time.
2. **A lazy restore boots over dm-clone.** The destination is a thin
   volume: a snapshot of the staged base when there is one, otherwise a
   fresh volume. The bitmap is on a small thin volume of its own. The
   source is a ublk device. The guest's drive is the dm-clone device,
   made under `/dev/engram/` as ADR 0125 makes every drive. The guest
   boots as soon as the table is loaded.
3. **The source is read-only and stateless.** It serves one manifest.
   For each read it uses the first tier that holds the chunk: a staged
   volume whose chunk has the same hash, the chunk cache, a peer, then
   a ranged read from the chunk store followed by a fill of the whole
   chunk. Guest reads go before hydration reads. The fetch, the hash
   check and the copies of each chunk run in their own task, so the
   source uses every core and one slow fetch holds up only the reads
   that need its chunk. The source holds no
   writes; every byte it can serve is in the chunk store. Its state is
   a function of the manifest id.
4. **One source process per copying sandbox, outside the host-agent.**
   The host-agent starts it, moves it to the node's cgroup as it does
   the Firecracker processes (ADR 0044 K2), and
   records it in `sandbox.json`. It exits after the cutover. A
   host-agent restart does not touch it. A new build of the source
   applies to new copies only, so the source is never upgraded in
   place.
5. **A source failure stalls reads; it never ends a session.** The ublk
   device uses user recovery with reissue: when the process dies the
   kernel keeps the device and queues its I/O, and a new process
   attaches and the kernel reissues what was in flight. The host-agent
   restarts a dead source from `sandbox.json`. No code path kills,
   evicts or rewinds a VM because its source failed.

   A read the source cannot serve is retried with backoff while the
   failure is transient (the store is unreachable, slow or throttled).
   A definitive failure, a chunk the store reports as absent or whose
   bytes fail the hash check again after a fresh fetch, is data loss:
   the source returns an I/O error for that read and logs an error
   with a fixed message that alerting matches. Waiting cannot repair
   it, and a panic would only make recovery reissue the same read.

   A source is stopped only while it can finish every request it
   holds: the host-agent recovers a dead source before it stops it,
   and a stopping source first fails any read it cannot serve. The
   kernel waits for requests in flight when a ublk device stops; a
   request that nothing will finish blocks that stop, and every later
   device operation on the node, until the node reboots.
6. **Hydration belongs to the sandbox.** It is a host-owned job tied to
   the sandbox's lifetime: destroyed with the sandbox, resumed after a
   host-agent restart from the bitmap (which the kernel owns), never
   restarted from zero. The restore RPC returns when the VM runs, not
   when hydration ends. Memory prefetch is tied to the sandbox the same
   way.
7. **Cutover is the only exit.** When the bitmap shows every region
   copied, the host-agent suspends the device, loads a `linear` table
   onto the destination, resumes it, stops the source process, and
   deletes the bitmap volume. The session is then an ADR 0125 session
   in every respect.
8. **A checkpoint during hydration is correct without waiting.** Under
   the pause the checkpoint syncs the drive, which commits the bitmap
   with every region the guest wrote. It then suspends the drive, which
   stops the background copy, snapshots the bitmap volume and then the
   destination, and resumes the drive. The copy does not stop for the
   guest pause. With the bitmap snapshot taken first and the drive
   synced, the pair is consistent: the kernel records a region in the
   bitmap only after its copy is in the destination, so the later
   destination snapshot holds every region the bitmap snapshot records.
   The suspend makes the pair consistent in either order, and costs
   milliseconds inside the pause that already exists. A destination
   snapshot reads zeros in regions not yet
   copied, so the upload never reads it directly. It reads through a
   view: a second dm-clone table over the two snapshots and the same
   source, with hydration off and loaded read-only, which the kernel
   composes exactly as the guest saw it. The dm-thin root lane proves
   this on kernel 6.8 with the copy running and guest writes before and
   after the snapshots. The upload hashes every 16 MiB window in which the
   destination snapshot maps any block, and publishes the windows whose
   hash differs from the source manifest; every other window is the
   source manifest's chunk. Copied windows hash equal to the source, so
   they cost a read and a hash, not an upload. The view and both
   snapshots are deleted once the manifest is durable.
9. **The heartbeat carries a fact, not an event.** Per sandbox: whether
   it is hydrating. A host roll waits, within a bound, until no sandbox
   on the host is hydrating; past the bound it rolls and the sources
   survive it (Decision 4).
10. **Disk readiness stops gating placement.** A host that has not
    staged an image can take a session of that image: the first session
    is a lazy restore from the image manifest. Staging continues in the
    background and later sessions are eager. Memory readiness (ADR 0092)
    is unchanged.

## Design

### Objects on a node

```text
/dev/engram/engram-pool.sb-<sandbox>-root          the guest's drive: dm-clone while hydrating, then linear
/dev/engram/engram-pool.sb-<sandbox>-root-dest     the destination thin volume
/dev/engram/engram-pool.sb-<sandbox>-root-bitmap   the dm-clone metadata thin volume
/dev/engram/engram-pool.<view>                     a checkpoint's read-only view, while it uploads
/dev/ublkbN                                        the source, one per hydrating role
<work_dir>/<sandbox>/sandbox.json                  the volume names, the source manifest, the source process
```

Swap follows root: a role whose manifest is not local restores lazily
the same way.

`SessionDisk` carries the drive operations: `clone_drive`,
`open_clone`, `clone_state`, `enable_hydration`, `cut_over`,
`snapshot_clone`, `clone_view` and `remove_clone`. `FileDisk` (macOS and
the simulator) has no dm-clone. It hydrates eagerly: `clone_drive`
copies the source into the destination before it returns, so a drive
is hydrated from the start and `cut_over` only records the state.

A drive takes a bitmap that no drive has used (its first 4 KiB, where
the kernel writes the dm-clone superblock, reads as zeros), and refuses
a destination or bitmap that another drive or view uses. Operations on
one drive are serialized, and a suspend runs to its resume in a task
that a cancelled caller cannot stop.

The region size is part of the on-disk format. The bitmap's superblock
records it, and the kernel refuses a table over that bitmap with any
other size, so a change of `CLONE_REGION_BYTES` is a format change. A
view reads the region size from the superblock of its bitmap snapshot,
so it composes with whatever size the drive used.

### Lifecycles

**Lazy restore.** Create the destination and bitmap volumes. Start the
source process for the manifest and wait for its ublk device. Load the
dm-clone table (region 128 sectors, hydration enabled, threshold and
batch sized from the measured source rate). Record everything in
`sandbox.json`. Start Firecracker on the dm-clone device.

**Hydration.** The kernel copies. The host-agent reads the bitmap
counters from `dmsetup status` on each heartbeat and starts the cutover
when they are complete.

**Host-agent restart.** Opening the pool resumes every device of the
pool that a death between a suspend and its resume left suspended:
thins first, then the drives and views on them. Read `sandbox.json`.
For a hydrating sandbox, confirm the dm-clone device and the source
process. Restart the source if it is gone (Decision 5). A drive with no
live table is a `clone_drive` that did not finish: remove it and make
it again on a new bitmap. Nothing is copied again.

**Checkpoint, evict, destroy.** As ADR 0125, with Decision 8 for the
upload. Destroy also stops the source and deletes the bitmap.

**Teleport target.** The target boots on a lazy source for the moved
checkpoint instead of staging it first. The three-tier block move of
ADR 0123 is unchanged for the blocks the source wrote after the
checkpoint.

### Placement

The pool floor (ADR 0125) stays a hard gate. A lazy restore from an
image with no staged base cannot share blocks with a base, so it
consumes the volume's full size as it hydrates; placement counts the
manifest's non-zero bytes against the floor for a lazy restore.

## Tradeoffs

- **A userspace process is in the read path while a sandbox
  hydrates.** It is read-only and bounded in time, and its failure mode
  is a read stall. It is not in the write path at any time.
- **First reads during hydration cost the source's latency.** A GCS
  miss is tens of milliseconds for a ranged read. The page cache, the
  restored guest memory, the chunk cache and the read priority keep
  this to the first touch of each chunk.
- **A partial write into an uncopied region waits for one region
  copy.** Measured at one source read. A whole-region write does not
  wait.
- **An fsync during hydration commits the bitmap.** Measured at about 3
  ms more than a plain thin volume.
- **A lazy restore without a staged base uses the full volume in the
  pool.** It shares no blocks. An eager restore over a staged base
  shares them, which is one reason the eager path stays.
- **Two restore paths.** Decision 1 decides between them from manifests
  before the restore, and the fallback makes a wrong decision safe. A
  later change can unify them if seeding the bitmap from the base proves
  sound.
- **A ublk device can block a node.** A device stopped with a request
  that nothing will finish blocks in the kernel until reboot (Decision
  5 states the rule that prevents it). The rule is enforced in the
  source and in the host-agent, and tested.
- **dm-clone is less used than dm-thin.** It has been in mainline Linux
  since 5.4. The tests below cut every step, and the production node
  image is checked before any code depends on it.

## Testing

- **Unit (`engram-disk-source`):** the source serves a manifest's exact
  bytes for every offset and length; tier order; guest reads ahead of
  hydration reads; a zero chunk reads as zeros without a fetch.
- **dm-thin root lane (KVM runners, `ci.yml`):** the spike's checks as a
  test: reads, partial and whole-region writes, hydration, cutover with
  a handle open, and destination equals source plus every write. Cut at
  every step of the lazy restore and the cutover, then converge on the
  next pass to the same bytes, keeping every write of the earlier pass;
  a drive left with no table is removed and made again. A drive left
  suspended resumes when the pool is opened again and copies on to the
  cutover. Kill the source mid-hydration; the reads
  stall and complete after recovery with no error. Stop a source that
  holds a read it cannot serve; the read fails and the stop completes.
  A chunk the store reports absent fails its read with an I/O error; a
  store that is only slow does not.
- **Firecracker integration (KVM runners, `ci.yml`):** resume from a
  manifest with no staged base: the guest runs in seconds and its disk
  is correct; a checkpoint during hydration reproduces through `stage`;
  a host-agent restart during hydration loses no write and copies
  nothing again. Each sized to its property.
- **Decision 1:** the estimate picks eager for a small diff over a
  staged base and lazy for an unstaged image; a forced fallback from
  eager to lazy produces the same bytes.
- **Simulation (engram-sim):** the heartbeat's hydrating fact; the
  invariant that no source failure produces an eviction or a rewind.
- **Production evidence for acceptance:** a resume of an idle session on
  an image version no host stages, on a new host, boots in under 10 s;
  restore-deadline failures at zero for a week.

## Rollout

Each step is one PR and retires what it replaces.

0. **Gate.** The production node image has dm-clone built in and
   `ublk_drv` as a module (checked 2026-11-05). Node-prep loads
   `ublk_drv` once per boot beside `dm_thin_pool`, so no source process
   loads modules. Turn on host-agent tracing in production so restores
   can be measured. Make the chunk fetch paths spread their work across
   cores (Context: where the fetch time goes) and measure the eager
   rates Decision 1 uses.
1. **The source.** The `engram-disk-source` crate and binary: the ublk
   server over a manifest, with its tests.
2. **dm-clone in `engram-disk-thin`.** `SessionDisk` gains the clone,
   status and cutover operations, `FileDisk` gains their file
   equivalents for macOS and the simulator, with the root-lane tests.
3. **Lazy restore.** Decision 1 and the lazy path in
   `provision_for_restore`; hydration and cutover owned by the sandbox;
   the restore RPC returns at boot; memory prefetch tied to the sandbox.
4. **Placement and teleport.** Decision 10, the pool accounting for
   lazy restores, and the teleport target.
5. **Accept.** Write the facts into the DESIGN.md "Session disk" page
   and flip this ADR.
