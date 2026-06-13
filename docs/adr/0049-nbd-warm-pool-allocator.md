# ADR 0049: NBD slot allocator — large pool + warm front

**Status:** Proposed (2026-06-13). The ADR 0048 fleet load test wedged on a
host-local resource that was neither GCS, CPU, nor RAM: the **NBD `/dev/nbdN`
slot pool**. Prod ran a 16-slot pool scanned on the restore hot path; a
same-image burst (24+ concurrent restores on two nodes) exhausted it,
`acquire()` blocked holding each session's reservation, the queue couldn't
drain, and the fleet stalled at `queued=13 / free_mib≈760` with CPU at ~11%.
This ADR replaces the path-list allocator with E2B's `DevicePool` shape: a
bitset over `0..nbds_max` (default 64, dense fleets 4096) plus a background
populator that keeps 64 pre-validated slots **warm** for an O(1), syscall-free
`acquire()`.
**Related:** ADR 0007 Phase 4 (the original NBD daemon + path-list allocator),
ADR 0017 (kernel-busy probe), ADR 0044 K2 (survivor stale-binding sweep), ADR
0048 (the fleet load test that surfaced this).

## Context

Each FC sandbox restoring a chunked rootfs grabs one `/dev/nbdN` device for
the lifetime of its NBD daemon. The kernel exposes a fixed number of these,
sized by the `nbds_max` module parameter at `modprobe` time.

The original allocator ([`NbdSlotAllocator::from_paths`], ADR 0007) held an
explicit free-list of device paths from `ENGRAM_NBD_DEVICES` and, on every
`acquire()`:

1. popped a path, `stat`'d `/sys/block/nbdN/pid` to skip kernel-busy devices
   (ADR 0017), rotating busy ones to the back — O(slots) syscalls per acquire;
2. when every slot was busy, slept 500 ms and retried.

Two compounding problems under burst, both observed in the ADR 0048 load test
(2026-06-13, 30 same-image dev_vm sessions on two `kvm` nodes):

- **The pool was tiny.** OSS default was 4 devices; the prod override was 16
  (the burst logs only ever touched `/dev/nbd0–15`). A burst that *fits* a
  host's RAM still wants far more than 16 simultaneous restores.
- **Exhaustion blocks, and blocking holds reservations.** Once 16 slots were
  out, the 17th+ restore parked in the sleep-retry loop. Its session sat
  `created` (RAM reserved, never `Active`), so `free_mib` stayed pinned near
  zero and the placement queue couldn't drain. Leaked devices from an earlier
  buggy run (stale `/sys/block/nbdN/pid`) made it worse — they poisoned slots
  the pool could never reclaim. The fleet wedged: `queued=13` flat for the
  whole run, **host CPU ~11%** (idle — the work wasn't running, it was
  blocked).

GCS was never the bottleneck (the chunk cache single-flights + caches on
NVMe; the base image is fetched ~once per host). Neither was CPU or RAM. The
bottleneck was a software slot pool two orders of magnitude too small, scanned
on the hot path.

E2B solved the same problem in `packages/orchestrator/pkg/sandbox/nbd/pool.go`:
a `bitset` over `0..nbds_max` (they run `nbds_max=4096`) and a `Populate`
goroutine that keeps a channel of pre-validated free slots ready; `GetDevice`
is a channel receive with no hot-path syscall.

## Decision

Rebuild [`NbdSlotAllocator`] in the `DevicePool` shape:

- **Bitset universe, not a path list.** Slots are tracked by a reserved bit
  over `0..max`. `max = min(kernel nbds_max, ENGRAM_NBD_MAX_SLOTS, 4096)`.
  The hard ceiling (`MAX_SUPPORTED_SLOTS = 4096`) bounds the bitset.
- **Warm pool.** A background populator keeps up to `warm_target`
  (`ENGRAM_NBD_WARM_SLOTS`, default 64) **pre-validated** free slots in a
  queue. `acquire()` pops one — O(1), **zero hot-path syscalls** (the
  populator already paid the `/sys` free-check). When a burst drains the warm
  pool, `acquire()` re-polls every 20 ms while the populator refills
  concurrently; the populator naps 10 ms while refilling, 100 ms when steady.
- **Sturdier free-check.** A device is free iff `/sys/block/nbdN/pid` is
  absent **and** `/sys/block/nbdN/size == 0` (two signals, matching E2B) — a
  half-torn-down device is never warmed. `release()` just clears the reserved
  bit; the populator re-validates before re-warming, so a still-tearing-down
  slot is skipped until truly free (no blocking in `Drop`).
- **Self-sizing from the kernel.** [`build_from_kernel`] reads
  `/sys/module/nbd/parameters/nbds_max`; `None` (materialize-to-file fallback)
  when the module isn't loaded (macOS dev) or `ENGRAM_NBD_DISABLE` is set.
  This **retires `ENGRAM_NBD_DEVICES`** — a clean break (zero users), the
  chart's `nbdsMax` is now the single knob for pool size.
