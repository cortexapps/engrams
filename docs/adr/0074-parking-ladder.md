# 0074 — The parking ladder: cancellable, pressure-driven eviction

Status: Proposed (2026-07-06)

Issue: #545 (2026-07 core-ops overhaul, Tier 2). Depends on: #540 (RAM
ledger — hard for rungs 2–4), #543 (op log — soft; cancel becomes an
op-cancel once it lands). Builds on ADR 0067 (the outbox delivery
driver is where a returning prompt meets the ladder) and ADR 0028
(diff-first checkpoints are the cheap-pause primitive).

## Problem

Idle is a cliff, not a ladder. From the instant of nomination the
session is on a one-way conveyor — nominate → snapshot → destroy →
Idle → full rebuild on return — with no `Evicting → Active` edge in
the FSM, so the single most common "the user is here" signal (a
follow-up prompt during eviction) waits for a healthy VM to be
destroyed and then pays the full resume (prod p50 12.2s / p95 89.1s;
prompt→run_started p50 ≈24.5s; 45/45 resumes rewound transcripts) plus
a 3–21s lease collision with the eviction's own finalize. Descent
fires on a 300s clock regardless of memory pressure; the
pressure-aware fix shipped default-off because parked residency is
unaccounted (the ADR 0046 honesty problem → #540).

## Decision

Idle becomes descending rungs driven only by memory pressure, each
with a cheap ascent. **Invariant: eviction is reversible up to the
moment its resources are actually reclaimed, and it descends only as
far as pressure requires.**

1. **Nominated** (`Evicting`, `park_rung=1`, VM untouched): ascent is
   one lease-guarded PG CAS over the new `Evicting → Active` edge —
   the only new FSM edge in the overhaul (no new states; op rows are
   the in-flight visibility per the resolved overhaul decision).
2. **Parked-paused** (`park_rung=2`; FC paused after a 38–53ms diff
   whose upload runs in the background): ascent = un-pause + the same
   edge. Frees no RAM — it buys interactive-return latency, dwell
   capped (default 900s; stale guest TCP), skipped under acute
   pressure.
3. **Parked-local** (`Idle`, `park_rung=3`; VM destroyed, snapshot
   staging + NBD backing retained on NVMe): ascent = resume pinned to
   the parking host from local artifacts (~0.5–2s). NEW machinery —
   explicitly not `live_attach` (which reattaches live processes).
4. **Evicted-remote** (`park_rung=0`): today's full GCS restore, now
   the rare case.

Pressure-driven descent becomes the ONLY mode; the clock TTL survives
as candidacy, never as a reclaim trigger; hard TTL remains the
absolute ceiling. Rung-2/3 residency charges `allocatable_mib` (#540).
The speculative `ensure_rung` typing hint is dropped from v1.

## Staging

**Rung 1 (PR #581).** The `Evicting → Active` legality edge,
`park_rung`/`parked_at` schema (migration 0086), nomination stamping,
and a lease-guarded cancel folded into `ensure_active` — so ADR 0067's
outbox delivery driver cancels the nomination inline and the returning
user's prompt proceeds against the untouched VM. The 8s Evicting hold
loop survives only as the capture-already-started fallback.

**Rung 2 + reaper (this PR).** Parked-paused, stacked on rung 1:

- **Park decision** (`evict_session_to_state`, Idle target, before the
  browser reap / capture): if `host_has_memory_headroom` — the RAM
  ledger's `allocatable_mib` (#540, which already nets out other parked
  VMs' PSS so we never over-park) is ≥ `ENGRAM_PARK_HEADROOM_FLOOR_PCT`
  (default 30%) of `mem_total_mib` — PAUSE the VM in place, stamp
  `park_rung=2 / parked_at`, and return `EvictOutcome::ParkedPaused`
  with the session held at `Evicting` and the sandbox alive. The
  headroom check **fails closed** (a telemetry gap → no park → full
  eviction): a paused VM frees no RAM, so parking under *unknown*
  pressure is the dangerous direction, the mirror of the idle
  detector's fail-open-toward-eviction. `allow_park=false` (the drain
  path and the reaper's own descent) skips this branch entirely.
- **Ascent** (`try_cancel_nominated_eviction`): if the session is
  `park_rung=2`, un-pause the sandbox **before** the CAS
  `Evicting → Active`, under the lease we already hold — and only commit
  the Active transition if the un-pause succeeded (a failed un-pause
  leaves the session `Evicting` for the standard resume path rather than
  advertising Active over a paused VM). The returning user's prompt then
  proceeds against the same live guest — no rebuild.
- **Reaper** (`park_reaper_advance_one`, driven by the existing eviction
  scanner tick): a parked row (`park_rung ≥ 2`) is routed here instead
  of the eviction pipeline (which would re-pause it and bump
  `evict_attempts` to `HostLost` every tick — guarded in both
  `scanner_run_once` and `scanner_advance_one`). The reaper DESCENDS the
  parked VM to a full eviction when the dwell cap
  (`ENGRAM_PARK_DWELL_SECS`, default 900s) elapses OR the host loses
  headroom (`reason=dwell|pressure`): un-pause → `evict_session_to_state
  (Idle, allow_park=false)` → `Idle` with a durable snapshot, exactly
  like a plain idle eviction. Parking leaves no trace in the terminal
  state.

A parked-paused session stays `Evicting`, which `reserves_host_memory`
already counts — correct, the paused VM still occupies RAM; the RAM
ledger's `allocatable_mib` nets it back out for placement so it isn't
double-counted.

**Rung 3 (parked-local) is sequenced into #548, not built here.** Its
core primitive — destroy the VM but retain the snapshot staging + NBD
backing on local NVMe so a returning session resumes pinned to the
parking host without a GCS round-trip — IS the authoritative-affinity +
peer-first-fill machinery of the GCS-free-resume epic (#548). Building
host-local retention now, separately from #548's affinity tracking,
would be the exact "special case layered on shared infrastructure"
anti-pattern this overhaul is trying to retire (and would duplicate the
affinity/pin state two ways). Rung 3 lands there, reusing `park_rung=3`
as the on-host-retention marker. **Rung 4 (evicted-remote) is the
existing full eviction** — unchanged, now the pressure/dwell floor the
ladder descends to.

## Divergence log

- **No host `cancel_evict` RPC.** The issue specified one ("clear the
  eviction-inflight marker, reset idle clocks") — but ADR 0067 phase 3
  deleted the host-side eviction markers and idle clocks entirely; the
  host holds NO nomination state. Cancellation is purely
  coordinator-side: `try_acquire` the session lease (no retry — a held
  lease means the capture pipeline owns the session; fall back to the
  hold loop), CAS `Evicting → Active`, clear `park_rung`, release. The
  lease guard is load-bearing: the eviction scanner treats `Active` as
  a legal post-lease input (the drain path), so an unguarded CAS could
  cancel "under" a pipeline that then proceeds to evict an Active
  session.
- **Rung 2 is pause-only in v1 — no diff-banking.** The Decision framed
  parked-paused around "a 38–53ms diff whose upload runs in the
  background" (ADR 0028's cheap-pause primitive). This PR parks with a
  plain `pause` and defers the diff/upload to the 2→4 descent (which
  pays the full capture). The diff-banking optimization — flush a
  checkpoint *at park time* so the eventual descent is near-free — is a
  clean follow-up that doesn't change the rung's state shape; it was
  dropped from v1 to keep the parked path a single backend `pause` call.
- **Headroom gate reads `allocatable_mib`, fails closed.**
  `host_has_memory_headroom` prefers the RAM ledger's `allocatable_mib`
  (falling back to raw `mem_total − mem_used` only on pre-ledger /
  non-Linux hosts) and returns `false` on any telemetry gap
  (`mem_total_mib == 0`, host not found, list error). Parking under
  unknown pressure could overcommit a host, so the safe default is the
  full eviction.
- **The park pipeline's lease releases asynchronously**, so an ascent or
  descent fired *immediately* after a park can race the release and see
  a held lease (transient false / `Skipped`). Production already retries
  this (the `ensure_active` hold loop; the next scanner tick); the unit
  tests poll `wait_for_lease_free` before the follow-on step. No product
  code change — the retry paths already existed for rung 1.

### Post-ship incident: the un-pause vsock black-hole (2026-07-06)

The first prod deploy of rung 2 wedged every un-parked session: the
ascent's `host.resume` returned Ok in ~30ms, but the returning prompt
never ran — `send_prompt` kept succeeding into an intact hub connection
(outbox rows showed `attempts=N, delivered, never acked`), in-guest exec
hung, and the FC event-loop thread burned 100% of a core while the vCPUs
idled. Two wrong theories died on the way to the root cause: the
harness→hub binding is NOT lost across a pause (the hub tears down only
on reader-loop EOF, and pause is a pure vCPU freeze), and the harness
has NO self-reattach to wait for (its connection never EOFs; it re-dials
only on a dropped link or agentd's SIGUSR1) — which invalidated the
first fix attempt (#594, an outbox "self-reattach window" that was also
unreachable dead code because `outbox_defer` didn't increment
`attempts`).

The real cause is in Firecracker itself — **upstream v1.16, inherited by
the fork**: `Vmm::resume_vm()` calls `kick_virtio_devices()` on *every*
resume, and the vsock device's `kick()` unconditionally arms
`pending_event_ack`, the RX gate that blocks all host→guest vsock
delivery until the guest acks a `TRANSPORT_RESET`. That is correct after
a snapshot (`prepare_save` queued a reset for the guest to ack) but a
plain pause→resume queues NOTHING: the gate arms with no reset to ack
and never clears. Rung 2's park/un-pause is exactly a plain
pause→resume, so it turned a latent VMM bug (also reachable via the
ADR 0045 admin pause/resume and migration-abort resume) into a hot path.

Fix (fork commit `engram/fix-vsock-rx-gate-plain-resume`): `kick()`
signals only when the gate is *already* armed and never arms it itself;
`prepare_save` (in-process) and `restore()` (cross-snapshot, re-armed
from the saved `virtio_state.activated` flag — an activated snapshot
always carries a queued reset) own the arming. Snapshot byte-format
unchanged. Regression proof: `tests/pause_resume_vsock.rs` (fork binary
via `ENGRAM_FC_FORK_BIN`, same gating as the stock/fork compat test) —
exec over vsock, plain pause→resume, exec again within a bounded budget.
Engrams-side hardening shipped with it: parked-paused now uniformly
means `Evicting` (the admin `EvictIdle` path used to park an ACTIVE
session), the ascent clears `park_rung` as soon as the un-pause lands
(Active→Active transitions conflict by design), the headroom gate
resolves the host via PG `sessions.host_id` (the in-memory registry made
parking a per-replica coin flip), and `ensure_active` maps terminal
sessions to Gone so the outbox driver drops their rows instead of
deferring forever. Follow-up: file the plain-resume gate bug upstream.
