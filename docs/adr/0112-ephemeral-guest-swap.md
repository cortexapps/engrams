# ADR 0112: Ephemeral guest swap — a degradation band for anonymous memory

Status: Proposed (2026-08-06)

**Related:** ADR 0025 (we own the guest kernel — `CONFIG_SWAP=y` is already
in it), ADR 0027/0055 (the aux-drive attach machinery this reuses), ADR 0028
(the memory/disk coherence rule this must not break), ADR 0046/0048 (what
placement reserves today — memory and vCPU, not disk), ADR 0049 (why we do
not spend a second NBD slot per guest), ADR 0070 (the chunk-cache disk budget
the swap file shares a filesystem with), ADR 0101 (checkpoint cadence — why
`swapoff` cost must be bounded), ADR 0110 (dirty files on node NVMe; this ADR
closes its open disk-accounting gap).

Terms used in this document:

- **Guest**: the VM a user's session runs in.
- **Anonymous memory**: process memory with no file behind it — heaps,
  stacks. The JVM heaps in a dev session are anonymous.
- **File-backed memory**: memory that mirrors a file. The kernel can drop it
  and read it back later ("refault").
- **Swap**: a block device where the kernel can park anonymous pages. With
  no swap, anonymous memory can never be evicted, at any price.
- **Swap PTE**: the page-table marker the kernel leaves behind when it swaps
  a page out. It points at a slot on the swap device.
- **Reclaim**: the kernel freeing memory to satisfy new allocations.
  `kswapd` does it in the background; "direct reclaim" is an allocating
  process forced to do it inline (a stall).
- **Capture**: our snapshot of a guest's memory + disk (idle eviction,
  periodic checkpoint, base bake — all run the same code).
- **Work dir**: the host's dedicated NVMe filesystem (`/var/lib/engram`,
  prod: 2×375 GB RAID-0), home of the chunk cache, dirty files, and jails.

## Summary, in plain English

A machine with no swap does not degrade under memory pressure. It falls off
a cliff. Anonymous memory — the JVM heaps that dominate a dev session —
cannot be evicted without a swap device, so when it outgrows RAM the kernel
attacks the only thing it can evict: the file pages that hold the very code
and data the workload is executing. The guest thrashes, then the OOM killer
shoots processes. Our guests have zero swap, so this is exactly what they
do. We measured it: a 24 GiB guest hit 81 MB free, took two OOM kills, and
turned an 8-second compile into a 23-minute one that still exited 0.

The fix is to give each guest a modest swap device, so the kernel can park
cold anonymous pages instead of eating hot file pages. The device is a plain
sparse file on the host's NVMe, attached to Firecracker as a second disk. It
is deliberately a dead end: its contents are never uploaded, never appear in
any manifest, and are discarded at every capture. Before we snapshot a
guest's memory, the guest runs `swapoff`, so no memory image ever references
swap contents that will not exist at restore. A fresh, empty swap file
appears at every resume.

Swap contents are the most-written, least-valuable bytes in the system.
Persisting them through chunk upload and dedup would be paying our most
expensive machinery to preserve data whose only purpose is to be forgotten.

## Context

### The measured incident

Session f660f022 (2026-08-06), a 24 GiB / 8 vCPU dev-brain guest running the
tilt stack plus one real `:web:compileKotlin` after a one-file edit:

| Signal | Value |
|---|---|
| MemAvailable low-water | **81 MB** of 24,056 MB |
| Kernel OOM kills | **2** |
| One-file compile | **1372 s** vs 8.45 s healthy baseline (**162×**), rc=0 |
| Load average | 152 on 8 vCPUs |
| `pgmajfault` | 5,404,438 |
| `pgscan_direct` : `pgsteal_direct` | 2,050,264,209 : 91,097,158 (**22:1**) |
| `pswpin` / swap total | 0 / 0 — **the guest has no swap** |

The 22:1 scan-to-steal ratio is the signature: the kernel scanned two
billion pages and found almost nothing it was allowed to evict, because the
working set is mostly anonymous and anonymous memory without swap is
unreclaimable. All pressure landed on file-backed pages, which live on the
chunked-NBD rootfs — every refault paid a device round trip.

