# State reconciliation + live-VM continuity — rollout tracker

Live punch list for the ADR 0009 migration. Keep this current as
commits land; if something here drifts from the code, the code wins —
update the doc.

The architectural plan lives in
[`docs/adr/0009-state-reconciliation.md`](./adr/0009-state-reconciliation.md).
This file is the operational view: what's shipped, what's left, what
gates each successive failure-mode-closure.

## Status legend

- ✅ **shipped** — landed on main, tests passing
- 🟡 **partial** — meaningful work landed, named gaps remain
- ⬜ **pending** — not started
- ⛔ **blocked** — needs design call or upstream dependency
- 💤 **deferred** — explicitly out of v1 scope

## Where we stand (one-paragraph)

Status: ⬜ pending. ADR proposed 2026-05-13; implementation has not
started. The closest existing surface is the heartbeat path
(`crates/engram-protocol/src/heartbeat.rs`) plus the snapshot
completion path (`crates/engram-coordinator/src/api/snapshot.rs`)
where Phase 1 + Phase 2 will plug in.

---

## Phase 1 — Schema + wire surface

**Status: ⬜ pending — gates everything after.**

### Scope

- New migration: `snapshots.recoverable BOOLEAN NOT NULL DEFAULT false`.
- `WIRE_VERSION` → 6. Additive: `Heartbeat.running_sandboxes:
  Vec<SandboxId>` populated by `backend.list()`.
- Coord parses the new field but **does not act on it yet**. Goal of
  this phase is just to flow the data shape end-to-end so subsequent
  phases can rely on it being present.

### Expected commits

- `crates/engram-protocol/src/heartbeat.rs` — add field.
- `crates/engram-protocol/src/wire.rs` — bump `WIRE_VERSION` to 6;
  update the version-history doc-comment.
- `crates/engram-host-agent/src/heartbeat.rs` —
  `build_heartbeat` accepts `running_sandboxes`.
- `crates/engram-host-agent/src/lib.rs` — `heartbeat_provider`
  populates the field from `backend.list().await`.
- `deploy/migrations/<n>_snapshots_recoverable.sql`.

### Gate

`just check` green. The existing `wire_integration.rs` round-trip
tests cover the new field via positive case. Manually verify a v5
host-agent registering against a v6 coord (or vice versa) fails the
handshake loudly — same mechanism as previous WIRE_VERSION bumps.

---

## Phase 2 — Snapshot pipeline writes `recoverable`

**Status: ⬜ pending — depends on Phase 1.**

### Scope

- After chunked-OCI upload completes in
  `crates/engram-coordinator/src/api/snapshot.rs`:
  - `blob.head(disk_manifest)` + `blob.head(memory_manifest)`.
  - If both succeed, `UPDATE snapshots SET recoverable=true WHERE
    id=$1` in the same transaction that marks the snapshot complete.
- Chunk-store GC clears the flag back to `false` when it reaps a
  manifest:
  - One CTE addition in `crates/engram-coordinator/src/chunk_gc.rs` —
    `UPDATE snapshots SET recoverable=false WHERE disk_manifest_id IN
    (reaped) OR memory_manifest_id IN (reaped)`.

### Expected tests

- Migration smoke: backfill existing rows to `recoverable=false`;
  verify next snapshot per session writes `true`.
- Integration test: take a snapshot, force a manifest reap via the
  admin GC endpoint, assert the column flips back to `false`.

### Gate

`recoverable=true` rate ≥ 99% on healthy hosts (excluding manifests
in active GC pressure). If significantly lower, investigate before
Phase 3.

---

## Phase 3 — Reconcile pass activated

**Status: ⬜ pending — depends on Phase 1 + Phase 2.**

### Scope

- New module `crates/engram-coordinator/src/reconcile.rs`:
  - `reconcile_host(host_id, running_sandboxes, db_snapshot) ->
    Vec<StatusFlip>`.
  - Strikes counter (`HashMap<SessionId, u8>` behind
    `parking_lot::Mutex`); 3-heartbeat grace.
  - Flip executor uses `pg_try_advisory_lock` for the UPDATE,
    matching `dead_host.rs`.
