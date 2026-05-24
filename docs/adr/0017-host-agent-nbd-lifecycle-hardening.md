# ADR 0017: Host-agent NBD lifecycle hardening on restart

Status: 2026-05-24 — **Proposed**, scoped from ADR 0016 Phase B
closing follow-ups (§"Newly-filed follow-ups"). Will flip to
**Accepted** when the commit chain in §"Plan" lands.

Supersedes nothing; complements ADR 0016 (Phase B continuous sync)
by closing the host-agent-restart story that ADR 0016 commit 7's
rehydration left unfinished.

Phase chain (filled in as commits land):

- **Phase 0** — this ADR, in Proposed status. _(this commit)_
- **Phase A** — destroy-path hang investigation + fix. _(pending)_
- **Phase B** — NBD kernel-state cleanup on host-agent startup. _(pending)_
- **Phase C** — `egress_sessions` → `session_bindings` rename. _(pending)_
- **ADR closing bookend** — as-built notes, commit chain, divergences.

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