Two complementary fixes, one already shipped:

- **PR #1044 (merged)** fixed the file-refault *cost*: ranged pread instead
  of whole-chunk materialize, 2.5 ms → 28 µs p50 at the backend.
- **This ADR** fixes anonymous memory being *unreclaimable*. Faster reads
  alone cannot prevent the OOM kills; there was nothing left to reclaim.

Heap-capping was tried first (engrams-internal #135) and measured
insufficient: low-water 84 MB vs the 81 MB stock baseline, and the compile
still failed. Per-JVM RSS runs ~1.1 GB above `-Xmx` and a second Gradle
daemon forks under contention, so waste scales with daemon count, not size.
Capping rations a fixed pool; it does not change the failure shape.

### Why the guest kernel makes this cheap

We own the guest kernel config (ADR 0025). `CONFIG_SWAP=y` on both
architectures (`deploy/kernel/microvm-kernel-ci-x86_64-6.1.config`,
`…-aarch64-6.1.config`). This is a pure userspace change: no kernel roll,
no fleet-wide forced re-capture.

## Decision

### D1. Attachment: a raw sparse file, directly attached — not an NBD/chunked disk

The swap device is a per-sandbox sparse file on the work dir, `ftruncate`d
to `swap_mib` and attached to Firecracker as a second read-write virtio-blk
drive (`drive_id: "swap"`), beside the aux-drive loop
(`engram-sandbox-firecracker/src/lib.rs:2511`). The file path goes through a
per-sandbox canonical symlink, exactly like the rootfs canonical
(`lib.rs:2469`), so the path embedded in `state.bin` re-anchors on any host.

The device must exist in the base snapshot's device model — FC restore only
re-points drives that are already in `state.bin`. So the drive is attached
at base-snapshot capture, and each restore:

1. ensures a placeholder file exists at the embedded canonical path before
   `load_snapshot` (under the existing ADR 0048 per-source-path lock,
   `lib.rs:2960`), then
2. `patch_drive`s the `"swap"` drive to a fresh per-sandbox sparse file in
   the paused window, beside the aux swap plan (`lib.rs:3494`).

A failed swap `patch_drive` fails the restore. Resuming a guest onto a
wrong-contents swap device is the corruption case; a failed restore is
redrive-safe.

**Reclamation**: unlink the backing file right after FC holds the fd
(verify on the dev VM that FC never re-opens a drive by path after attach;
the snapshot path does not). The bytes then live only in an anonymous inode
the kernel reclaims the instant FC exits — no orphan files, no sweeper, and
the smallest possible at-rest window (see D6). If the probe fails, fall back
to unlink-at-destroy plus a sandbox-id-keyed orphan sweep.

**Why not a chunked disk on the NBD path.** An earlier draft of this work
directed exactly that. It is the wrong half of the storage machinery:

- **Redeploy corruption.** The FC process outlives host-agent redeploys
  (~11/day, ADR 0110); NBD daemons do not. A swap NBD disk whose dirty
  overlay is `remove_on_drop` silently destroys swapped-out pages under a
  live guest that still holds swap PTEs — the unfixed #739 pod-roll survivor
  class, recreated for data we never wanted durable. Avoiding it would mean
  giving swap a persistent dirty file and teaching the rehydrate path to
  re-serve it: durability machinery for a disk defined by not needing any.
  A raw file is served by the kernel; host-agent death cannot touch it.
- **Slot density.** A second NBD slot per sandbox halves per-host slot
  capacity. ADR 0049 exists because slot exhaustion wedged the fleet.
- **Blast radius.** `nbd_sandboxes` is one-entry-per-sandbox, and that
  cardinality is load-bearing at five sites: the capture drain and its
  RefuseUntracked guard (`pooled_backend.rs:2599`), the SIGTERM flush
  (`:4403`), abandon (`:4676`), the spool (`:4710`), and rehydrate
  (`:9888`). The guard's whole purpose — never drop acked writes — is the
  inverse of swap's contract (always drop). Ephemeral-by-construction beats
  ephemeral-by-exclusion at five sites.
- **Latency.** Post-#1044 a swap-in over NBD is ~120–135 µs (daemon round
  trip + virt floor). The raw file is a host page-cache or direct NVMe read,
  and it queues behind nothing when reclaim storms hit.