- `crates/engram-coordinator/src/api/hosts.rs`: on
  `NotifyKind::Heartbeat` ingest, invoke `reconcile_host(...)`.
- `crates/engram-coordinator/src/config.rs`: `reconcile_grace_ticks`
  (default 3).

### Gate

**Case B closed.** Validation: the four currently-stuck sessions
(`21815fbc`, `bcf2917d`, `689278fe`, `d08707e8` — see
`docs/known-issues.md`) should resolve themselves automatically on
the next heartbeat after this phase deploys. Operator does nothing.

### Expected tests

- Integration test in `engram-coordinator/tests/`: start coord +
  1 in-proc host; create 3 active sessions (one with a
  `recoverable=true` snapshot taken first); drop two from the
  backend's in-memory map (simulating crash); wait 20 s; assert: the
  one with a snapshot flipped to `Idle`, the other to `Dead`, the
  third stayed `Active`.

---

## Phase 4 — Host-side VM supervision (§4)

**Status: ⬜ pending — independent of Phases 1-3; can land in any order.**

### Scope

- `crates/engram-sandbox-firecracker/src/lib.rs`: at sandbox-create,
  `tokio::spawn(child.wait())`; on exit, prune `Sandboxes` entry.
  Same for the UFFD handler `Child`.
- `crates/engram-sandbox-vz/src/backend.rs`: hook
  `VZVirtualMachineDelegate` state transitions; prune on
  Stopped/Error.
- `crates/engram-sandbox-process/src/lib.rs`: watcher per
  `agent_children` entry; prune on exit.
- (Optional) Add `NotifyKind::SandboxDied { sandbox_id, reason }`
  variant; host pushes on prune for ~100 ms reconcile latency
  instead of the next heartbeat tick. Additive variant, no
  WIRE_VERSION bump.

### Gate

**Case F closed.** Create an FC sandbox; `kill -9` the FC PID
directly; assert the entry leaves `backend.list()` within ~1 s; coord
receives the heartbeat with the absence; session flips per Phase 3's
policy.

---

## Phase 5 — FC sandbox manifest format + write-on-create

**Status: ⬜ pending — gates Phases 6 and 7.**

### Scope

- New module `crates/engram-host-agent/src/sandbox_manifest.rs`:
  schema v1 (defined in the ADR); atomic read/write helpers using
  the write-temp-then-rename pattern from `chunk-store/file.rs`;
  per-sandbox path conventions
  (`<work_dir>/sandboxes/<sandbox_id>/sandbox.json`).
- `crates/engram-sandbox-firecracker/src/lib.rs`:
  - At end of `create()`, write the manifest atomically (after FC
    is fully running and its API responds, so the recorded `pid` +
    `start_time_jiffies` are stable).
  - On `destroy()`, delete the manifest *after* the sandbox teardown
    completes (so a host-agent crash mid-destroy leaves a manifest
    pointing at the still-dying FC, which path 1 will fail and path
    3 will reap).

### Gate

Manifest readback unit tests. Chaos test: SIGTERM the host-agent
while a sandbox is alive; verify the manifest is on disk and
correctly parseable.

---

## Phase 6 — FC startup pidfd reattach pass — path 1 only

**Status: ⬜ pending — depends on Phase 5.**

### Scope

- New module `crates/engram-host-agent/src/live_attach.rs`:
  `reattach_pass(work_dir, backend) -> ReattachReport`. Drives path-1
  reattach over all manifests.
- New module `crates/engram-sandbox-firecracker/src/pidfd.rs`:
  thin `nix::sys::pidfd_open` wrapper + `AsyncFd<PidFd>` adapter for
  tokio. (Path 2 will share this module.)
- `crates/engram-sandbox-firecracker/src/lib.rs`: expose
  `reattach_sandbox(manifest) -> Result<LiveSandbox>`.
- `crates/engram-sandbox-firecracker/src/net.rs`: add
  `NetAllocator::mark_slot_used(slot)` for rehydration.
