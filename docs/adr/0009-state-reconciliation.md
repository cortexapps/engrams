# ADR 0009: State reconciliation, graceful-shutdown checkpoint, and live-VM reattach

Status: proposed (review pending), 2026-05-13
Phase: 0 (design)
Supersedes: nothing. Layers above ADR 0007's chunked storage and
ADR 0008's chunks-in-OCI. The dead-host detector of Phase 3,
the idle evictor of Phase 4, and the preemption drainer all
remain — this ADR adds a periodic reconciliation primitive that
catches what those event-driven detectors miss, plus a
graceful-shutdown path that survives common operator actions
without dropping sessions.

## Context

Engram flips session state today on **observed events**:
heartbeat timeouts (`dead_host.rs`), explicit harness-Idle
notifications (`idle_evictor.rs`), preemption notices
(`preemption_drain.rs`). Each detector is correct for the
failure mode it watches. The problem is the **gaps between
them** — every failure mode that doesn't produce an observable
event leaves state divergent forever, and the operator is the
only path back to convergence. Beyond correctness, the operator
UX of redeploys is surprising: a host-agent process restart
wipes the in-memory `SandboxBackend` map and orphans every VM
the host owned, even when the underlying Firecracker processes
are still running.

The failure-mode taxonomy, against today's behaviour:

| # | Scenario | Today | User-visible |
|---|---|---|---|
| A | Coord redeploy (`--mode=coordinator` prod) | Hosts reconnect; `repopulate_routing` re-seeds. VMs continue. | Sessions stay `Active`. ✅ |
| B | Coord redeploy (`--mode=all` dev) | Same process owns coord + host. VMs die with it. | Sessions sit `Active` pointing at sandbox_ids that no longer exist. ❌ (observed today) |
| C | Host-agent code redeploy (prod, FC); FC processes survive | In-memory map wiped; FC processes orphaned. | Coord routing breaks against live but unreachable VMs. ❌ |
| C' | Graceful host reboot (prod); FC processes terminate too | In-memory map wiped AND FC processes killed; NVMe disk survives. | Sessions lost; no recovery despite tens of seconds of SIGTERM budget. ❌ |
| D | Host-agent redeploy (dev, VZ / process) | VMs are in-process — they die with the host-agent. | Sessions lost. Acceptable for dev. |
| E | Host hardware death / kernel panic | Dead-host detector fires; sessions → Dead. | ✅ |
| F | VM crashes mid-session, host-agent lives | `Child` future resolves but nothing prunes the entry from `backend.list()`. | Host reports phantom sandbox; exec fails; session sits `Active`. ❌ |
| G | Harness disconnects mid-session (VM alive) | Connection-scoped TTLs lose their trigger. | `Active` until 30 min hard TTL — or forever for `harness: none`. (Deferred to a future GC ADR.) |
| H | Operator drain | Drain flag set; existing sessions run out. | ✅ |
| I | Coord ↔ host transient partition | <30 s blip recovers cleanly. | ✅ |

Observed today: four sessions (`21815fbc`, `bcf2917d`,
`689278fe`, `d08707e8`) sit `Active` pointing at sandbox_ids
that no longer exist anywhere — case **B**. Cases **C** and
**C'** are the larger production concerns: a routine host-agent
rolling upgrade today mass-orphans every session on each host,
and a graceful OS reboot loses every session even though the
SIGTERM window had plenty of time to checkpoint state to local
NVMe.

## Decision

Six sub-decisions. §§1–3 converge after divergence happens;
§§4–6 prevent divergence during normal lifecycle events. §5
combines two complementary continuity paths — a SIGTERM-time
checkpoint that writes only-dirty chunks to local NVMe (~seconds)
and a startup reattach pass that prefers running FC processes
when available and falls back to NVMe restore.

### 1. Reconciliation as the unifying primitive