- **Survivor path preserved.** [`NbdSlotAllocator::from_paths`] still builds a
  restricted-universe pool over specific devices (the FC integration tests and
  the survivor-rehydrate harness); `claim()` reserves a specific busy device,
  skipping the free-check (a survivor's device is busy by design).

Defaults: OSS `nbdsMax: 64`, `warmSlots: 64` (the whole pool stays warm at
small scale). Dense fleets override total — engrams-internal runs
`nbdsMax: 4096`, `warmSlots: 64`. `nbds_max` only takes effect on a fresh
module load, so node-prep reloads the module when the live value differs
(succeeds on a fresh/idle node; logs and defers on a busy one).

## Consequences

- A same-image burst no longer exhausts the pool: 4096 slots, 64 warm,
  acquisition is a queue pop. The wedge — blocked restores holding RAM
  reservations — cannot form from slot scarcity. The placement queue paces the
  burst; the populator keeps slots ready.
- Leaked/stale devices stop poisoning the pool — at 4096 slots a handful of
  stuck bindings are noise, and the populator's free-check routes around them.
- The hot path loses its `/sys` `stat` storm (O(slots) → O(1)).
- A background populator task per host (one `tokio::spawn`, holds a `Weak` so
  the allocator drops cleanly). Idle cost is a 100 ms poll of a lock + compare
   — negligible.
- Rolling `nbds_max` 64→4096 on an existing node needs a module reload, hence
  a node drain/recreate; engrams-internal recreates the `kvm` nodes as part of
  the roll.

## Validation

- Unit tests (`slot::tests`): populator warms to target, distinct handout,
  drop-repopulates, `acquire()` blocks-then-wakes, busy-slot skip, specific
  `claim()`, `free_paths` excludes reserved.
- FC integration tests (`nbd_chunked_disk`, `nbd_netlink_reconfigure`,
  `two_host_live_teleport`) exercise `from_paths` + `claim` on real devices.
- Prod: re-run the ADR 0048 load test (n=30 then n=100) on the `kvm` pool with
  `nbdsMax: 4096` — expect the wedge gone (queue drains, restores complete,
  lossless), host NBD-slot telemetry well below the cap. Flip to Accepted on a
  clean run.

## Follow-ups — bottlenecks the throttle was hiding

The 16-slot pool was *serializing* restores; removing it surfaced three latent
issues, each fixed in turn (the "fix one bottleneck, expose the next" cascade):

1. **UFFD substrate invisible on fresh nodes.** The host-agent's
   `/var/lib/engram` volumeMount lacked `mountPropagation: HostToContainer`, so
   node-prep's base-shm tmpfs (mounted in the host ns *after* the agent starts)
   never propagated in — FC's shmem-only UFFD restore 503'd. Long-lived nodes
   hid it via a restart-after-mount race; recreating nodes for `nbds_max=4096`
   exposed it. Fixed by adding the propagation flag.

2. **helm-deploy wedged the host-fleet release.** The DaemonSet is `OnDelete`
   (operator owns the drain-gated roll), so `helm upgrade --wait` always timed
   out → release `failed`; a cancelled run mid-wait left it stuck
   `pending-upgrade` → next deploy aborts "another operation in progress". The
   pipeline now self-heals (rollback) + drops `--wait` for that release. Note
   the operator rolls host-agent pods on **image** change only — a chart/
   template-only change (e.g. the propagation flag) is rolled by a graceful
   per-pod delete, not the operator.

3. **Sidecar device race (real, but secondary).** `restore_with` reads
   `rootfs_source` from the per-base-snapshot `snapshots/<base>/manifest.json`,
   which EVERY same-base restore patches with its own `/dev/nbdN`
   (read-modify-write). A sibling's patch landing between a restore's patch and
   its read could make FC open the WRONG device. Fixed by passing each
   restore's device DIRECTLY to FC via `restore_with_rootfs_override`
   (`SandboxBackend` trait), authoritative over the shared sidecar. **This was a
   genuine hardening but did NOT fix the load-test `lossless` failure** — the
   re-run showed identical corruption, which led to (4).

4. **Same-base concurrent-restore data corruption — the real `lossless`
   failure.** A restored session's chunked-disk backend is keyed by
   `disk_manifest_ref.manifest_id`, and on the FRESH-CREATE path that is
   `bundle.disk_manifest` — the BASE IMAGE's id, identical for every same-base
   session. The flush path only `next_version()`s that shared id (it never minted
   a new one, unlike the *memory* path which does `ManifestRef::new()` per
   capture). So N concurrent same-base sessions all wrote under one
   `manifests/<base>/vN` chain: they raced the chunk-store version counter (the
   ADR 0014 "version conflict; retry latest+1" band-aid, at 22-wide × 41 retries)
   and `(manifest_id, version)` became AMBIGUOUS across sessions — session A's
   `v_k` and B's `v_k` are different disks, indistinguishable to the resume/
   recovery selector → cross-session reads (`reread ''`) + stale-drop reaps.
   Confirmed live: 22 sandboxes → one `manifest_id=2502838e`, 41 conflicts, 18
   stale-drops. **Fix:** lazily fork the disk manifest to a private per-session
   `manifest_id` on the first flush of a fresh-base session (`BackendState::
   fork_pending`, armed by `attach_manifest(.., fork_on_first_flush=true)` only on
   the fresh-create path). The fork's chunk list still references the shared,
   content-addressed base chunks (no byte duplication — density preserved); only
   the manifest IDENTITY forks. Mirrors what memory already does
   (`seed_checkpoint_chain`). Transparent to GC (pins by content hash) and the
   coord columns (already per-row UUID); the resume selector's
   `live.manifest_id == snapshot.manifest_id` assumption is *repaired*. Regression
   tests: `backend::tests::fresh_same_base_backends_fork_to_distinct_private_ids`
   + `unforked_backend_ticks_its_attached_id` (CI); prod load test is the e2e
   gate.

   *Residual (lazy fork):* a session idle-evicted with ZERO disk writes (its
   snapshot `disk_manifest` is still the base) then resumed-and-written would
   re-attach the base id and tick it — the fork only arms on the fresh-create
   attach, not on resume-from-base. This is rare (a running guest almost always
   writes *something* to its rootfs before idling) and strictly better than the
   pre-fix state, but to close it fully the coordinator would pass a
   "disk_manifest == image base" flag at resume so the host arms the fork there
   too (or fork eagerly at attach, trading a manifest PUT per restore). Deferred.