- `crates/engram-host-agent/src/lib.rs`: on startup, call
  `live_attach::reattach_pass(work_dir, backend)` BEFORE dialing
  coord; log the report.
- `crates/engram-host-agent/src/main.rs`: `--live-attach` flag +
  `ENGRAM_LIVE_ATTACH` env var (defaults on for FC, off for VZ /
  process).

### Gate

**Case C closed.** End-to-end test in a Linux+KVM environment:
create a sandbox, exec into it, SIGTERM the host-agent process, wait
for restart, exec into the same sandbox via the same coord/host
registration. Assert exec works and the session was never in a
non-`Active` state.

PID-recycle defense: create an FC sandbox; kill it; spawn another
process; invoke reattach; assert refuses on start-time mismatch.

### Documentation refresh

- `docs/deploy.md`: note the Linux 5.3+ kernel floor for pidfd.
- `DESIGN.md`: update the host-agent process description to reflect
  the reattach contract.

---

## Phase 7 — SIGTERM checkpoint pipeline for FC

**Status: ⬜ pending — depends on Phase 5; can land before or after
Phase 6.**

### Scope

- New module `crates/engram-host-agent/src/shutdown.rs`:
  - Signal handler (`tokio::signal::unix::SignalKind::terminate` +
    `interrupt`).
  - Per-backend graceful-shutdown orchestrator.
  - Concurrency-bounded fan-out across sandboxes.
  - Deadline budgeting against `--shutdown-deadline-secs` (default
    25 s, matching the GCE-Spot-style budget).
- `crates/engram-host-agent/src/main.rs`:
  - `--shutdown-deadline-secs`, `--shutdown-drain-secs` (default 5),
    `--shutdown-checkpoint-parallelism` (default 8) flags.
  - `ENGRAM_GRACEFUL_SHUTDOWN` env var.
- `crates/engram-sandbox-firecracker/src/lib.rs`: expose
  `checkpoint_to_local(sandbox_id) -> Result<LocalSnapshotRef>` that
  pauses FC, runs the chunked-memory pipeline against the on-host
  chunk store, updates the manifest's `last_local_snapshot`, and
  leaves FC running.

### Gate

End-to-end test in Linux+KVM: create an FC sandbox; exec a
long-running process inside; send SIGTERM to the host-agent.
Assert: (a) the manifest's `last_local_snapshot` is populated within
5 s, (b) the host-agent exits cleanly within the configured budget,
(c) the chunk-store has the referenced chunks on disk.

### Telemetry

Log per-sandbox checkpoint duration + dirty-chunk count. Validates
the ADR's ~3 s budget arithmetic empirically before Phase 8 turns on
the matching reattach path.

---

## Phase 8 — FC startup path-2 NVMe restore

**Status: ⬜ pending — depends on Phases 6 + 7.**

### Scope

- `crates/engram-sandbox-firecracker/src/lib.rs`: expose
  `restore_from_local_snapshot(manifest) -> Result<LiveSandbox>`
  using the existing chunked-restore pipeline.
- `crates/engram-host-agent/src/live_attach.rs`: extend the reattach
  pass — when path 1 fails, try path 2 (NVMe restore) before falling
  through to orphan-reap.

### Gate

**Case C' closed for FC.** End-to-end: SIGTERM the host-agent AND
`kill -9` the FC process AFTER the checkpoint completes (simulating
an OS reboot that takes FC down). Restart the host-agent. Assert:
path 2 fires, FC is re-spawned from the local snapshot, the session
continues from the checkpointed memory state (validated by reading a
memory-resident artifact written before checkpoint).

Mixed-mode chaos: run 10 sessions, mid-stride SIGTERM the
host-agent. Assert: sessions whose FC survived go path 1; sessions
whose FC died-and-was-checkpointed go path 2; sessions whose FC
died-WITHOUT-checkpoint go path 3 (reconcile flip per Phase 3).

---

## Phase 9 — VZ SIGTERM checkpoint + startup restore

**Status: ⬜ pending — independent of FC phases; can land any time
after Phase 5's manifest format is defined.**

