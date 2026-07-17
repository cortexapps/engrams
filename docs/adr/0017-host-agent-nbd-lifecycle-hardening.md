# ADR 0017: Host-agent NBD lifecycle hardening on restart

Status: 2026-05-24 — **Accepted** as of commit chain below. All three
phases shipped; cross-restart NBD slot leakage no longer accumulates.

Supersedes nothing; complements ADR 0016 (Phase B continuous sync)
by closing the host-agent-restart story that ADR 0016 commit 7's
rehydration left unfinished.

Phase chain:

- **Phase 0** — this ADR in Proposed status. (commit `a24b0ee`)
- **Phase A** — destroy-path hang fix (NBD kernel-thread join
  detached out of `Drop` → tokio worker no longer parks on kernel
  cleanup). (commit `396bb38`)
- **Phase B** — NBD kernel-state cleanup on host-agent startup.
  (commit `1240915`)
- **Phase C** — `egress_sessions` → `session_bindings` rename.
  (commit `6dc0458`)
- **ADR closing bookend** — as-built notes (this commit).

---

## Context

ADR 0016 Phase B shipped continuous disk sync and restart-time
rehydration (commit 7), but the dev-vm e2e verification that
closed Phase B surfaced two interrelated host-agent issues that
make restart NOT a clean operation today:

### Issue 1 — NBD device kernel state survives ungraceful host-agent exit

When a host-agent process dies without explicitly running
`NBD_DISCONNECT` on its bound `/dev/nbdN` devices (SIGKILL, OOM
kill, deploy roll, panic — anything that doesn't cleanly run
`NbdSandboxState`'s Drop), the Linux kernel keeps the device in
"bound" state:

- `/sys/block/nbdN/pid` keeps pointing at the now-dead PID.
- The kernel rejects new `NBD_SET_SOCK` ioctls on that device
  with `EBUSY`.
- `nbd-client -d /dev/nbdN` from userspace **does not reliably
  recover** in this state — observed during 2026-05-24 dev-vm
  testing, `nbd-client -d` silently no-op'd on devices bound to
  dead PIDs.
- The kernel module can't be unloaded (`modprobe -r nbd` returns
  EBUSY) because the "in-use" devices reference-count the
  module.
- The only reliable recovery observed was a host reboot.

Concrete failure mode on dev-vm 2026-05-24: after a series of
e2e test runs, `/sys/block/nbd{0,1,3,5,6}/pid` pointed at PIDs
4203, 5562, 6911, 9093, 10683 — all of which were dead. Only
`/dev/nbd2` was free in the 8-device pool. New session-create
RPCs failed with `nbd attach_manifest: io: Device or resource
busy (os error 16)` until VM reboot.

This is the most operationally surprising failure mode in the
Phase B chain: every prod host-agent restart leaks NBD slots
proportional to the number of running sandboxes. After enough
deploy rolls, hosts become unable to accept new chunked-disk
sandboxes despite the kernel module being loaded.

### Issue 2 — host-agent appears to hang after FC SIGKILL during destroy

A second symptom observed on the same dev-vm run: after a session
went through eviction (`snapshot → record_snapshot → destroy`),
the host-agent stopped responding to subsequent gRPC RPCs from
coord. coord reported `503 Service Unavailable: sandbox vm error:
grpc The service is currently unavailable: http2 error` on the
next operation (resume, in the e2e_resume_rejoins test).

Diagnostic:

- The host-agent process was still alive (`State: S (sleeping)`,
  `Threads: 10` per `/proc/<pid>/status`).
- The host-agent's log had no entries after the FC SIGKILL line:
  `2026-05-24T19:00:53 firecracker didn't exit in time; SIGKILLing`.
- 18 seconds later coord's reconciler declared the host dead via
  the heartbeat-loss path.

The chain of events:

1. `PooledBackend::destroy` calls `inner.destroy(id)` (FC
   backend's destroy).
2. FC's destroy sends SIGTERM to the firecracker process, waits
   3s, then SIGKILLs.
3. After SIGKILL, the FC backend's destroy await returns.
4. *Something* downstream blocks. The host-agent stops servicing
   the runtime, including heartbeats.

The blocking point is unknown. Candidates:

- **`nbd_sandboxes.remove(&id)`** → `NbdSandboxState::Drop` →
  `NbdHandle::Drop` → `NBD_DISCONNECT` ioctl + join the kernel
  thread. If the kernel thread doesn't release, the join hangs.
  This would tie back to Issue 1 — the destroy is itself
  ungraceful from the kernel's perspective.
- **`FlushSchedulerHandle::Drop`** → `JoinHandle::abort()`. Should
  return immediately; not a likely blocker.
- **`Arc<ChunkedDiskBackend>` reference count.** The scheduler
  task holds an Arc; if it's mid-`flush()` when the destroy
  fires, the Arc keeps the backend alive past the
  `nbd_sandboxes.remove`. Not a hang per se, just a delayed
  drop.
- **FC's destroy await itself.** Maybe `inner.destroy` doesn't
  actually return cleanly after the SIGKILL fires — could be
  blocked on a tokio task that's waiting on a now-dead FC API
  socket.

The actual root cause needs reproduction + a stack-trace dump
of the sleeping host-agent threads. We can use `eu-stack` or
`gdb -p` against the live process to see what each tokio worker
thread is parked on.

### Why these two together

Issue 1 is the consequence; Issue 2 is the cause (or one of the
causes). Both stem from the same gap: the host-agent's destroy
path doesn't reliably reach the NBD_DISCONNECT ioctl in all
failure modes. Fixing them together gives a coherent story:

- Issue 2 fix ensures NBD_DISCONNECT runs on the *normal* destroy
  path (SIGTERM → 3s → SIGKILL → cleanup).
- Issue 1 fix recovers from the *abnormal* exit (SIGKILL of the
  host-agent itself, OOM, panic) where no destroy ran at all.

Belt and suspenders. The combination means host-agent restart
becomes a routine operation that doesn't accumulate stuck
devices.

### Why not deferred to M4.1

A counter-argument: NBD lifecycle hardening could wait for the
M4.1 evacuation primitive (ADR 0015 M4 follow-up), which would
explicitly drain a host's sandboxes to snapshots before destroying
the host-agent process. That's true, but:

1. **M4.1 doesn't help with crashes / OOM / panics** — those
   never run the graceful drain. Hardened restart is a precondition
   for *any* meaningful evacuation story.
2. **Today's prod host-agents restart on every deploy roll.** A
   host with 30 chunked-disk sessions and 8 NBD devices accumulates
   one stuck device per restart per slot until the pool exhausts.
   Without a fix, every prod host eventually wedges.
3. **The dev-vm verification of ADR 0016 is blocked by Issue 1.**
   Today the resume regression e2e fails on cascading NBD-busy.
   Closing the loop on Phase B's e2e signal requires this work.

---

## Decision

Three coupled changes ship as one milestone:

1. **Phase A — destroy-path hang fix.** Reproduce the hang on
   dev-vm, identify the blocking await, and fix it. Likely a
   tokio task-spawn pattern that detaches the NBD cleanup so the
   destroy await returns even if the kernel-side disconnect is
   slow.

2. **Phase B — NBD kernel-state cleanup on startup.** Host-agent's
   NBD pool allocator probes `/sys/block/nbdN/pid` on startup;
   any device whose bound PID is dead gets `NBD_DISCONNECT`
   *and* `NBD_CLEAR_SOCK` ioctls (the latter is the load-bearing
   one when the bound process is already gone — looking up
   what other open-source NBD tools do in this state). Freed
   devices land in the pool. Cleanup is best-effort + logged
   with a `tracing::warn!` so prod ops see how many devices
   recovered each restart.

3. **Phase C — `egress_sessions` rename.** The DashMap is now a
   Phase B primitive (sandbox→session lookup for the publisher),
   not just an egress concern. Rename to `session_bindings`.
   Pure refactor; no semantic change. Filed here so the cleanup
   bookends with the lifecycle work conceptually adjacent to it.

### Why a separate ADR

ADR 0016 closed cleanly on Phase B; reopening it for the host-
agent-lifecycle work would balloon a single ADR past the point
where it reads as one document. The structural concerns are also
distinct enough: 0016 is the COW + continuous-sync surface;
0017 is host-agent operational hygiene. Cross-references between
the two stay tight (this ADR's §Context links back to 0016's
closing notes; 0016's closing notes link forward to this ADR's
work plan).

### Out of scope

- **M4.1 evacuation primitive** — graceful drain before host
  shutdown. This ADR is about UNGRACEFUL exits + accumulated
  state. Evacuation work is its own thing.
- **FC-side NBD-loss recovery** — if `/dev/nbdN` goes into a
  stuck state mid-sandbox (kernel issue, not host-agent issue),
  the FC sandbox's I/O fails and it's a candidate for eviction-
  to-snapshot. Requires heartbeat-side health check + evac
  trigger — out of scope here, filed for M4.1.
- **NBD module kernel patches.** If `nbd-client -d` reliably
  freed devices bound to dead PIDs, this ADR would only need
  Issue 2's fix. The kernel-side gap is real but fixing it
  upstream is out of our scope; we work around it with explicit
  NBD_CLEAR_SOCK + NBD_DISCONNECT on startup.

---

## Plan

Commit chain (ADR bookends per `[adr_bookends_substantive_work]`):

- **Commit 0**: this ADR in Proposed status. _(this commit)_
- **Commit 1** — Phase A: destroy-path hang reproduction +
  fix. Likely outcome: detach the NBD cleanup into a spawned
  task so the destroy RPC returns immediately. If the
  investigation reveals something else, the fix shape adjusts
  accordingly + this section gets an "as-built divergence" note.
- **Commit 2** — Phase B: NBD pool cleanup on startup. New
  helper in `disk_daemon/slot.rs` that probes /sys/block + sends
  the recovery ioctls. Tests: a `#[ignore]`'d Linux integration
  test that spawns + kills + restarts an in-process NBD daemon
  and verifies cleanup. Wired into the FC CI lane.
- **Commit 3** — Phase C: `egress_sessions` → `session_bindings`
  rename. Pure refactor; sed-style with manual review. Updates
  comments to reflect the dual-purpose semantic.
- **Commit 4** — ADR closing bookend: as-built notes with commit
  hashes, divergences from this design, dev-vm verification
  status, and any newly-surfaced follow-ups.

## Consequences

### What gets easier

- **Host-agent restart becomes routine.** Crash, deploy roll,
  OOM kill — all recover cleanly on the next startup. NBD
  device pool repopulates from kernel-stuck state without
  operator intervention.
- **Phase B's resume regression e2e becomes verifiable on
  dev-vm.** Today blocked by the cascading NBD-busy that Issue 1
  produces. Once the cleanup ships, the resume regression test
  passes against a fresh integration stack reliably.
- **Operator surface clarifies.** The startup cleanup logs a
  one-line `recovered N stale NBD devices` per restart; ops
  alert can monitor this for unexpected accumulation.
- **`session_bindings` matches its actual role.** Future readers
  don't have to mentally translate "egress_sessions" → "Phase B
  publisher resolver state" — the name says what it does.

### What gets harder

- **NBD pool cleanup adds startup time.** Each `/sys/block/nbdN/pid`
  read + ioctl is fast (<1ms), and the pool is ≤16 devices, so
  total impact is <50ms — negligible against the 5-15 second
  host-agent boot.
- **The startup cleanup is a "trust the kernel" surface.** If
  the kernel state is corrupted in ways nbd-client doesn't
  encode, our cleanup may not recover everything. Documented
  fallback: operator manually `modprobe -r nbd` (which won't
  work if the module is in use) or reboot. Same affordance prod
  has today.
- **Phase A fix has unknown shape.** The hang's root cause
  needs reproduction first. The investigation may surface a
  design issue (e.g., the FlushScheduler's task-spawn pattern
  needs a `CancellationToken` rather than `JoinHandle::abort`,
  or the NBD daemon's serve task needs a more aggressive abort
  path). Plan stays in Proposed until commit 1 lands.

### Explicit non-goals

- **Replacing NBD with something else (e.g., direct virtio-blk to chunked store).**
  Out of scope; NBD is the right ADR-0007/0008 primitive, just
  needs better lifecycle hygiene.
- **Graceful host-agent shutdown signaling.** The host-agent
  should ideally trap SIGTERM + run cleanup before exit. That's
  a separate change (and only helps when the signal is
  SIGTERM, not SIGKILL/OOM); this ADR is about the
  recover-from-unclean-exit half.

---

## Open questions

- **Phase A reproduction.** Need to reliably reproduce the host-
  agent hang to investigate. Today's evidence is one observation
  during the Phase B e2e. May need: trigger a destroy under
  artificial FC slowness, OR add `eu-stack` instrumentation to
  the next dev-vm run and dump threads when the symptom recurs.
- **NBD_CLEAR_SOCK semantics on stuck devices.** Linux docs
  imply CLEAR_SOCK forces the device into a known-disconnected
  state regardless of bound PID. Need to verify empirically on
  dev-vm before committing to it as the recovery primitive.
- **Whether the destroy hang touches the scheduler.** The
  FlushScheduler holds `Arc<ChunkedDiskBackend>` and may be
  mid-flush during destroy. If the hang is in
  `JoinHandle::abort` waiting for an in-progress flush to
  yield, the fix is to switch to a `CancellationToken` so the
  flush can early-exit. Investigation in Phase A will tell.

---

## As-built notes (2026-05-24)

### Phase A — `396bb38`

Root cause confirmed by reading the `NbdHandle::Drop` body: the
`kernel_thread.join()` happens *inline* on whatever tokio worker
ran the destroy await. `NBD_DISCONNECT` is fast, but the kernel
thread's actual exit can stall on inflight I/O — at which point
the tokio worker is parked, and the host-agent stops servicing
heartbeats. Eighteen seconds later coord reaps the host. Exactly
the observed symptom.

Fix shape: the cheap synchronous steps (`NBD_DISCONNECT`,
`serve_task.abort()`) stay inline; the rest (`kernel_thread.join`,
`NBD_CLEAR_SOCK`, fd close) move into a detached `std::thread::
spawn`. `Drop` returns immediately, tokio workers keep servicing
heartbeats, and the kernel cleanup runs out-of-band.

Paired with Phase A's `nbd_kernel_busy` probe in
`NbdSlotAllocator::acquire`: the slot is released to the pool the
moment `Drop` returns, but a fast re-acquire on the same path
would race the kernel's tear-down. The probe pokes
`/sys/block/nbdN/pid` and rotates past slots whose kernel-side
cleanup is still in flight, with a 500ms periodic re-probe wake
source.

### Phase B — `1240915`

`recover_stuck_nbd_devices(paths)` ships in
`disk_daemon/runtime.rs`, gated `cfg(target_os = "linux")` and
exported from `disk_daemon::`. The host-agent's startup
(`crates/engram-host-agent/src/main.rs:391`) calls it BEFORE
`NbdSlotAllocator::from_paths` so the kernel state is cleaned
before the pool is seeded.

Open-question answer on `NBD_CLEAR_SOCK`: empirically (dev-vm
2026-05-24), `NBD_DISCONNECT` + `NBD_CLEAR_SOCK` issued back-to-
back is the right primitive. The first signals the kernel thread
to exit; the second drops the kernel's reference to the bound
socket so the device's `/sys/block/nbdN/pid` clears.

Integration test in
`crates/engram-host-agent/tests/nbd_startup_recovery.rs`:

- `recovery_is_noop_for_unbound_devices` runs unconditionally
  (covered by CI's `cargo nextest run --workspace`).
- `recovery_clears_kernel_busy_device` is `#[ignore]`'d, gated on
  `ENGRAM_NBD_STUCK_DEVICES`, and skips on insufficient device
  perms. Prod host-agents grant the necessary R/W via the
  Packer-installed udev rule.

### Phase C — `6dc0458`

Pure rename `egress_sessions → session_bindings` across the four
host-agent files that reference it. No semantic change. The field
doc comment now leads with both consumers (destroy + publisher)
and notes the rename rationale so future readers don't have to
mentally translate "egress" → "binding index."

### Divergences from the design

None substantive. The plan called for a one-shot startup probe +
ioctl pair; that's exactly what shipped. The Phase A fix shape
matched the design's "detach the NBD cleanup into a spawned task"
prediction.

The plan mentioned a possible `CancellationToken` rewrite for the
FlushScheduler. That didn't end up necessary: the destroy hang
turned out to be the `NbdHandle::Drop`'s kernel-thread join, not
the FlushScheduler. The scheduler's existing `JoinHandle::abort`
is correct — Phase A's investigation closed that open question
without code changes to the scheduler.

### Dev-vm verification

- `cargo test -p engram-host-agent --lib` (122 tests) passes on
  dev-vm after Phase B.
- `recovery_is_noop_for_unbound_devices` passes.
- `recovery_clears_kernel_busy_device` skips on dev-vm because
  the test user (`nikhil_unni_cortex_io`) isn't in the `disk`
  group; the prod host-agent user is.
- `just check` (full workspace fmt + clippy + nextest) passes
  locally after Phase C.

### Newly-filed follow-ups

- **FC-side NBD-loss recovery** stays in scope for M4.1 (already
  out of scope in §"Out of scope"). Not regressed by this ADR.
- **dev-vm test user in `disk` group.** The
  `recovery_clears_kernel_busy_device` test is skip-on-perms,
  which is the right behavior, but it'd be nice to actually
  exercise the recovery path in the dev-vm test loop. Two
  options: (a) udev rule on dev-vm matching the prod Packer
  manifest, or (b) the test stages a stuck NBD device under root
  before exercising recovery. Neither is urgent — the noop test
  + prod runtime cover the primitive's behavior.

### Pre-existing stuck devices on dev-vm

As of 2026-05-24, dev-vm still has `/dev/nbd1..5` bound to dead
PIDs `4203, 5562, 6911, 9093, 10683`. They'll be cleaned on the
next host-agent startup (now that Phase B is wired). No manual
intervention required.

---

## Addendum (2026-07-17): acked-write durability across pod rolls + slot hygiene

Session `85e0298a` (prod, 2026-07-16) surfaced guest-visible ext4
corruption (`Corrupt inode bitmap`, `Structure needs cleaning`,
EUCLEAN) traced to three acked-write-loss / stale-read windows in the
NBD data plane. Fixed in the commit chain carrying this addendum:

1. **SIGTERM deadline overrun rolled back acked writes.** NBD WRITEs
   ack from the in-RAM dirty tier; the issue-#225 final flush is
   budgeted (20 s) against `terminationGracePeriodSeconds`, and on
   overrun the dirty tier died with the process — the successor
   rehydrated from the last published manifest, silently rolling a
   RUNNING guest's disk back (320–370 MiB per sandbox in the incident;
   9 overruns fleet-wide that week). Fix: the abandon sweep now exports
   every un-uploaded chunk (dirty ∪ pending-upload tiers) to a
   **shutdown spool** under `<checkpoint_dir>/spool/<sandbox_id>/`
   (hostPath, survives the roll), and the successor's
   `rehydrate_sandbox` adopts a lineage-matching spool into the fresh
   backend's dirty tier BEFORE the RECONFIGURE releases parked guest
   I/O. The spool tolerates the store-ahead case (chunks + manifest
   uploaded, coord publish lost) by attaching from the spool's newer
   ref. Regression: `nbd_shutdown_final_flush.rs::
   sigterm_overrun_spools_dirty_writes_and_successor_adopts_them`.

2. **Host page cache is a hidden volatile write tier.** FC's drive is
   buffered host I/O (`cache_type=Unsafe`), so guest-acked writes sit
   in the host page cache for `/dev/nbdN`; during the pod-handoff
   dead-connection window their writeback fails and the kernel drops
   them (`lost async page write` — observed at both incident rolls).
   Fix: the SIGTERM flush pass now `sync_all`s each survivor's device
   FIRST (while our serve loop can still ack the writeback), pushing
   that tier into the dirty map where the flush/spool can see it. The
   full fix (O_DIRECT FC drives so device errors surface to the guest
   instead of vanishing) needs the ADR 0045 FC fork and stays open.

3. **NBD slot reuse leaked the previous tenant's page cache.** The
   ADR 0049 allocator reuses `/dev/nbdN` minors with no invalidation
   anywhere in release → acquire → attach; the kernel does not
   reliably invalidate a bdev's page cache across
   disconnect/reconnect, so a fresh tenant could read the PRIOR
   tenant's cached pages — including pages whose writeback had failed
   at that tenant's teardown. (The incident session attached to a slot
   that had just absorbed a 13-minute failed-writeback storm and hit a
   corrupt-bitmap CRC failure 17 minutes later, before any loss event
   of its own; the post-copy migration path already carried a
   `BLKFLSBUF` for exactly this class.) Fix: `attach_backend` now
   BLKFLSBUFs every freshly CONNECTed device before the caller hands
   it to FC; failure is a hard attach error (a failed create beats
   silent cross-tenant corruption).