The host's `SandboxBackend::list()` is the authoritative source
for "what live VMs exist on this host" — provided **the host's
view of `list()` itself stays honest** (which §4 enforces).
Postgres `sessions` rows are the authoritative source for "what
the platform thinks exists." When they diverge, the host is
right and sessions get transitioned to a terminal state.
Detection is by *absence of evidence*, inverting today's
event-driven model.

### 2. Reconcile rides the heartbeat (no new RPC)

`Heartbeat` already flows every 5 s carrying
`(capacity, local_snapshots, draining)`. Extend it with
`running_sandboxes: Vec<SandboxId>` populated from the
host-agent's `backend.list()`. `WIRE_VERSION` bumps to **6**.

Coord-side handler, on every `NotifyKind::Heartbeat`:

```
expected = SELECT id, sandbox_id FROM sessions
           WHERE host_id = $host AND status = 'active'
missing  = expected − heartbeat.running_sandboxes
for each missing_session:
    increment strikes; if strikes ≥ N → flip via §3 policy
```

Cost: ~12 B × ~50 sandboxes ≈ 600 B per heartbeat per host;
one indexed `SELECT … WHERE (host_id, status)` per heartbeat.
Trivial.

**Anti-flap grace window.** A sandbox must be missing from N=3
consecutive heartbeats before the coord transitions the
session — 15 s at the 5 s default cadence, symmetrical with
`dead_host.rs`'s 30 s heartbeat timeout. Tracked in-memory in
the coord (`HashMap<SessionId, u8>` strikes counter behind
`parking_lot::Mutex`). Strikes reset to zero on a heartbeat
where the sandbox is present.

### 3. Missing-sandbox policy: `snapshots.recoverable`

When a session's sandbox is missing for N consecutive
heartbeats:

- Look up the latest `snapshots` row for the session.
- If `snapshots.recoverable = true` → transition to `Idle`
  (the next user prompt rehydrates via the existing resume
  path).
- Otherwise → transition to `Dead` (terminal).

`snapshots.recoverable` is a new column on the `snapshots`
table, set at **snapshot-creation time**:

1. Snapshot pipeline completes the chunked-OCI upload to
   BlobStorage (existing path).
2. Pipeline verifies `blob.head(disk_manifest)` +
   `blob.head(memory_manifest)` — canonical manifests are
   durable.
3. Sets `recoverable = true` in the same transaction that
   marks the snapshot complete.
4. Chunk-store GC clears the flag back to `false` when it
   reaps a manifest (one CTE addition to `chunk_gc.rs`).

Reconcile then trusts the column — zero per-tick I/O against
BlobStorage. The column reflects "as of the last GC sweep,
the snapshot was recoverable"; transient blob backend outages
don't flap session state.

### 4. Host-side VM supervision: keep `backend.list()` honest

Today the FC backend stores a `Child` handle per sandbox
(`engram-sandbox-firecracker/src/lib.rs`) but nothing waits on
it. If the FC process exits — kernel OOM, segfault, manual
kill — the entry stays in the host's `DashMap` forever.
`backend.list()` reports a phantom sandbox; reconcile is blind
to the corpse. This is failure mode **F**.

Per backend, supervise the underlying process lifecycle and
prune on exit:

- **Firecracker.** At sandbox-create, `tokio::spawn(child.wait())`.
  On `Ok(status)` or `Err(_)`, acquire the `Sandboxes` lock and
  remove the entry. Same for the UFFD handler `Child` when
  active. Emit a structured `tracing::warn!` with the exit
  status.
- **VZ.** Apple's `VZVirtualMachineDelegate` emits state
  callbacks. Hook the `VZVirtualMachineStateStopped` (and
  `*Error`) transitions through the existing `objc2-virtualization`
  binding; prune in the same shape.
- **Process.** Same pattern as FC — watcher per `agent_children`
  entry. (Already partially observable via exec failure; pruning
  makes it deterministic.)

**Optional fast-path notify (rollout phase 4).**
`NotifyKind::SandboxDied { sandbox_id, reason }` — additive
variant; host pushes on prune so reconcile fires within ~100 ms
instead of waiting for the next heartbeat. Optional and
orthogonal to correctness; heartbeat-driven reconcile alone is
sufficient.