### Scope

- `crates/engram-sandbox-vz/src/backend.rs`: expose
  `checkpoint_to_local(sandbox_id)` that APFS-clones the rootfs
  into `<snapshot_dir>/<sandbox_id>/rootfs.ext4` and writes the
  manifest. Reuses the existing VZ snapshot path.
- `crates/engram-sandbox-vz/src/lib.rs`: VZ's analogue of the FC
  sandbox manifest writer, written at create + updated on SIGTERM.
  Different format (no PID; just rootfs path + APFS clone path).
  Shares schema with the FC manifest via backend-specific union
  variants.
- VZ startup pass scans manifests; for each with
  `last_local_snapshot`, cold-restores from the APFS clone +
  cold-boots the VM. Conversation continuity comes from the
  bootstrap supervisor + `claude --resume <id>` per ADR 0003.

### Gate

**Case C' partially closed for VZ** (session continuity, not RAM
continuity). End-to-end on macOS: `just dev` running an active VZ
session; Ctrl-C the coord; `just dev` again. Assert: session
continues with the bootstrap supervisor restarting the harness;
conversation history via `claude --resume` survives.

---

## Phase 10 — Orphan-reap extension + operator endpoint

**Status: ⬜ pending — independent; can land any time after Phase 5.**

### Scope

- `crates/engram-host-agent/src/orphan_reap.rs`: new entry points
  per backend layout —
  `reap_failed_reattach_fc(work_dir, manifests)`,
  `reap_stale_sandbox_files_vz(work_dir, live_set)`,
  `reap_stale_sandbox_cwds_process(work_dir, live_set)`. Called by
  `lib.rs` after the reattach pass so failed-reattach sandboxes get
  their disk artifacts cleaned in the same pass.
- `crates/engram-coordinator/src/api/admin.rs`:
  `POST /api/admin/reconcile-now?host_id=<id>` — fans out a
  reconcile pass to the named host (or all hosts if omitted) for
  incident response. Plugs into the existing `HostAdminHandler` RPC
  surface.

### Gate

Manual: kill a sandbox via backend, hit `POST
/api/admin/reconcile-now`, observe the session flip in <1 s instead
of waiting for the next heartbeat strike-out.

---

## Cross-cutting

- **Documentation cadence.** Update `docs/known-issues.md` to retire
  the "active sessions stick after restart" entry after Phase 3
  lands. Keep the "abandoned `harness: none` sessions never GC'd"
  entry, pointing forward to the future session-GC ADR. Add a new
  entry forward-referencing the future ephemeral-host preemption
  ADR.
- **Observability.** Each phase logs structured events for every
  flip / supervised exit / checkpoint / reattach with structured
  reason codes. No new metrics surface in this ADR (deferred under
  the existing observability gap).
- **Backwards compatibility.** Phases 1-2 are wire-additive and
  schema-additive; safe to roll forward. Phases 3+ are operator-
  visible behaviour changes; no rollback hazard since they're all
  "flip to a more accurate state."

---

## Deferred (out of v1 scope)

- **Garbage collection of long-abandoned-but-live sessions**
  (case G). `harness: none` sessions left forever; Idle sessions
  never revisited. Own ADR.
- **Ephemeral-host cold-tier durability** (spot preemption,
  autoscale-down). NVMe disappears with the host, so this ADR's
  SIGTERM checkpoint doesn't help. The right primitive is a
  preemption-drain rewrite that uploads chunked-memory deltas to
  BlobStorage within the cloud's preemption notice window. Newly
  feasible in the chunked-memory world. Own ADR.
- **Cross-host session migration on host loss.** ADR 0005's
  deferred decision still stands.
- **Per-session reconciliation cost telemetry.** Deferred under the
  existing observability-gap entry.
- **VZ live-VM pidfd reattach.** Architecturally infeasible
  (in-process VMs). VZ gets the cold-boot path instead (Phase 9).
- **systemd-managed FC instances** (each FC under its own systemd
  unit). Operational evolution; out of scope here.