The chunk data plane exists to provide durability, dedup, upload, and
cross-host restore. Swap must have none of those. What this design *does*
reuse is the drive machinery: staged file + `put_drive` at capture +
`patch_drive` in the paused window + canonical-path re-anchoring — the
ADR 0027/0055 aux-drive and rootfs patterns, unchanged.

**Device ceiling.** We boot `pci=off`; the virtio-mmio GSI pool is 19 lines
minus rootfs, net, vsock, and the 12 reserved dyn slots
(`engram-core/src/types/sandbox.rs:170-184`). The swap drive fits the
arithmetic, but the comment's own rule stands: the exact ceiling is pinned
by a dev-VM probe (boot/capture/restore with rootfs + net + vsock + 12 dyn
+ swap) before any base re-bake.

### D2. Guest arming: agentd owns mkswap/swapon and the VM sysctls

A new `crates/engram-agentd/src/swap.rs`:

- **Device selection**: the swap disk is the only *writable* non-`vda`
  virtio disk — every aux drive attaches `is_read_only: true`
  (`lib.rs:2529`). Probe `/sys/block/vd*/ro == 0`. No serials exist in our
  device model and none are needed.
- **Arm** (`mkswap` + `swapon`) at cold boot (beside
  `apply_block_readahead()`, `main.rs:344`) and at session bind (beside the
  `remount_and_log` call sites, `handler.rs:318` and
  `harness_supervisor.rs:160`) — bind is where a restored guest meets its
  fresh, empty device.
- **Sysctls** at cold boot, so the base snapshot freezes them (the same
  ride `read_ahead_kb` takes today):
  - `vm.swappiness = 100`. Kernel 6.1 range is 0–200; 100 means "anonymous
    and file reclaim cost the same". Post-#1044 that is approximately true
    (28 µs ranged pread vs a page-cache/NVMe swap-in), and the measured
    pathology was the default 60 biasing reclaim *away* from the one thing
    that needed evicting. Not higher: no evidence swap-in is cheaper, and
    over-swapping lengthens `swapoff` at capture (D3).
  - `vm.page-cluster = 0`. Swap readahead is 2^n pages; swap-in here is
    random 4 KiB against fast backing. Cluster reads only amplify I/O
    during exactly the storms we are trying to survive.
  - `vm.watermark_scale_factor = 125` (default 10). The 22:1 direct-reclaim
    ratio says kswapd woke far too late; allocators were reclaiming inline.
    `min_free_kbytes` stays untouched (it changes OOM behavior; separate
    experiment).
- All best-effort, log-and-continue — the `tuning.rs` contract: tuning must
  never block agent bring-up.
- **Readahead carve-out**: `tuning.rs::apply_block_readahead` currently
  writes 4096 to every `vd*`. It must skip the swap device (write 128
  instead). The 4 MiB window is sized for serial classloading chains on the
  rootfs; #1044's profiling showed large readahead inverts under reclaim
  pressure, and the swap device lives entirely under reclaim pressure.
- **Kill switch**: an env flag on the RefreshAgent payload
  (`ENGRAM_GUEST_SWAP=off`) makes agentd never arm. It reaches every
  session through an agentd roll, with no re-capture; the device stays
  attached and unused, which is safe.

### D3. Capture protocol: no restorable memory image ever contains swap PTEs

This is the ADR's core invariant. A restored memory image that holds live
swap state — a registered swap device and PTEs pointing into it — paired
with a fresh zeroed device returns garbage on first swap-in: silent memory
corruption in whatever was swapped out. This is ADR 0028 Defect B's shape,
but worse: no filesystem journal stands between the kernel and the damage.

Therefore, in `capture_phase` (`pooled_backend.rs:2488`) — the single
coherence cut shared by every capture flavor — the guest runs `swapoff`
**before** the pause, via a `swap_disarm` exec helper modeled on
`sync_guest_fs` (`:1141`). `swapon` re-arms only after `inner.snapshot`
returns. No new swap PTEs can appear in the disarm→pause window, so every
snapshot in the lineage restores safely onto an empty device. The disarm
runs while the guest is live: it extends capture wall-time, never the
guest-visible pause.

