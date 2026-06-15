# ADR 0047: stateless coordinator — Postgres is the only authority

**Status:** Accepted (2026-06-12) — S2 (PG-authoritative placement + durable cordon, `69a3ad63`), S3+S4 (teleport pins + sealed broker tokens, `a9e5e6cb`), S5 (shared reconciler strikes) + S6 (replica audit + cross-replica tests) landed as one chain; prod multi-replica flip tracked in engrams-internal.
**Related:** ADR 0012 (single-replica constraint — superseded by this), ADR 0015 M3 (routing cache + read-through), ADR 0044 (K8s host fleet), ADR 0046 (PG-backed placement reservation), ADR 0048 (fleet autoscaling — the consumer that forced this)

## Context

ADR 0048 makes the coordinator load-bearing for autoscaling decisions: cordons
must survive for the minutes a consolidation wave runs, the operator reads
fleet demand from whichever replica the Service routes to, and a 1000-session
burst will be the moment a single coord pod is busiest — exactly when we want
N replicas behind the Service. ADR 0012's "single replica, no HA" posture was
always a deferral, not a design; this ADR closes it instead of carrying the
debt into the autoscaling work.

An audit of every piece of in-memory coordinator state found the system is
already ~80% replica-safe:

**Already correct (stays as is):**
- **Session events / SSE** — events are PG-persisted (`session_events`) and
  fanned out cross-pod via `LISTEN/NOTIFY` (`pg_listener.rs`); the per-pod
  broadcast bus is a delivery vehicle, not state.
- **Scanners** (evac_resumer, idle_evictor, eviction/queue scanners) — all
  per-session work is `session_lease`-guarded (PG `INSERT … ON CONFLICT`);
  the dead-host detector races replicas via PG advisory locks.
- **`sandbox_owner` routing map** — a true read-through cache
  (`resolve_owner` → `host_for_sandbox`); correct to drop and rebuild.
- **Capacity/utilization** — persisted per heartbeat (migrations 0023/0056/0058).

**Replica-unsafe (this ADR fixes):**
1. **`HostState.ready_images` + `local_snapshots` + `current_bundles`** live
   only in the per-pod heartbeat mirror, and the first two are *load-bearing
   for placement* (the image-readiness gate and snapshot-affinity ranking).
   Heartbeats land on ONE replica per request; every other replica sees an
   empty `HostState` and refuses to schedule onto the host (spurious 503
   `ImageNotReady`) or loses affinity.