### 5. FC live-VM continuity: SIGTERM checkpoint + three-path reattach

The case-**C** and case-**C'** fix. Three pieces working
together: a per-sandbox manifest format, a SIGTERM-time NVMe
checkpoint, and a startup reattach pass with a three-path
fallback. The chunked-memory architecture (ADR 0007) is what
makes the checkpoint cheap enough to fit a SIGTERM window —
only dirty chunks need writing, and they're already destined
for the on-host chunk-store.

**Per-sandbox manifest.** Written atomically (write-temp-then-
rename, matching the pattern in `chunk-store/file.rs`) at
sandbox-create. Lives at
`<work_dir>/sandboxes/<sandbox_id>/sandbox.json`. Deleted on
`destroy`. Schema v1:

```jsonc
{
  "schema_version": 1,
  "sandbox_id": "...",
  "backend": "firecracker",
  "spec": { /* full SandboxSpec */ },
  "firecracker": {
    "pid": 12345,
    "start_time_jiffies": 4242424,   // /proc/<pid>/stat field 22
    "comm": "firecracker",
    "api_socket": "/var/sandboxes/<id>/firecracker.sock",
    "vsock_uds_base": "/var/sandboxes/<id>.vsock",
    "vsock_cid": 3
  },
  "network": {
    "tap_name": "engtap-42",
    "vm_cidr": "10.200.0.16/30",
    "host_ip": "10.200.0.17",
    "guest_ip": "10.200.0.18",
    "iptables_chain": "engram-egress-42",
    "net_allocator_slot": 42
  },
  "uffd_handler": {                   // null when UFFD isn't active
    "pid": 12346,
    "start_time_jiffies": 4242500,
    "comm": "engram-uffd-handler",
    "socket": "/var/sandboxes/<id>.uffd"
  },
  "last_local_snapshot": {            // null until first SIGTERM-checkpoint
    "disk_manifest_id": "...",        // points at chunk-store entries on local NVMe
    "memory_manifest_id": "...",
    "taken_at": "2026-05-13T18:42:11Z",
    "trigger": "sigterm"
  }
}
```

**SIGTERM checkpoint pipeline.** Host-agent installs a SIGTERM
handler at startup. On signal:

1. Set the "shutting down" flag; refuse new `create()` calls
   (`SandboxError::Unavailable`).
2. Wait for in-flight `exec_stream` / `snapshot` / `restore`
   RPCs to drain, bounded by `--shutdown-drain-secs` (default
   5 s).