`swapoff` pages everything back into RAM, which is unbounded and can fail
`ENOMEM` on an overcommitted guest. The helper is therefore a guard, not a
prayer. It reads `/proc/meminfo` and returns structured
`Disarmed { paged_in_kb }` or `Refused { swap_used_kb, mem_available_kb }`:

- **Refuse when `SwapUsed > MemAvailable − margin`** — `swapoff` would
  fail or trigger the OOM killer.
- **Periodic checkpoints additionally refuse above ~256 MiB of used swap.**
  ADR 0101's cadence floor is 30 s; paging gigabytes back in every tick,
  only for the kernel to re-evict them, is pure thrash.

Degradation on `Refused` is the existing recovery ladder, not new
machinery:

- **Periodic checkpoint** → a disk-only checkpoint (fsync, wait_idle,
  flush_local still run; the FC memory snapshot is skipped and the record
  says so). Recovery from it is the existing ADR 0028 rung-2 cold disk
  boot. A counter tracks it; sustained degradation is the operator signal
  "this image is under-sized — raise `suggested_memory_mib`".
- **Terminal flavors** (idle-evict, drain, SIGTERM, base bake) → disk-only
  eviction: sync, disk capture, destroy. The session stays parkable and
  loses process state, never disk state. Today the same guest would be
  OOM-killing instead.

In the healthy steady state swap usage is ≈0 and the disarm is a
no-op-priced exec round trip. Mid-residence re-arm is `swapon` alone (the
signature survives `swapoff`); a fresh post-restore device gets the full
`mkswap` at bind (D2). Re-arm failure leaves the guest running swapless —
safe, logged, counted.

### D4. Sizing: an image hint, resolved like memory

`ImageConfig::resolved_swap_mib()` sits beside `resolved_memory_mib()`
(`engram-core/src/types/image.rs:241`) and obeys the same contract: capture
and restore MUST compute it identically, because the device geometry is
frozen in `state.bin`. Changing it takes effect on re-capture, like memory
(the ADR 0093 note on `suggested_disk_gib` applies unchanged).

- Opt-in: `ResourceHints.suggested_swap_mib`. Absent or 0 ⇒ no swap —
  nothing changes for any existing image.
- Default when opted in: `clamp(resolved_memory_mib() / 4, 1024, 8192)`.
  dev-brain (24 GiB) → 6 GiB. RAM/4 bounds three things at once: the
  worst-case `swapoff` page-back-in (D3), the worst-case host disk
  footprint (D5), and the degree of overcommit we are willing to paper
  over before the honest fix (a bigger guest) is forced.

Swap is a per-image property, not per-session — the ADR 0055 rule for
memory applies verbatim: the base snapshot is sized once per image.

### D5. Host disk bookkeeping: attribute, reserve, and close ADR 0110's gap

The swap file lands on the work dir — the same filesystem as the chunk
cache, ADR 0110's dirty files, memfiles, and FC staging. What exists today:

- The heartbeat's `util_disk_{total,used}_mib` is a single `statvfs` of the
  work dir (`host-agent/src/util.rs:130-183`): swap blocks appear
  automatically, attributed to nothing.