2. **Cordon** is an in-memory flag plus a PG `hosts.status` write that the
   next heartbeat **clobbers back to `ready`** (`host_http.rs` derives
   `row_status` from the *host's own* `draining` flag). A wave-cordoned victim
   can silently re-open for placement mid-drain.
3. **`state.teleport_targets`** (DashMap) — an operator's pinned teleport
   target is invisible to the sibling replica whose scanner picks the
   `Evacuating` session up.
4. **`git_broker_tokens`** (DashMap) — minted on the pod that created the
   session; a guest forge/upload request landing on (or after a failover to)
   another replica gets 401 until the session dies.
5. **Reconciler strike counters** — per-pod debounce of "sandbox missing from
   heartbeat"; N replicas each count independently, multiplying the effective
   strike rate.

## Decision

PG becomes the only authoritative state. In-memory is allowed for exactly two
categories: **connection-local resources** (gRPC backend pool, SSE broadcast
channels, harness listen addr) and **pure read-through caches** with a PG
fallback (`sandbox_owner`). Everything the *scheduler or lifecycle machinery
decides on* is read from PG at decision time.

### 1. Heartbeat scheduling state moves to PG; placement reads PG

Migration `0060_hosts_scheduling_state.sql`:

```sql
ALTER TABLE hosts
  ADD COLUMN ready_images    JSONB   NOT NULL DEFAULT '[]'::jsonb,
  ADD COLUMN local_snapshots JSONB   NOT NULL DEFAULT '[]'::jsonb,
  ADD COLUMN current_bundles JSONB   NOT NULL DEFAULT '[]'::jsonb,
  ADD COLUMN cordoned        BOOLEAN NOT NULL DEFAULT FALSE,
  ADD COLUMN total_vcpus     INTEGER NOT NULL DEFAULT 0;  -- 0 = not yet reported (ADR 0048 CPU packing)
```

`touch_host_heartbeat` becomes the single per-heartbeat UPDATE carrying
capacity + utilization + the three JSONB fields + `total_vcpus` (one
round-trip, same as today). The handler's in-memory `update_state` is deleted;
the registry only bumps its per-entry `last_observed_heartbeat` (which the
`resolve_owner` TTL still uses — that's connection-freshness, not scheduling
state).

**The picker moves out of `HostRegistry`** into a new `placement.rs`:
- `rank_hosts(hosts: &[HostRecord], ctx) -> RankedCandidates` — pure, fully
  unit-testable: filters to schedulable hosts (status `ready`, not `cordoned`,
  heartbeat fresh within the TTL, not `exclude_host`, digest in
  `ready_images` when required) and ranks the snapshot-affinity prefix first.
- Async wrappers (`pick_for_session`, `pick_specific_host`,
  `pick_capture_host`, fleet snapshot) read `list_active_hosts()` — ~fleet-size
  rows, trivially cheap — rank, and resolve the backend.
- The create path keeps the ADR 0046 shape: ranked candidates feed
  `reserve_placement`, whose `FOR UPDATE` transaction stays the only place
  capacity is committed. Its host SELECT gains `AND NOT cordoned`.

`HostRegistry` is demoted to: the gRPC backend map, the `sandbox_owner`
read-through cache, and the `HostClient` fan-out impl. `HostState`,
`update_state`, `snapshot_state`, `snapshot_all_states`, in-memory
`cordon`/`uncordon`, `candidates_for`, `pick_for_session`, `fleet_metrics` are
**deleted** (clean break, no dual path). `/api/hosts` renders from the PG rows
alone — which now carry everything the old in-memory merge provided.

**Backend resolution is read-through too:** any replica can route to any live
host — `backend_of(host)` on miss falls back to the PG row's `host_addr`,
warms the gRPC pool, and registers the entry (the same self-heal the heartbeat
handler does today, made on-demand).

### 2. Coordinator-authoritative cordon

`hosts.cordoned` is written only by the admin cordon/uncordon endpoints (and
the ADR 0048 wave driver); heartbeats never touch it. `hosts.status` stays
**host-reported** (`draining` = the agent's own shutdown/preStop flag), which
ends the clobber by construction. Effective schedulability =
`status = 'ready' AND NOT cordoned AND fresh`.

A cordon PG write failure is now a **500** (durability is the point — no more
warn-and-continue), and cordoning a host that has a row but no live connection
on this pod succeeds (wave-cordon during pod churn).

**Dead-host exemption.** Today the detector skips `status='draining'` rows so
a K3 image-roll's brief heartbeat gap can't strike out a reattaching host. The
roll's cordon moves to `cordoned`, so the exemption keys on
`cordoned OR status='draining'` — **with a backstop**: a cordoned host whose
heartbeat is older than 10× the stale threshold is struck out anyway. A wave
victim that genuinely dies mid-drain is therefore recovered (sessions →
`HostLost` → rehome) instead of being shielded forever by its own cordon,
while the K3 roll keeps its (minutes-bounded) exemption.

### 3. `teleport_targets` → `sessions.teleport_target_host_id`

Migration 0061 adds the nullable column. The admin teleport endpoint writes
it; the evac scanner reads it as its pinned `require_host`; every existing
clear site (resolution, abort arms, error unwind) becomes the store call. The
DashMap is deleted. Any replica's scanner now honors any replica's pin.

### 4. `git_broker_tokens` → `session_broker_tokens`

Migration 0061 adds `session_broker_tokens` (session_id PK + the
KEK-sealed envelope: wrapped_dek / nonce / ciphertext / key_id — the
`session_secrets` shape). Hash-only storage was REJECTED during
implementation: the token is re-injected into the guest env on every
`/exec` and on resume, so a replica that can't recover the plaintext
would mint a NEW token and invalidate every env a sibling already
injected — replicas would flip the token under live shells per request.
Instead the mint is first-writer-wins (`ON CONFLICT DO NOTHING`; a
losing replica re-reads the winner), making the token STABLE for the
session's lifetime on every replica, and the DashMap is demoted to a
pure read-through cache over the sealed row (authorize + injection
unseal on miss). Cleared on terminal transition, `ON DELETE CASCADE`
as backstop.

### 5. Reconciler strikes → `sessions.missing_strikes`

The missing-sandbox strike counter moves onto the session row (migration
0062, `MetadataStore::apply_missing_sandbox_strikes` — one transaction per
heartbeat: reset the present, increment the missing, return + reset the
ones that crossed `grace_ticks`). The per-pod counter wasn't just slower
under N replicas — it was WRONG: a host's heartbeats round-robin across
pods, so "present" resets land on one pod while strikes accumulate on
another, and a healthy session could be flipped HostLost without ever
being missing N CONSECUTIVE ticks. The shared column restores the
consecutive semantics (pinned by the live-PG test
`missing_sandbox_strikes_are_consecutive_and_shared`).

### 6. Background-task replica audit (verified, no changes needed)

- `enable_scanner` — lease-claimed jobs (`claim_enable_jobs`); a crashed
  pod's claim expires and a peer resumes. ✓
- `checkpoint_retention` — one idempotent DELETE; racing pods delete
  zero rows. ✓
- `idle_detect_backstop` — nominates via `transition_session` (FSM-legal;
  the loser gets `Conflict`). ✓
- `evac_resumer` / eviction scanner — `session_lease`-guarded. ✓
- `dead_host` — PG advisory locks. ✓
- `pg_listener` / SSE — per-pod delivery over PG-persisted events. ✓

## Consequences

- `replicas: 2+` becomes legal for the coordinator Deployment; the
  engrams-internal flip is part of the validation matrix (kill a replica
  mid-session: guest forge calls keep working, SSE resumes, placement and
  cordons unaffected).
- Placement pays one `hosts` SELECT per pick (vs a DashMap walk). At fleet
  sizes (tens–hundreds of rows on the PK index) this is noise against the
  ~330ms+ operations it feeds; resume p99 is measured before/after anyway
  (reliability + latency are non-negotiable).
- Test fixtures that registered hosts purely in-memory must now seed host rows
  in their mock store — the mocks already implement `list_active_hosts`.
- `HostHeartbeat` (the new trait-level struct) replaces the 4-arg
  `touch_host_heartbeat` signature; mock-store fan-out is mechanical.
- The heartbeat UPDATE grows three JSONB columns (~1–10 KB/host/5s) — well
  inside PG comfort; `local_snapshots` is the largest and already bounded by
  the host's snapshot retention.

## Alternatives considered

- **pg NOTIFY fan-out of heartbeats to all replicas** (keep the in-memory
  mirror, sync it). Rejected: N caches with eventual-consistency races for
  state that has one natural home; the read is cheap and the write already
  happens.
- **Sticky routing (session→replica affinity at the LB)** for broker tokens
  and SSE. Rejected: doesn't fix placement/cordon, breaks on pod death — the
  exact case HA exists for.
- **Redis/memcache for tokens + pins.** Rejected: a second stateful system to
  operate for data PG already transacts with the session row.

## Validation

- Pure `rank_hosts` unit tests (filtering, affinity prefix, freshness, cordon).
- Live-PG two-`AppState` test over one database: cordon on A excludes the host
  from B's placement; teleport pin written on A honored by B's scanner; broker
  token minted on A authorizes on B; a heartbeat fielded by A makes the host
  schedulable from B.
- Existing e2e suites prove the demotion didn't change single-replica behavior.
- Prod: coord `replicas: 2` + kill-one-replica checks (engrams-internal half).