3. For each sandbox in `Sandboxes`, in parallel (concurrency
   bounded by `--shutdown-checkpoint-parallelism`, default 8):
   - FC API: `PATCH /vm Paused`.
   - Snapshot memory chunked via the existing pipeline
     (`engram-sandbox-firecracker::snapshot::serialize_to_chunks`)
     writing to the on-host chunk-store on NVMe. **No
     BlobStorage upload** — local-only is the budget-meeting
     cost. Content-addressed dedup means only dirty chunks
     since the last snapshot are written; a steady-state 4 GiB
     VM with ~100 MiB of mutation produces ~200 chunks ×
     ~1 ms NVMe write ≈ 200 ms.
   - Atomic manifest update via write-temp-then-rename:
     `last_local_snapshot: { disk_manifest_id, memory_manifest_id, taken_at, trigger: "sigterm" }`.
   - **Leave FC running** (do not kill the FC process). This
     preserves path 1 of the reattach pass as the fastest
     restart path; the NVMe checkpoint is purely a fallback
     for when FC also dies (case C').
4. Exit cleanly. If SIGKILL'd before completion, partially-
   written sandboxes either still have a live FC (path 1) or
   their last successful checkpoint (older) is still valid for
   path 2.

**Budget arithmetic.** 50 sandboxes × ~500 ms per sandbox
(pause + serialize + bookkeeping) / 8-way parallelism ≈ **3.1 s**
for a fully loaded host. The default systemd
`TimeoutStopSec=90 s` (or even GCE Spot's 30 s notice) covers
this comfortably. The dominant cost is the FC pause-and-
serialize step, not the I/O.

**Reattach pass on host-agent startup — three paths.** Before
connecting to coord:

1. Scan `<work_dir>/sandboxes/*/sandbox.json`.
2. For each manifest, attempt **path 1: live-pidfd reattach**:
   - `kill(pid, 0)` → process alive
   - `/proc/<pid>/stat` field 22 matches `start_time_jiffies`
     (defeats PID recycling across reboot)
   - `/proc/<pid>/comm` matches recorded value
   - FC API socket responds with a successful `InstanceInfo`
   - Network device `tap_name` exists in the kernel

   If all five checks pass: `pidfd_open(pid)` (Linux 5.3+),
   wrap in `tokio::io::unix::AsyncFd`, reconstruct
   `LiveSandbox` from the manifest, mark `net_allocator_slot`
   reserved, spawn the §4 supervisor on the pidfd. The
   sandbox rejoins `backend.list()` transparently.
   **Wall-clock cost: ~10 ms.** Catches case **C**.

3. If path 1 fails, attempt **path 2: NVMe local-snapshot
   restore**:
   - Manifest's `last_local_snapshot` must be non-null.
   - All chunks referenced by `disk_manifest_id` +
     `memory_manifest_id` must be present in the local chunk-
     store (single `chunk_store.has_all_chunks(manifest)`
     check — content-addressed, fast).

   If both pass: issue `backend.restore()` against those
   manifests through the existing chunked-restore pipeline.
   Chunks are already on NVMe, no BlobStorage round-trips.
   **Wall-clock cost: ~500 ms – 1 s** for typical dirty-memory
   deltas. Catches case **C'**.

4. If both paths fail, **fall through to orphan-reap**: delete
   the manifest; run orphan-reap on the sandbox subdir /
   vsock UDS / TAP / iptables chain. Sandbox absent from
   `running_sandboxes` in the first heartbeat → §3 flips the
   session per `snapshots.recoverable` (the BlobStorage cold-
   tier path you already have).

**Process supervision after reattach.** Same supervisor as §4
in both path 1 and path 2. After path 1, the supervisor watches
the pidfd-wrapped `AsyncFd`. After path 2, the supervisor
watches the freshly-spawned FC `Child` from the restore.

**Backend scope for path 1.** Live-pidfd reattach is
**Firecracker only**. VZ VMs run inside the host-agent's
address space (Apple's framework hosts them in-process), so
they cannot survive host-agent death. Process backend's
subprocesses likewise share lifecycle with the parent. The
SIGTERM-checkpoint half generalizes — see §6.

### 6. Host-agent startup contract + SIGTERM coverage per backend

| Backend | SIGTERM behaviour | Startup behaviour | Failure modes covered |
|---|---|---|---|
| FC (production) | Full §5 pipeline: pause + chunked NVMe snapshot + manifest update, FC left alive. ~3 s for a loaded host. | §5 three-path reattach. | C, C' transparent; B via reconcile fallback. |
| VZ (dev / Mac fidelity) | APFS-clone snapshot to `<snapshot_dir>/<sandbox_id>/rootfs.ext4` (existing VZ snapshot path) + manifest update with `last_local_snapshot`. **Session continuity, not RAM continuity** — VZ doesn't expose memory state for chunked capture. Conversation continuity via bootstrap supervisor + `claude --resume <id>` per ADR 0003. ~1 s per sandbox (APFS clone cost). | Clean slate (no pidfd reattach possible). For each manifest with `last_local_snapshot`: cold-restore from the APFS clone → cold-boot VM. Without a snapshot: reconcile flips per §3. | D upgraded: VZ sessions survive `just dev` restart with cold-boot continuity. |
| Process (dev) | Clean slate — no checkpoint. Process backend is for orchestration-loop testing only. | Clean slate; reap per-sandbox cwd subdirs. | D unchanged. |

The orphan-reaper extension lives in
`engram-host-agent/src/orphan_reap.rs` with new entry points
per backend's layout (`reap_failed_reattach_fc`,
`reap_stale_sandbox_files_vz`, `reap_stale_sandbox_cwds_process`).
Runs once at startup, after the reattach pass, so failed-
reattach sandboxes get their disk artifacts cleaned in the same
pass.

## Wire protocol

- `WIRE_VERSION` bumps to **6**.
- `Heartbeat` gains `pub running_sandboxes: Vec<SandboxId>`.
- (Optional, rollout phase 4) `NotifyKind::SandboxDied
  { sandbox_id, reason: String }` — additive variant; host
  pushes on §4 prune. Coord treats it as a low-latency
  "decrement strikes" hint, not a unilateral flip; N-strike
  semantics still apply.
- The sandbox-manifest format is **purely host-local**; never
  serialized over the wire. Coord does not see it.

## Schema changes

- `snapshots` gains
  `recoverable BOOLEAN NOT NULL DEFAULT false` (one migration).
  Backfill: existing rows stay `false`; the first new snapshot
  per session marks the column. Cost of false negatives is one
  extra cold-start for sessions whose only snapshot is pre-
  migration — acceptable.
- No changes to `sessions` or `hosts`. Host-local manifest is
  filesystem-only.

## Consequences

- ✅ **Coord redeploy: transparent.** Already true today
  (case A); ADR preserves it.
- ✅ **Production host-agent code redeploy (case C):
  transparent on FC.** Path-1 pidfd reattach catches every
  sandbox whose FC process survived the restart. ~10 ms per
  sandbox; ~500 ms total for a loaded host.
- ✅ **Graceful host reboot (case C'): transparent on FC,
  session-continuous on VZ.** SIGTERM checkpoint writes
  chunked-memory deltas to NVMe (FC) or APFS clone (VZ);
  path-2 NVMe restore on startup. Checkpoint: ~3 s for
  50 FC sandboxes parallelized 8-way. Restore: ~500 ms – 1 s
  per sandbox.
- ✅ **`--mode=all` dev redeploy: clean recovery via
  reconcile.** Case B fixed; the four currently-stuck
  sessions become a *do nothing* operator playbook.
- ✅ **Mid-session VM crash (case F): bounded recovery.** §4
  prunes phantom entries within ~1 s; next heartbeat surfaces
  the absence; reconcile transitions per policy.
- ✅ **VZ dev (`just dev` after Mac reboot): session
  continuity newly works.** APFS-clone SIGTERM checkpoint +
  cold-boot restore on startup.
- ✅ **One unifying mechanism.** Heartbeat reconcile +
  `snapshots.recoverable` + SIGTERM checkpoint + reattach:
  every convergence gap collapses into one logic chain.
- ⚠ **Abandoned-but-live sessions still sit forever.** Case G
  is out of scope; deferred to the session-GC ADR.
- ⚠ **Ephemeral-host preemption is not covered.** When the
  host itself goes away (spot preemption, autoscale-down,
  kernel panic), NVMe disappears with it; only the existing
  BlobStorage cold-tier path (ADR 0005) recovers. A future
  preemption-drain rewrite — newly feasible in the chunked-
  memory world — would upload to BlobStorage instead of
  NVMe, but is its own ADR.
- ⚠ **Linux 5.3+ kernel floor for path 1.** pidfd is Linux-
  only and gated on the kernel version. Production FC hosts
  already require Linux + KVM, and 5.3 is from 2019, so this
  is practically free. Document the floor in deploy docs.
- ⚠ **Manifest correctness is load-bearing.** A wrong PID or
  stale start-time in the manifest could in principle
  reattach to a recycled process. Mitigated by the three-axis
  verification (PID + start-time + comm) plus the FC API
  ping; PID-recycle within those microseconds is effectively
  impossible.
- ⚠ **`backend.list()` could still be wrong if §4 is buggy.**
  Premature transition would flip live sessions. Mitigated by
  the 3-heartbeat grace (15 s) and the conservative Idle-vs-
  Dead policy (Idle is reversible).
- ⚠ **`snapshots.recoverable` correctness depends on
  chunk-store GC keeping it honest.** Existing GC walks
  reachable manifests; one CTE addition flips the column when
  a manifest becomes unreachable. If GC misses a manifest, a
  session flips to Idle and the user's resume attempt fails
  loudly — degraded recoverable behaviour, not silent data
  loss.
- ⚠ **SIGTERM-checkpoint correctness depends on FC's pause-
  and-serialize being idempotent under load.** The existing
  snapshot pipeline is already exercised by the idle evictor;
  adding a third invocation site is an extension, not new
  code.
- ⚠ **Engineering cost.** ~5–6 weeks for one engineer:
  reconcile mechanism + `snapshots.recoverable` ~500 LOC;
  §4 host-side supervision ~200 LOC; §5 FC reattach +
  SIGTERM ~1800 LOC; §6 VZ SIGTERM ~300 LOC; integration
  tests against real FC microVMs and a chaos rig that
  SIGTERMs the host-agent mid-session.

## Alternatives considered

- **Always-Dead policy on missing sandbox.** Simpler, but
  throws away the cold-tier durability ADR 0005 paid for.
- **Live HEAD check at reconcile time instead of
  `snapshots.recoverable`.** One blob HEAD per missing
  session per tick; column trades one persistent boolean for
  zero per-tick I/O. Transient blob outages would flap session
  state under the live-HEAD design.
- **Dedicated `Reconcile` RPC.** Strictly more wire surface
  than extending Heartbeat for identical information.
- **Coord polls hosts via `SandboxBackend::list()` over the
  existing remote-backend RPC.** Same information, extra
  round-trip per host per tick instead of riding the heartbeat
  already in flight.
- **`prctl(PR_SET_CHILD_SUBREAPER)` instead of pidfd.** Lets
  the host-agent re-become the reaper across restart. Doesn't
  avoid the need to identify which FC PIDs belong to which
  sandboxes — manifests still required. pidfd's poll
  semantics fit tokio cleanly; subreaper makes the design more
  brittle for similar effect.
- **systemd-managed FC instances** (each FC under its own
  systemd unit). Operationally clean; introduces a systemd
  dependency to the host-agent runtime contract; deferred as
  a future evolution.
- **SIGTERM checkpoint uploads to BlobStorage instead of
  NVMe.** Would cover ephemeral-host preemption (spot,
  autoscale-down) where NVMe disappears with the host.
  Deferred: BlobStorage upload is ~10× slower per chunk than
  NVMe write; a 4 GiB VM with 100 MiB dirty would take
  ~5–30 s vs ~200 ms locally — might not fit a Spot
  preemption window for loaded hosts. The right ADR for this
  is a preemption-drain rewrite.
- **SIGTERM async-fires BlobStorage upload after NVMe write
  completes.** Best-effort cover for ephemeral hosts when
  budget allows. Rejected for scope here; the future
  preemption ADR layers it cleanly.
- **Bundle session-GC / abandoned-session TTL into this
  ADR.** Considered and rejected for scope. Convergence +
  live-VM continuity (this ADR) and lifecycle policy (a
  future ADR) are separable concerns.
- **On-disk sandbox manifest covering everything needed to
  reattach to a live VZ VM.** Architecturally infeasible:
  Apple's `VZVirtualMachine` is hosted in the host-agent's
  address space; its state cannot survive process death. The
  APFS-clone SIGTERM checkpoint is the strongest path VZ can
  offer.
- **Status quo.** Every restart leaves operational debt; the
  bug pattern observed today recurs forever; host-agent
  rolling upgrades remain dangerous.

## Rollout

Tracked in `docs/state-reconciliation-rollout.md`. Ten
maturity tiers:

1. **Schema + wire surface.** Migration adds
   `snapshots.recoverable`. `WIRE_VERSION` → 6 with
   `running_sandboxes` in `Heartbeat`. Host populates from
   `backend.list()`. Coord parses but doesn't act yet —
   observe heartbeat data shape for a release.
2. **Snapshot pipeline writes `recoverable`.** Hook
   BlobStorage HEAD verification into the snapshot completion
   path. Chunk-GC clears the column on reap.
3. **Reconcile pass activated.** Heartbeat handler invokes
   `reconcile_host(host_id, running_sandboxes)`; 3-heartbeat
   grace + Idle/Dead policy. Every flip logged with structured
   reason. **Case B closed.**
4. **Host-side VM supervision (§4).** Per-backend Child /
   VM-state watchers; prune on exit. Optional:
   `NotifyKind::SandboxDied` for low-latency push. **Case F
   closed.**
5. **FC sandbox manifest format + write-on-create.** Define
   schema, atomic write at create, delete at destroy. No
   reattach logic yet. Validate manifest readback in unit
   tests + a chaos test that kills host-agent and verifies
   manifest is on disk.
6. **FC startup pidfd reattach pass — path 1 only (§5).**
   Three-axis verification, net-state rehydration, in-memory
   map reconstruction. Behind `ENGRAM_LIVE_ATTACH=1`. Per-
   registry validation against real FC microVMs. **Case C
   closed.**
7. **SIGTERM checkpoint pipeline for FC (§5).** Install
   signal handler; drain in-flight RPCs; pause-and-checkpoint
   per sandbox in parallel; write `last_local_snapshot`.
   Behind `ENGRAM_GRACEFUL_SHUTDOWN=1`. Integration test:
   SIGTERM mid-session, restart, verify path-2 NVMe restore
   succeeds.
8. **FC startup path-2 NVMe restore (§5).** Reattach pass
   falls through to local-snapshot restore when path 1 fails.
   **Case C' closed for FC.**
9. **VZ SIGTERM checkpoint + startup restore (§6).** APFS
   clone on SIGTERM; cold-boot from snapshot on startup.
   **Case C' partially closed for VZ** (session continuity,
   not RAM continuity).
10. **Orphan-reap extension + operator endpoint.** New
    `orphan_reap.rs` entry points;
    `POST /api/admin/reconcile-now?host_id=...` for incident
    response.

Per-phase gates and exit criteria in the rollout doc.

## What this ADR does NOT cover

- **Garbage collection of long-abandoned-but-live sessions**
  (case G). `harness: none` Active forever, `Idle` for weeks —
  explicitly deferred. This ADR converges *divergence*;
  lifecycle/storage policy is its own ADR.
- **Ephemeral-host cold-tier durability** (spot preemption,
  autoscale-down, hardware death). NVMe disappears with the
  host. The right ADR is a **preemption-drain rewrite** that
  uploads chunked-memory deltas to BlobStorage within the
  cloud's preemption notice window (~25 s for GCE Spot).
  Newly feasible in the chunked-memory world; was infeasible
  in the pre-chunked tar+zstd era. Tracked separately.
- **VZ live-VM pidfd reattach.** Architecturally infeasible
  (in-process VMs). VZ gets the cold-boot-from-snapshot path
  instead (§6).
- **Cross-coord race on reconcile flips** in multi-replica
  deployments. Use the existing `pg_try_advisory_lock` pattern
  from `dead_host.rs` for the UPDATE.
- **Per-session reconciliation cost telemetry.** Deferred
  under the existing observability-gap entry.
- **Per-session `lost_sandbox_policy` (Idle vs Dead) at
  create-time.** Considered as a `SandboxSpec` field; rejected
  as overengineering. Universal policy from §3 applies.
- **Cross-host session migration on host loss.** ADR 0005's
  deferred decision still stands.