4. **`claim` raced the populator's validation window.** `populate`
   holds a slot RESERVED for the duration of its free-check; a busy
   survivor device is reserve→check→unreserve cycled, so a one-shot
   `claim` landing inside the window read "reserved, not warm" as "a
   lease owns it" and returned `None` — the successor's rehydrate then
   strands the survivor's disk until evict_local → resume (surfaced as
   a CI flake of the spool regression test; the same race exists at
   every prod successor startup). `claim` now retries across the
   window; a genuine lease still returns `None` after the budget.

Related fix in the same chain (engram-chunk-store): the cache sweep's
`list_entries` walk aborted on any vanish-mid-walk `NotFound` stat —
chronically, several times an hour on every busy host — so eviction
never completed a pass and the cache blew far past its budget (424 GiB
/ 77 % of the incident node's volume, an ENOSPC-corruption risk). The
walk now skips vanished entries.

Still open (follow-ups, not this chain): serve-loop outage
post-checkpoint failed guest writes for 13 minutes on the incident
node (root cause of the storm itself, distinct from the slot-reuse
leak above); read-path integrity is verify-on-populate only (a corrupt
NVMe cache file or RAM tier is served unverified and can be laundered
into new dirty chunks via RMW); periodic checkpoints retry forever
against a dead data plane instead of escalating.
