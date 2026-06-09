# ADR 0046: PG-backed memory reservation for session placement

**Status:** Proposed (2026-06-09)
**Related:** ADR 0044 (K8s host fleet + K4 autoscaler), ADR 0022 (File-backend base-memfile residency), [#147], [#148]

## Context

A 10-session factory burst (all created within ~5 s) took down a host. The
coordinator placed every one of them — plus ~6 still-bound sessions from a prior
round — onto a **single** host (`ab021c17`), whose node then OOM-killed the
host-agent and its NBD daemon; the fleet's second host sat nearly empty. The
coordinator's dead-host detector marked the host dead and moved 16 sessions
through `HostLost` → `Idle`/`Dead`.

The root cause is one missing primitive — **reservation accounting** — and its
absence breaks three things at once:

1. **Placement stacks.** `HostRegistry::pick_for_session_inner`
   (`host_registry.rs:534-555`) already implements the right shape: least-loaded
   (largest free RAM) among ready hosts that fit `memory_mib`. But "free" is
   `capacity.total_mib − capacity.used_mib`, and **`used_mib` is hardcoded `0`**
   at host registration (`main.rs`). So every host always looks fully free, the
   capacity-fit is inert, and the picker effectively stacks onto one host.
2. **No admission control.** With every host looking empty, nothing is ever
   rejected — the picker overcommits a host past its RAM and the kernel OOM-kills
   it for us. 10 × 4 GiB guest RAM does not fit on two ~29 GiB-allocatable nodes
   that each pin ~16 GiB of base-memfile residency (ADR 0022); the correct
   outcome is to **reject** the overflow.
3. **The autoscale signal is phantom.** `fleet_metrics().free_mib` is
   `Σ(total_mib − used_mib)` (`host_registry.rs:577`) — with `used_mib = 0` it
   always reports the fleet as fully free, so the K4 autoscaler can never see
   demand pressure. (Independently, autoscaling is currently unset on the
   deployed fleet — a separate deploy-side gap.)

Memory is the binding resource here, and it has an exact bound: Firecracker
hard-caps each VM at its configured guest RAM, so a session can never exceed its
budget. `Σ(session budgets)` is therefore an exact upper bound, not an estimate.

## Decision

Make **reserved memory** a real, durable, multi-coordinator-correct quantity,
and let the existing least-loaded/fit picker consume it. The coordinator runs a
single replica today, but we will **not** bank on that — the reservation lives
in Postgres so it stays correct across any number of coordinator replicas.

### The `sessions` table is the reservation ledger

A session bound to a host *is* a live reservation; when it goes idle / dead /
off-host the reservation releases automatically (reservation lifecycle = session
lifecycle). No separate reservation table, no stale-reservation reaper.

- **Migration 0057**: add `sessions.mem_budget_mib BIGINT` (set at create from
  the image manifest's `suggested_memory_mib`; the FC hard-cap makes it exact) +
  an index on `(host_id, status)` for the placement aggregate.
- **`reserved(host)`** =
  `Σ mem_budget_mib WHERE host_id = H AND status IN ('pending','created','active','evacuating')`
  **+ `residency_floor`**, where `residency_floor = Σ guest-RAM of enabled
  images` (each enabled image pins a base memfile resident on every host —
  ADR 0022). The floor is a scalar derived from `enabled_images` + their
  manifests, identical across hosts.

### Placement is one serialized PG transaction

Replacing the in-memory capacity read that trusted the always-0 `used_mib`:

```
BEGIN;
  SELECT id, util_mem_total_mib FROM hosts
    WHERE status IN ('ready','draining') FOR UPDATE;   -- serialize concurrent placers (any replica)
  -- reserved(host) from the sessions aggregate + residency floor
  -- free(host) = util_mem_total_mib − reserved(host)
  -- pick least-loaded host with free ≥ session budget
  INSERT INTO sessions (..., host_id = picked, mem_budget_mib = budget, status = 'pending');
COMMIT;                                                -- the INSERT bumps reserved for the next placer
```

`FOR UPDATE` on the candidate host rows serializes concurrent placements across
**all** coordinator replicas — no advisory lock, no non-default isolation level.
If no host fits → **reject with 503** (retryable; the BFS driver re-attempts next
round; once autoscaling is enabled, the now-real `free_mib` scales the pool).

### `free_mib` becomes real

`fleet_metrics().free_mib` is computed the same way (`Σ util_mem_total −
reserved` over schedulable hosts) → a true demand-pressure signal for the K4
autoscaler.

### Reservation lifecycle: reserve at pick, hold through evict, release at idle

The reservation *is* the session row, and it lives from pick to teardown:

- **Reserve at pick.** The placement transaction inserts the session row in
  `pending` with `host_id` + `mem_budget_mib` and `sandbox_id` NULL — *before*
  the VM boots. This moves the insert earlier than today's "row exists only once
  we have host_id + sandbox_id" (`create_session_inner`): the reservation must be
  visible to concurrent placers during the boot window, which is the burst we're
  fixing. (`sandbox_id` is already nullable and the hot paths guard on
  `sandbox_id IS NOT NULL`, so a pending-no-sandbox row is a representable, inert
  state — verified before relying on it.)
- **Finalize after boot.** On a successful boot/restore the row is updated with
  `sandbox_id` (+ status, as today). On boot failure the pending row is deleted
  (reservation released) alongside the existing sandbox teardown.
- **Hold through evicting.** A session keeps its reservation across
  `pending → created → active → paused → evacuating/evicting`. The VM is fully
  resident the entire time — and an eviction *snapshot can take minutes* (#147) —
  so releasing at evict-*start* would re-open the over-admit during the snapshot.
  The reserved set is "states where the VM is resident."
- **Release at idle/terminal.** The reservation drops when the session reaches
  `idle` (or `completed` / `failed` / `dead`) — once the sandbox is actually torn
  down.

**Accepted residual window.** Eviction is PG-first (ADR 0016 §A.1.6):
`status→Idle` is set just before `host.destroy()`, so the budget is released a
sub-second before the RAM is physically freed. We accept this narrow window
rather than thread release through the async, best-effort destroy. A **periodic
reconcile** (reserved recomputed from the resident-state sessions per host) is
the consistency/leak backstop; if the window ever bites, release can move to fire
right after `host.destroy()` without changing the model.

## Key decisions (and the alternatives)

- **Sessions-as-ledger** over a dedicated `reservations` table — the session row
  already carries `host_id` + `status`, so the reservation's lifecycle is free; a
  separate table would need its own reaper (cf. `session_lease`'s 180 s reaper).
- **`SELECT … FOR UPDATE`** over `SERIALIZABLE`+retry or `pg_advisory_lock` —
  explicit, READ-COMMITTED-compatible, connection-pool-friendly, and placements
  are infrequent so serializing them is free.
- **Reserve guest RAM (`suggested_memory_mib`)** — exact, because FC caps the VM
  there; no headroom guessing.
- **Include the residency floor** — without it `free` ignores the ~16 GiB of
  pinned base memfiles and over-admits straight back into OOM.
- **Reject (503), not queue** — simplest correct admission control now; a
  server-side queue is future work.

## Consequences

- Burst-safe + multi-coordinator-correct: a placed session immediately raises
  `reserved`, and `FOR UPDATE` makes concurrent placers (across replicas) see it.
- The OOM class is closed: a host is never committed past its real RAM; overflow
  is rejected, not OOM-killed.
- The autoscale signal stops lying, so enabling autoscaling on the fleet becomes
  meaningful (deploy-side follow-up, not this ADR).
- Restore/resume placement (`restore_for_session` → `pick_for_session`) is gated
  the same way — a resume also reserves.
- `capacity.used_mib` is retired as a placement input in favor of the PG-derived
  figure (kept only as an observability mirror, if at all).

## Non-goals (the broader resource pass)

CPU and disk budgets, autoscaling-policy tuning, the residency reduction (#148),
and a server-side placement queue are out of scope. This ADR is the
memory-placement-reservation primitive that pass will build on.

## Implementation

1. Migration 0057 — `sessions.mem_budget_mib` + `(host_id, status)` index.
2. `MetadataStore`: reserved-per-host aggregate + residency-floor helper.
3. The placement transaction (`FOR UPDATE` hosts + aggregate + atomic bind),
   used by both create and restore.
4. Picker: reject when a budget is set and nothing fits — drop the any-ready
   fallback for the has-budget case; keep it only for "no capacity report yet".
5. `free_mib` from the same reserved figure.
6. Tests: burst placement spreads + rejects overflow; reserved releases on
   session-leaves-host; residency floor respected.

## Implementation notes (divergences from the proposal above)

Two things changed while building this; both are improvements, recorded here per
the ADR-bookend habit:

1. **Host-measured `allocatable` supersedes the coordinator-estimated residency
   floor.** The proposal subtracted `Σ enabled-image guest RAM` (a coordinator
   estimate of the mlock'd residency). That ignored the rest of the host
   baseline — the host-agent daemon (~5–7 GiB), the OS, kube-system pods, and
   the chunk cache — so placement would over-admit by that much. Instead the
   host now reports **`allocatable_mib = MemAvailable + Σ guest-resident (PSS)`**
   in the heartbeat (`HostUtilization`, migration 0058): `MemAvailable` nets out
   the *entire* baseline (daemon + OS + chunk cache + the mlock'd residency)
   automatically — measured, not estimated, and drift-tracking — and adding back
   the running VMs' resident lets placement subtract each session's full budget
   without double-counting. So `free = allocatable − Σ(reserved budgets)`, and
   the `enabled_residency_floor_mib` estimate is deleted. `allocatable == 0`
   (non-Linux dev backend / pre-0058 / brand-new host) is a graceful fallback,
   not a gate.

2. **The reserve-at-pick row.** As noted in the lifecycle section, the session
   row moves to pick time (`pending`, `sandbox_id` NULL) inside the placement
   txn, is finalized by an upsert in `create_session_created` after boot, and is
   deleted on boot failure — versus today's "insert only after the sandbox
   exists." `Pending` becomes a (briefly) persisted state for the boot window;
   the hot paths already guard on `sandbox_id IS NOT NULL`.

3. **Scope:** resume/evac placement still uses the in-memory picker; making it
   reservation-safe (re-binding an existing `idle` row) is a tracked fast-follow
   — the incident this fixes was create-bursts.

[#147]: https://github.com/cortexapps/engrams/issues/147
[#148]: https://github.com/cortexapps/engrams/issues/148