- The coordinator's two disk gates are both *brakes*, not reclaimers: the
  20 GiB placement floor (`placement.rs:1217`) vetoes new placement, and
  the idle detector's disk hold *pauses eviction*. A full host quiesces; it
  does not recover. The kubelet cannot help — the work dir is a dedicated
  mount, outside its eviction fs (ADR 0070: "the budget is the real
  mechanism; the volume is defense-in-depth").
- The chunk-cache evictor does watch real fs free space every sweep
  (`chunk-store/src/cache.rs:2157`) and evicts harder when foreign files
  eat the mount — but it can only free what the cache holds, one 60 s sweep
  behind.
- **ADR 0110's dirty files have no accounting at all** — no budget, no size
  gauge, no heartbeat term; reaping is startup-only. That ADR explicitly
  deferred the problem to rollout watching. This ADR is a second consumer
  moving in; mirroring the gap would compound it, so we close it instead.

Decisions:

1. **Hard per-sandbox cap by construction.** The file is `ftruncate`d to
   `swap_mib` and that IS the virtio device size. A guest cannot allocate
   past it. Worst case per host: Σ `swap_mib` ≤ Σ `mem_budget_mib` / 4.
2. **Attribution via the memfile pattern.** Extend the existing `st_blocks`
   walk (`image_prefetch.rs:791`, `allocated_bytes`) to cover the swap dir
   AND the dirty root in one pass; emit gauges beside
   `HOST_BASE_MEMFILE_BYTES`.
3. **The co-tenant reserve becomes multi-source.**
   `ChunkCache::set_co_tenant_reserved` (`cache.rs:743`) is a single scalar
   with exactly one unconditional writer today (memfiles); a second `set`
   caller would clobber the first. Convert it to summed named sources
   (memfiles + swap + dirty), then feed swap and dirty allocated bytes in,
   so the cache budget shrinks honestly instead of discovering pressure via
   the floor arm a sweep late.
4. **An admission-side committed term.** The heartbeat gains
   `committed_swap_mib` (Σ `swap_mib` over live sandboxes), and
   `host_disk_floor_ok` subtracts it from free space before the floor test.
   Placement becomes reservation-safe — it prevents the overcommit rather
   than observing it after the blocks are gone. No new PG budget dimension
   or queue-partition key in v1; the floor-with-committed-term is the
   honest middle.
5. **Sweeping.** With unlink-after-attach there are no swap files to sweep.
   If the fallback path is needed, `sweep_dirty_root_at` extends to swap
   files — and becomes periodic rather than startup-only, which benefits
   the dirty files equally.

### D6. Security: ephemeral is the mechanism, stated honestly

Swap bytes are guest memory in plaintext on host NVMe. That crosses the
same trust boundary the rootfs dirty overlay already crosses — but it is
worse in kind, not just in degree: anonymous memory holds secrets that
never touch a filesystem (env vars, in-heap key material).

What this design buys:

- **Never uploaded.** The dirty overlay's chunks do reach GCS; swap bytes
  never leave the host.
- **Fresh per residence.** Every restore gets a new zeroed file; no swap
  byte survives a park/resume cycle.
- **No at-rest artifact.** With unlink-after-attach the bytes live in an
  anonymous inode that vanishes when FC exits — nothing to find on a
  decommissioned disk beyond what the overlay already leaves.
- 0600/root in a per-sandbox directory while a path exists at all.

**Encrypted swap is impossible in v1**: the guest kernel has no device
mapper at all (`CONFIG_MD is not set` — no `CONFIG_BLK_DEV_DM`, no
`CONFIG_DM_CRYPT`), so there is nothing for `cryptsetup` to stack on.
The AES/XTS crypto core is already compiled in. Follow-up recorded for the
next ADR 0025 kernel roll: add `CONFIG_BLK_DEV_DM` + `CONFIG_DM_CRYPT`;
agentd then arms `cryptsetup plain` with a random ephemeral key before
`mkswap` — a pure agentd change once the kernel lands, and the key dies
with the guest by construction.

### D7. Teleport: incompatible in v1, guarded

C2 post-copy teleport never pauses the guest long enough to run `swapoff`,
and moving swap's sealed chunks over the peer channel would persist exactly
the bytes this ADR promises never to persist. There is no discard-and-refill
seam in a live move — the guest keeps its swap PTEs across the wire.

v1: the teleport path (`coordinator/src/live_migration.rs`) checks
`resolved_swap_mib() > 0` and falls back to snapshot-rehome, which runs
`capture_phase` and therefore the D3 disarm — already correct.
`ENGRAM_LIVE_TELEPORT` has never been enabled in production and the teleport
program is deferred; this is one `if`, a test, and this paragraph. Revisit
if the program revives.

### D8. Placement, memory ledger, and what does NOT change

Memory reservation (ADR 0046) is untouched: swap does not let us shrink
`mem_budget_mib`, because the budget must still cover the guest's RAM at
restore. Swap changes what happens *inside* the guest when its workload
exceeds its RAM — it does not change what the host promises the guest.
vCPU (ADR 0048) unchanged. Disk stays out of the 2D packing key (D5.4 is a
floor term, not a dimension).

## Lifecycle coverage

Every leg of the sandbox lifecycle, and what the swap design does there.
"No gaps" is a review requirement for this table.

| Leg | Behavior |
|---|---|
| Fresh create (fork from base) | `patch_drive` a fresh sparse file in the paused window; agentd arms at bind (`mkswap` + `swapon`) |
| Cold boot / rung-2 disk-only recovery | agentd `run()` executes → arms naturally |
| Parked-paused rung (pause in place, no capture) | No memory image is recorded → swap stays armed; same host, same FC process, file intact |
| Periodic checkpoint (ADR 0101) | disarm → capture → re-arm; `Refused` → disk-only checkpoint + counter |
| Idle eviction / drain / SIGTERM capture | disarm before pause; `Refused` → disk-only eviction; destroy reclaims the file |
| Base-snapshot bake | Device attached at capture; the same `capture_phase` disarm guarantees the base image holds no swap PTEs; no swap manifest exists to be forked or GC-pinned |
| Host-agent redeploy (reattach, never drain) | FC holds the file fd; no daemon in the path — unaffected by construction |
| Host death / evacuation | The local file dies with the host; all recovery keys off PG manifest refs, and swap is in none of them; the new host creates a fresh file at restore |
| Teleport | Guarded → snapshot-rehome fallback (D7) |
| Destroy | File already unlinked (or unlink-at-destroy in the fallback) |
| Crash orphans | None with unlink-after-attach; else the sandbox-id-keyed sweep |
| GC / pin sets / sim conformance | Swap appears in no manifest, no PG row, no PinSet — zero contact with the durability plane |
| Concurrent same-base restores | Placeholder creation under the existing ADR 0048 per-source-path lock |
| VZ (macOS) | Same mechanism — sparse file + RW virtio-blk; `capture_phase` and agentd logic are shared; parity phase (production drives the design) |

## Alternatives considered

- **A chunked disk on the NBD path, blank manifest, no flush scheduler.**
  The originally-directed shape. Rejected in D1: redeploy corruption (#739
  class), slot-density halving (ADR 0049), five exclusion sites, slower
  under load. The blank-manifest + flushless-attach primitives it would
  have used do exist (`manifest.rs:302`, `runtime.rs:747`) — the machinery
  is not the objection; what the machinery is *for* is.
- **A separate host-NVMe scratch tier.** Rejected before this ADR: it buys
  ~1.3× swap-in latency in exchange for owning snapshot/park/resume,
  teleport, placement, and secret handling as new problems. (The raw-file
  design is not a tier; it is one file on storage the platform already
  owns and already budgets.)
- **zswap.** `CONFIG_ZSWAP=y`, default-off, is already in the guest kernel
  — a reviewer grepping the config will find it. zswap is a compressed
  cache *in front of* a real swap device: it requires the device this ADR
  creates, so it is a potential future addition, never an alternative. Its
  pool also steals guest RAM, and its writeback still lands on the device.
  Cheap post-v1 experiment (`zswap.enabled=1` on the cmdline).
- **zram.** Not compiled (`CONFIG_ZRAM` absent) — adding it is a kernel
  roll plus fleet-wide re-capture. It also spends RAM to fake RAM: the
  incident is anonymous demand exceeding RAM, which compression defers
  rather than absorbs, and a zram device's contents would bloat every
  memory snapshot we capture.
- **Heap capping.** Tried (engrams-internal #135), measured insufficient
  (low-water 84 MB vs 81 MB stock; compile still failed). RSS overhead
  scales with JVM count, not size; capping cannot change the failure shape.
- **Bigger guests.** Honest but orthogonal: 24 GiB was disproven as a
  reasonable dev-brain size and the image hint should rise — but every
  size has a cliff one workload away as long as anonymous memory is
  unreclaimable. Swap converts the cliff into a slope at every size.
- **MGLRU** (`CONFIG_LRU_GEN=y`, default-off): a runtime-enableable reclaim
  improvement worth a canary, not a substitute — it improves *which* pages
  get evicted, not *whether* anonymous pages can be.

## Consequences

- Guests gain a degradation band: under overcommit they get slow instead of
  OOM-killed, and the JVM daemons stop being shot mid-compile.
- A new durability trade, stated plainly: a guest deep in swap refuses
  memory capture and degrades to disk-only checkpoints/evictions (rung-2
  recovery — process state is lost on the next resume). Today that same
  guest is OOM-killing; staler warm recovery replaces live-process death.
  The degradation counters make a chronically-refusing image an operator
  signal, not a mystery.
- Capture wall-time grows by the `swapoff` page-back-in when swap is in
  use (bounded by D3's thresholds and D4's size cap); the guest-visible
  pause is unchanged.
- Guest plaintext lands on host NVMe with a shorter lifetime than the
  existing dirty overlay, but a wider kind of content (D6). dm-crypt closes
  this at the next kernel roll.
- Host disk gains a bounded, attributed, admission-reserved consumer — and
  ADR 0110's dirty files gain the same attribution for free (D5).
- Residual risks named: the allocated-vs-committed gap (placement reserves
  the worst case; real growth is workload-paced), and the disk-pressure
  spiral (low disk still pauses eviction; the committed term prevents the
  overcommit that feeds it, it does not add a reclaimer).
- Every opted-in image needs one base re-capture to gain the device. Old
  snapshots restore untouched (all new fields are serde-defaulted `None`).
- One more virtio-mmio device per guest, against a ceiling we re-probe
  before the re-bake (D1).

## Non-goals

- Encrypted swap (kernel roll prerequisite; recorded follow-up in D6).
- Teleporting swap-armed guests (D7).
- A disk dimension in 2D packing / the queue-partition key (D5.4).
- zswap/zram/MGLRU tuning (candidate follow-up experiments).
- Shrinking `mem_budget_mib` because swap exists (D8).
- Fixing the host page-cache double-caching over `/dev/nbdN` (a separate
  host-side memory win noted during #1044 profiling — same investigation,
  different change).

## Implementation

One PR per phase, in order. Each phase leaves the system shippable with
swap resolved to "off" everywhere until Phase 5's canary.

1. **Types + plumbing, no behavior.** `ResourceHints.suggested_swap_mib` +
   `resolved_swap_mib()`; `SandboxSpec.swap_mib`;
   `SnapshotMetadata.swap_size_mib` (all serde-defaulted); coordinator
   plumb-through (`cold_boot_spec`, boot bundle, enabled-images, capture
   jobs). Serde round-trips including old-payload decode; sizing unit
   tests. Conformance suite (ADR 0098 D4) in the same PR if any
   `MetadataStore` surface moves.
2. **FC device lifecycle.** Sparse create + canonical symlink + `put_drive`
   at cold create; placeholder-before-`load_snapshot` under the 0048 lock;
   `patch_drive` in the paused window; unlink-after-attach (dev-VM probe
   first). FC integration test (boot sees a writable non-vda `vd*` of the
   right size; snapshot → restore → device is fresh; a no-swap snapshot is
   unaffected), wired into `ci.yml`'s `--test` list, minimally sized. The
   GSI ceiling probe gates the phase.
3. **agentd arming + tuning.** `swap.rs` (probe, `mkswap`/`swapon`,
   sysctls); the `tuning.rs` readahead carve-out; boot + bind arm sites;
   the RefreshAgent kill switch. Device-selection tests over a fake sysfs
   tree; the FC test grows `/proc/swaps` assertions post-bind and
   post-restore.
4. **Capture protocol.** `swap_disarm`/`swap_rearm` beside `sync_guest_fs`;
   the pre-pause call in `capture_phase` for every memory-capturing flavor;
   post-snapshot re-arm; disk-only degradation paths + counters. Wire-proto
   changes respect the append-only rule and the golden test. FC test:
   capture a swap-armed guest → restore → integrity; forced `Refused` →
   disk-only record → rung-2 recovery. Wired into `ci.yml`.
5. **Disk bookkeeping + guards + rollout.** The D5 walk/gauges/multi-source
   reserve/`committed_swap_mib` floor term; the D7 teleport guard; alert
   rules. Canary: set the hint on dev-brain, re-capture, re-run the
   f660f022 workload — assert zero OOM kills, MemAvailable low-water far
   above 81 MB, the one-file compile within ~2× of the 8.45 s baseline, and
   ≈0 disarm refusals. (Compare `host_id` before and after any timed run;
   fleet drains silently inflate elapsed time.) Then widen image-by-image
   and flip this ADR to Accepted with the commit chain.
6. **VZ parity.** Sparse RW swap disk in the VZ backend (near-identical
   mechanism); agentd and capture logic are shared already. `just vz-test`
   coverage mirroring the FC test.

## Implementation notes (divergences and accepted risks, 2026-08-06)

The phases above landed as PRs #1050–#1055. Divergences from the
proposal, plus the accepted risks an adversarial review made explicit —
appended per the bookend norm, the original body stands.

1. **No `SnapshotMetadata.swap_size_mib`.** The FC sidecar persists the
   full `SandboxSpec`, so restore reads the recorded `swap_mib` —
   capture and restore cannot skew, and a second copy would only be a
   disagreement channel.
2. **Restore re-points the drive by symlink-under-lock** (the rootfs
   ADR 0048 pattern), not `patch_drive` in the paused window. No
   pause-window work at all.
3. **Periodic refusal = skip + counter.** The continuously-flushed disk
   IS that tick's checkpoint; no new disk-only record type exists.
4. **Terminal refusal = typed error + requeue in v1.** Sustained
   refusal escalates through the existing eviction quarantine ladder to
   destroy + rung-2 disk recovery — the disk-only eviction by another
   road. A clean dedicated leg (drain disk, skip memory, record
   disk-only) is REQUIRED BEFORE BROAD ROLLOUT; the single-image canary
   accepts the ladder path. Terminal refusal needs an actively
   thrashing guest at eviction time, which idle-eviction targets
   rarely are.
5. **The disarm→pause window is an accepted risk within the guest-root
   trust model.** Between the pre-pause `swapoff` and the pause the
   guest runs live, and a root process inside it could re-arm swap and
   have swap PTEs captured — corrupting ITS OWN next restore. This is
   deliberate self-harm with blast radius confined to that guest, in
   the same class as the equally-available `dd of=/dev/vda` over its
   own rootfs lineage: guests hold root by design (dev-brain runs
   Docker in-guest, which needs CAP_SYS_ADMIN, so a capability drop is
   not available as a mitigation). No tenant or host boundary is
   involved. Accepted; not enforced.
6. **The teleport guard is HOST-side** (`migration_presetup` refuses
   with the existing `InvalidSpec` → snapshot-rehome semantics). The
   live sandbox spec is the authoritative swap source; an image row can
   drift after capture.
7. **D5's placement term is committed-aware, not prospective.** The
   floor subtracts the heartbeat-reported committed swap of EXISTING
   sandboxes; the incoming session's own `swap_mib` is not yet part of
   admission, and concurrent placements can stack within one heartbeat
   interval. Bounded (swap ≤ mem/4, memory is hard-reserved), but the
   original "reservation-safe" phrasing overstated v1. Prospective
   reservation (thread `resolved_swap_mib` through `ScheduleContext`)
   is the recorded follow-up, sized by canary overcommit data.
8. **Kill-switch semantics.** `ENGRAM_GUEST_SWAP=off` rides the
   spawn-delivered session/image env (built from the CURRENT config at
   bind time), reaching every session at its NEXT BIND with no
   re-capture — and actively disarming an armed guest. It is not
   instant: a session that never re-binds keeps swap until captured or
   re-bound. agentd's process env (frozen into the base snapshot) is a
   boot-time fallback only and cannot reach existing guests by itself.
9. **The disarm decision is pure** (`engram_host_core::plan_swap_disarm`
   beside `plan_capture_disk_drain` — ADR 0098 discipline), and the
   re-arm is cancellation-safe (a drop-guard fires the `swapon` on
   every capture exit, not just success).
