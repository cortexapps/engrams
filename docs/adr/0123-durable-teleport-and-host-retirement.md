# ADR 0123: Durable session teleport and the host retirement grant

Status: Accepted (2026-10-05); phases S1-S6, C1-C4, O1-O2, X1 implemented in the PR chain listed under Phases.

Terms used in this document:

- **Teleport**: the product verb that moves one session from a source
  host to a destination host. It has two kinds. A **live** teleport uses
  the post-copy page server (ADR 0045 C2). A **snapshot** teleport pauses
  the guest, captures a memory snapshot, and restores it on the
  destination. The old names "evacuation", "evac resumer", "live
  migration", and "relocation" all mean a teleport.
- **Teleport row**: one row in `session_teleports`. It is the durable
  journal of one move. Its `phase` column is the recovery cursor.
- **Retirement grant**: the coordinator's durable proof that a host holds
  nothing and depends on nothing. The host row reaches `status =
  'retired'` only through the grant transaction.
- **Generation**: one value of `sessions.binding_epoch`. One harness
  process attaches under one generation. A newer generation fences an
  older one (ADR 0073).
- **Attach token**: the pair `(sandbox_id, binding_epoch)` a harness
  presents when it attaches. Today it is two environment variables.
- **Resident sandbox**: a sandbox the host still holds resources for.
  Running and tearing-down sandboxes are both resident.

## Summary, in plain English

Issue #1549 showed that a planned scale-down can lose a session's run.
The operator drained a host that held one active session. Live teleport
was refused because the guest runs ephemeral swap (ADR 0112). The drain
captured a snapshot of the running guest. The operator's drain gate
passed on a heartbeat count of zero sandboxes. The operator deleted the
cloud node before the coordinator agreed, ignored the coordinator's
refusal, and reported success. The session fell to `Idle`, a 600 s lease
hid the dead host, and the harness that survived in the memory image was
fenced out by a new binding epoch. The original run never completed.

A source audit confirmed 15 defects across the operator, the
coordinator, the host agent, the Firecracker backend, the guest agent,
and the harness SDK. They share three root causes:

1. **No proof before destruction.** Node removal trusts a heartbeat
   count. The coordinator's check runs after the node is gone.
2. **No durable journal for a move.** Three modules (`evacuation.rs`,
   `evac_resumer.rs`, `live_migration.rs`) each own part of a move with
   unfenced writes, in-memory phase state, detached tasks, and a
   give-up path that drops the session to `Idle`.
3. **No continuity for the harness.** A restored harness keeps its old
   token, is fenced as `Superseded`, and exits. Events have no sequence
   numbers and no acknowledgement. Nobody settles the open run.

This ADR replaces those paths with three abstractions and deletes what
they subsume:

- **A. The retirement grant.** One transaction proves a host is empty.
  The operator removes a cloud node only after it observes the grant.
  Scale-down and stuck-roll repair use the same contract.
- **B. One durable teleport state machine.** One table, one
  `session_ops` verb, one scanner. Every entry point (operator retire,
  admin drain, UI teleport) enqueues the same verb. Every phase is
  recoverable by a successor coordinator from the row.
- **C. Harness generation continuity.** Every rebind mints a new
  generation. The guest agent delivers the new token to a retained
  harness through a file. Events carry sequence numbers and are
  acknowledged only after the coordinator stored them. The coordinator
  settles the open runs of a superseded generation exactly once.

Plus four substrate corrections (D) and the operator rewrite that
consumes the grant (E).

## Context: the verified findings

Line numbers are at `a100eaf58`.

| # | Finding | Evidence |
|---|---|---|
| 1 | Node removal happens before authoritative drain completion. | `engram-host-operator/src/autoscale.rs:763-784` passes on `running_sandboxes == 0`; a `NotFound` host also passes, and `GetHost` is `NotFound` for `dead` hosts. `:681-697` removes the node, then calls `DeleteHost`, logs the refusal, and returns `Removed`. |
| 2 | Snapshot rehome fences the harness the snapshot preserved. | `evac_resumer.rs:656` mints a new epoch. `engram-agentd/src/harness_supervisor.rs:256-318` reuses the live child and signals `SIGUSR1`, but the child keeps its spawn-time epoch. `engram-host-agent/src/harness.rs:684` rejects it as `Superseded`. |
| 3 | The post-copy disk seal omits unreadable dirty chunks. | `disk_daemon/backend.rs:2342-2399` returns a tuple; a failed read is logged and omitted. |
| 4 | The evacuation commit is partial, unfenced, and the claim never heartbeats. | `evacuation.rs:495-527`; `engram-postgres/src/lib.rs:3306` plain `UPDATE`; `evac_resumer.rs:180-192`; reclaim after 180 s (`session_ops.rs:48`) while a restore may take 240 s. |
| 5 | The post-copy role is persisted outside the cancellation-safe restore. | `pooled_backend.rs:2240-2299` and `:9392-9411`; persistence errors are warnings at `:2438-2451`. |
| 6 | Destroy acknowledges before teardown completes. | `engram-sandbox-firecracker/src/lib.rs:5631` removes the map entry first; `list` at `:5684` hides the teardown; pooled `destroy` at `:9471-9590` cleans up after the await. |
| 7 | Abandoned live moves have no durable stage replay. | `session_verbs.rs:35-39`; the claim payload is only the target. The kill arm at `live_migration.rs:952-958` uses an illegal `Evacuating -> Failed` edge. |
| 8 | Rollback declares `Active` while the source stays paused. | `live_migration.rs:434-450`. |
| 9 | Move success does not require a usable harness, and recovery stops early. | `live_migration.rs:606-618`; `evac_resumer.rs:696-713`; `boot_materializer.rs:362-421`; `api/snapshot.rs:2191-2224`; budget exhaustion at `evac_resumer.rs:265-306`. |
| 10 | Destination capacity is checked, not reserved. | `placement.rs:984-1022` reads unlocked totals; `pick_host_2d` (`lib.rs:531-569`) is never used by evacuation; `MIGRATION_GATE` is process-local. |
| 11 | Grow-only scaling can send a count-only shrink. | `autoscale.rs:813-822, 897-899` compares with the pod count; the actuators send absolute sizes. |
| 12 | Event delivery can lose terminal events; a replacement does not settle old runs. | `engram-harness-sdk/src/lib.rs:304-334`; `harness.rs:858-871`; `harness-claude/src/main.rs:3496`; `harness-codex/src/main.rs:1161`; `coord_client.rs:219-239` has no retry. |
| 13 | Swap arming races the capture cut. | `pooled_backend.rs:2795-2842` disarms under the capture lock; `start_agent` at `:9594-9615` takes no lock. |
| 14 | Long drains block demand response. | `autoscale.rs:617-626` awaits victims one by one up to 600 s each; demand is read once. |
| 15 | Operator fallback and partial mutations lose intent. | `main.rs:59-82` selects a no-op scaler on error; `:641-650` and `:667-674` are three unguarded mutations. |

## Decisions

### A. The retirement grant

**A1. `hosts.status` gains `retired`.** A host is `ready`, `draining`,
`dead`, or `retired`. `retired` is terminal for the row: a heartbeat
cannot revive it, and `upsert_host` keeps it. The only exit is
`DeleteHost`.

**A2. The grant is one transaction.** `grant_host_retirement(host,
now)` locks the host row with `FOR UPDATE` (the same lock `pick_host_2d`
takes, so no placement can race it) and re-evaluates every blocker
inside the transaction:

| Blocker | Predicate |
|---|---|
| `bound_sessions(n)` | sessions with `host_id = host` in `host_memory_reserving_states` |
| `capture_jobs(n)` | `capture_jobs` with `stage NOT IN ('done','failed')` |
| `open_teleports_as_source(n)` | non-terminal `session_teleports` with `source_host_id = host` |
| `open_teleports_as_dest(n)` | non-terminal `session_teleports` with `dest_host_id = host` |
| `pending_tombstones(n)` | `sandbox_tombstones` rows for the host |
| `resident_sandboxes(n)` | the last heartbeat's `running_sandboxes` (see D3) |
| `enable_work(n)` | `live_enable_work_by_host` (ADR 0088) |
| `no_heartbeat_since_request` | `last_heartbeat_at <= retire_requested_at` |
| `not_cordoned` | `cordoned = false` |

With zero blockers the transaction sets `status = 'retired'`,
`retired_at = now`, and `lease_state = 'none'`. With any blocker it
rolls back and returns the list.

**A3. `RetireHost` is the one entry point.** The RPC is idempotent and
returns promptly. It cordons the host with an owner, enqueues a teleport
for every resident session (B4), evaluates the grant once, and returns
the retirement status. A caller repeats it until `retired_at` is set.
`AdminDrainHost` is the same request with `owner = admin`: the scanner
re-plans it every tick and grants it when the host is empty, so a
resident that does not fit today is tried again instead of lingering
behind a one-shot plan. `UncordonHost{owner: admin}` cancels the request
before the grant. `GetHost` reads the row directly, so `dead` and
`retired` hosts are visible; `NotFound` means "no row".

**A4. `DeleteHost` requires the grant.** It succeeds on `retired` or
`dead` rows and is idempotent on a missing row. On any other status it
returns `FAILED_PRECONDITION`. The old `SessionsBound(n)` outcome is
deleted; the grant already proved the count.

**A5. Cordons have an owner.** `hosts.cordon_owner` is `operator` or
`admin`. `UncordonHost` requires the matching owner and cancels a
pending retirement. The operator never releases a cordon it does not
own. This amends ADR 0047, whose cordon had no owner.

**A6. Heartbeats from `dead` and `retired` hosts are refused.** The
HTTP handler answers `410 Gone`. The host agent re-registers;
`upsert_host` pins a `retired` status. Today a dead host's heartbeat is
accepted and reconciled, which is its own bug; both arms are fixed
together.

**A7. No lease shield on the retire path.** The handoff declaration
(ADR 0116 A-D2) stays for image rolls, where the host comes back. The
retire path does not need it: the host is cordoned, every resident
leaves through the teleport machine, the host's own post-request
heartbeat reports zero resident sandboxes, and only then is the row
`retired`. There is no window in which the host is both bound and
silent.

### B. One durable teleport state machine

**B1. Naming.** The verb is `OpKind::Teleport` (unchanged). The table
is `session_teleports`. The module is `engram-coordinator/src/teleport.rs`.
The types are `TeleportRow`, `TeleportPhase`, `TeleportKind { Snapshot,
Live }`, `TeleportReason { RetireHost, AdminDrain, Ui }`. The word
"relocation" is not a code name.

**B2. No feature flag.** `ENGRAM_LIVE_TELEPORT` and
`live_teleport_enabled()` are deleted. The admit step picks `Live` when
both hosts advertise the post-copy capability. If the source host
refuses the live capture (ADR 0112 swap, no page server, no checkpoint
chain, not a substrate sandbox), the row downgrades to `Snapshot` in
place and the move continues. Making live capture legal for a swapped
guest is out of scope.

**B3. The row is the journal and the reservation.**

```sql
CREATE TABLE session_teleports (
  id                UUID PRIMARY KEY,
  session_id        UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  kind              TEXT NOT NULL CHECK (kind IN ('snapshot','live')),
  reason            TEXT NOT NULL CHECK (reason IN ('retire_host','admin_drain','ui')),
  phase             TEXT NOT NULL CHECK (phase IN
    ('admitted','captured','restored','committed','attached','done',
     'rolling_back','aborted','failed')),
  source_host_id    UUID NOT NULL,
  source_sandbox_id UUID NOT NULL,
  dest_host_id      UUID NOT NULL REFERENCES hosts(id),
  dest_sandbox_id   UUID,
  pinned_dest       BOOLEAN NOT NULL DEFAULT false,
  mem_budget_mib    BIGINT NOT NULL,
  cpu_budget_vcpus  INT NOT NULL,
  snapshot_id       UUID,
  export_id         TEXT,
  attempts          INT NOT NULL DEFAULT 0,
  error             TEXT,
  created_at        TIMESTAMPTZ NOT NULL,
  updated_at        TIMESTAMPTZ NOT NULL,
  finished_at       TIMESTAMPTZ
);
CREATE UNIQUE INDEX session_teleports_one_open
  ON session_teleports (session_id) WHERE phase NOT IN ('done','aborted','failed');
CREATE INDEX session_teleports_open_by_source
  ON session_teleports (source_host_id) WHERE phase NOT IN ('done','aborted','failed');
CREATE INDEX session_teleports_open_by_dest
  ON session_teleports (dest_host_id) WHERE phase NOT IN ('done','aborted','failed');
```

Reservations stay derived (no counters). Every reserved aggregate
(`per_host_reserved`, `pick_host_2d`, `fleet_free_mib`,
`placement_no_fit_details`, and the sim twin) gains a third `UNION ALL`
arm: the row's budget on `dest_host_id` while `phase IN ('admitted',
'captured','restored','rolling_back')`. Before `committed` the session
row still reserves on the source. From `committed` on, `sessions.host_id`
is the destination and the session row reserves there, so the teleport
arm stops. `rolling_back` keeps the destination line until the
destination VM is destroyed.

**B4. Three entry points, one verb.** `RetireHost` and
`AdminDrainHost` enqueue one `Teleport` op per `Active` resident with
the idempotency key `teleport:{host}:{session}`. `Parked` residents
descend through `descend_parked_session` as today. `TeleportSession
{session_id, target_host?}` (the UI and admin verb; it replaces
`EvacuateSession`) runs admission synchronously inside the RPC, so a
caller gets a real error (`no_fit`, `not_active`, `target_cordoned`,
`target_lacks_capability`, busy lane) instead of an optimistic status
flip, and then enqueues the op. An explicit `target_host` is honored
under the full 2D capacity, cordon, and capability check and recorded
as `pinned_dest`.

**B5. Phases.** Every step performs its side effect and then one CAS,
`teleport_advance(id, from, to, patch, epoch)`, guarded by the row's
current phase and the session's `current_epoch`. A step whose CAS returns
`false` stops; the op executor's reclaim sweep hands the row to a
successor, which resumes from the phase on the row.

| Phase on entry | Step | Recovery after process loss |
|---|---|---|
| (none) | **admit**: resolve the budget; pick a destination inside the `pick_host_2d` locked transaction (candidates minus source, per-destination open-teleport cap); insert the row; flip `Active -> Evacuating` in the same transaction. `NoFit` leaves no row and the session `Active` on the source. | Nothing durable before the transaction. The idempotency key and the unique open index make a re-admit exactly once. |
| `admitted` | **capture**: snapshot kind: `snapshot_begin` + `snapshot_wait` on the paused source, record the snapshot, advance with `snapshot_id`. Live kind: `migration_presetup` (advance with `export_id`), spawn the destination restore, `migration_capture_postcopy`. A live refusal downgrades `kind` in place. Any capture error enters `rolling_back`. | Snapshot: re-issue the capture; an orphaned upload is swept by snapshot blob GC. Live: `export_id` set means presetup is done. |
| `captured` | **restore**: build the metadata, `restore` on the destination with the fence, advance with `dest_sandbox_id`. | A successor restores again; the first VM is unbound on the destination and is entombed by the stably-unbound sweep (ADR 0116 A5). |
| `restored` | **commit**: `teleport_commit` (B6). `None` enters `rolling_back`. | Idempotent: the phase is already `committed`. |
| `committed` | **attach**: bind the record at the new generation, `start_agent`, wait until `sessions.attached_binding_epoch >= new epoch` (C5), then fenced `Evacuating -> Active`. `HarnessPlan::None` flips at once. A deterministic spawn failure flips `Evacuating -> Created` and the move continues; the destination owns the session. | Bind, materialize, and `start_agent` are idempotent. |
| `attached` | **release**: snapshot kind: `destroy` the source sandbox; `Ok` or `NotFound` is the acknowledgement. Live kind: `migration_drain_wait`, `migration_commit`, then the same destroy. If the source host is `dead` or `retired`, the release is `SourceHostGone`. Any other error retries after 10 s with no budget. `teleport_release_source` writes the source tombstone and moves the row to `done`. | Destroy and drain are idempotent. |
| `rolling_back` | **rollback**: destroy the destination if any, then `resume` the source. The source must acknowledge before `Evacuating -> Active`. A failed resume retries; the row stays `rolling_back`. A dead source during rollback moves the row to `failed` and settles the session honestly (`Idle` with a durable row, else `Dead`). | Every sub-step is idempotent. |

Terminal outcomes are exactly `done` (Active on the destination),
`aborted` (Active on the source, no loss), and `failed` with a named
cause. There is no attempt budget that ends at `Idle`.

**B6. The commit is one statement.** `teleport_commit(id, epoch)`
updates `sessions.host_id`, `sessions.sandbox_id`, `sessions.binding_epoch
+ 1`, and `session_teleports.phase = 'committed'` in one statement under
`current_epoch`, and returns the new binding epoch. It does not write
the source tombstone: on a live teleport the source is still the page
server until the drain completes. The tombstone is written at release
(B5). This replaces the in-statement tombstone of `rebind_session_guarded`
for teleports.

**B7. Source release is a fact, not a count.** A source is released
when its `destroy` acknowledged (`Ok` or `NotFound`) or when its host is
durably `dead` or `retired`. A transport error is neither; it retries
and keeps the source host's retirement blocked by
`open_teleports_as_source`.

**B8. Session state edges.** `Evacuating -> Active` is added (attach and
rollback both end there). `Evacuating -> Queued` is removed: a
destination that cannot fit the session is an admission refusal that
keeps the session `Active` on the source, not a queue entry. `Evacuating
-> Failed` stays illegal; its only writer (`parachute_or_kill`) is
deleted. `Evacuating` remains the user-visible state while a row is open.

**B9. The dead-host lane defers to the machine.** The bulk orphan in
`mark_host_dead_if_lease_expired` excludes sessions whose open teleport
names the dead host as source; the machine releases them as
`SourceHostGone`. Destination death after commit is observed by the
machine as "the session is no longer `Evacuating`" and moves the row to
`failed`.

**B10. Deleted.** `evacuation.rs`, `evac_resumer.rs`, `live_migration.rs`
(including `MIGRATION_GATE`, `walk_back_to_active`, `parachute_or_kill`,
`confirm_source_teardown`), `core/types/evacuation.rs`,
`evacuate_session_core`, the `Evacuating` target of
`run_evict_pipeline`, the admin drain `JoinSet`, the teleport pin
(`teleport_target_*` columns and methods), `evac_attempts` and its
methods, `enqueue_evacuating_session_resume`, and the `EvacuateSession`
RPC. The `SnapshotId::new()` call that bypassed `services.entropy` goes
with the module.

**B11. A `TeleportFinished` session event** `{teleport_id, outcome:
done|aborted|failed, kind, dest_host_id, error}` is appended at every
terminal phase so the UI shows the result.

### C. Harness generation continuity

**C1. Every binding write mints.** `transition_session_created`,
`rebind_session_guarded`, and `teleport_commit` increment
`binding_epoch` in their own statement and return it. `mint_binding_epoch`
and `current_binding_epoch` are deleted, and so is the split between
`bind_session_routing` and `bind_session_routing_minted`. Live and
snapshot teleports both mint. This removes ADR 0073's live-move
exception ("the harness process survives, keep the epoch"); the
process still survives, and it receives the new generation (C2).
`current_epoch` stays a separate fence (ADR 0073).

**C2. The attach token is a file.** `engram-harness-proto::attach_token`
owns `AttachToken { sandbox_id, binding_epoch }` with `from_env_map`,
`env`, `load`, and `write_atomic`. The file is
`/run/engram/attach-token`, JSON, written by agentd on every
`SpawnHarness` before it signals a retained child (`SIGUSR1`) or execs a
fresh one. The SDK loads file-then-env and reloads before every dial.
`AgentSpec::attach_token_env`, `SANDBOX_ID_ENV`, and `BINDING_EPOCH_ENV`
move out of `engram-core`; the backends call `AttachToken::env`. The
clap arguments for the token are deleted from both harness binaries.

**C3. The fence.** Bind-before-spawn ordering is kept (ADR 0073). On
`Superseded` the SDK re-reads the token. A newer token means "redial
now". No newer token means "wait up to 15 s for one (woken by `SIGUSR1`
or a 500 ms poll), then exit". A stale process still exits; the window
between a VM resume and the token write becomes a transient instead of
a fatal fence. Swap re-arming on agentd reuse stays: a restored guest
has a fresh zero swap file and needs it (ADR 0112 D2).

**C4. Sequenced, acknowledged events.** `HarnessFrame` gains two
trailing variants: `SeqEvent { binding_epoch, seq, event }` (harness to
host) and `EventAck { seq }` (host to harness, cumulative). `seq` is per
harness process and strictly increasing from 1. `binding_epoch` is the
generation the SDK held when it **sequenced** the event, not when it
sent it: a replayed event produced under generation N still carries N
after the harness re-attached under N+1. The SDK keeps unacknowledged
events in a bounded outbox (1024) and replays them on reconnect; a full
outbox blocks the engine. The hub refuses a frame whose epoch is above
the attach epoch, acknowledges only after the sink returned `Ok`, and
the production sink retries the coordinator POST with a bounded
backoff, so an acknowledgement means the coordinator stored the event.
`HarnessEventRequest.delivery { binding_epoch, seq }` rides to the
coordinator, which deduplicates by `(session_id, binding_epoch, seq)`.
`Event` (variant 0) stays accepted for a harness resident in an older
memory image; it is sinked without an acknowledgement, as today.

**C5. Readiness and settlement share one column, and a continued run
is never settled.** `sessions.attached_binding_epoch` is the highest
generation whose first event the coordinator has seen; only an event
stamped with that generation (C4) can advance it, so a replay never
does. The teleport attach step waits for it before `Active`.

A snapshot teleport keeps the same harness process, and that process
keeps running the same run under the new generation. The harness is
the only party that knows this, so it says so: an engine that is
re-attached while a turn is in flight emits `HarnessEvent::RunContinued
{ run_id }` (a new trailing variant, persisted as `run_continued`); a
fresh engine announces `Idle` as today. When `attached_binding_epoch`
advances, `settle_harness_generation(session, epoch, continued)` appends
`run_interrupted { cause: harness_replaced }` for every open run of an
older generation **except the runs the advancing event references**
(`RunContinued { run_id }`, or any event that carries that `run_id`),
idempotently keyed per run. A replaced harness references no open run,
so every stale run settles; a continued harness names its run, so it
survives. The harness engines route their channel-closed exit through
their existing close-out block, so a non-fenced exit still emits
`RunInterrupted`; a fenced generation cannot report, and the settlement
is the authority. The orchestrator treats `run_interrupted` as a failed
run end and `run_continued` as a non-terminal lifecycle event.

**C6. Readiness is honest.** `materialize_snapshot_resume` returns
`Result<HarnessPlan { Spawn { .. } | None }>`; an explicit no-harness
session is `None`, every resolution error propagates.
`bind_harness_generation` returns `Result` and a failed bind blocks.
`HostClient::bind_session` returns `Result`. `finish_resume_to_active`
emits `Active` only after the plan is applied.

### D. Substrate corrections

**D1. The post-copy seal is a `Result`.** `seal_for_postcopy` fails on
any unreadable allocated chunk; the failure fires while `CaptureUnwind`
is armed, so the source resumes. The unreachable "chunk in no overlay"
arm becomes an invariant error.

**D2. Roles are part of the restore and capture transactions.** FC
`persist_sandbox_manifest(.., role)` returns `Result` and runs before
the restore guards disarm; a failed write destroys the half-made
destination. The pooled layer mirrors the destination role inside the
detached restore task. `set_migration_role` returns `Result`; the
source role is written before `CaptureUnwind` is defused. This amends
ADR 0045 C2's best-effort role.

**D3. Teardown keeps ownership.** `engram_core::teardown::TeardownRegistry`
holds one shared future per sandbox id while its resources are held.
`destroy` on FC, the pooled layer, and VZ joins the in-flight teardown
instead of returning early; a caller's cancellation cannot skip the
pooled cleanup because it runs inside the shared task. `list()` returns
running and tearing-down ids, so the heartbeat's `running_sandboxes`
means **resident**. `migration_commit` propagates a destroy failure. VZ
`destroy` of an unknown id returns `Ok(())` like the other backends.

**D4. `start_agent` serializes on the capture lock.** The pooled
`start_agent` takes the same per-sandbox lock `capture_phase` holds, so
a `SpawnHarness` cannot re-arm swap between the disarm and the pause.
This is the host-side race ADR 0112 D3 did not cover; the live-teleport
swap refusal is unchanged.

### E. The operator consumes the grant

**E1. One victim annotation.** `fleet.engram.io/victim` holds JSON
`{ fleet, kind: shed | repair, phase: retiring | removing, deadline }`
and is written in one Node patch together with `spec.unschedulable`.
`removing` is the only phase that must be durable: it means
`remove_node` has been or is being issued. Granted is observed from
`RetireHost` every tick, never stored. The old `scaledown-victim` key
is not read (clean break; roll the operator with no wave in flight).

**E2. The order invariant.** `RetireHost` -> observe `retired_at` ->
write `phase = removing` -> `remove_node` -> `DeleteHost`. `remove_node`
is the only irreversible step and is unreachable without the grant.
This supersedes ADR 0044 K3 ("gate = `running_sandboxes -> 0` and
`failures == []`") and ADR 0048 section 5 step 2, and reverses ADR 0044's
"no drain-status endpoint": `RetireHost`'s response is that endpoint.

**E3. Short reconciles.** Each victim gets at most one `RetireHost`,
one `remove_node`, and one `DeleteHost` per tick. There is no sleep or
poll inside a reconcile. Demand is re-read each tick; pressure releases
a shed victim that is still pending. A granted host is empty and is
removed unconditionally. A roll-stuck node becomes a `repair` victim
with no deadline that is never released. `maxConcurrentDrains` is
deleted; `maxShedPerWave` is the single in-flight bound.

**E4. Release retains intent.** Release is `UncordonHost { owner }`,
then clear the annotation. A failed step leaves the annotation, and the
next tick retries. An owner mismatch means the cordon is not ours; only
the annotation is cleared.

**E5. Grow-only against the cloud's own target.** `NodePoolScaler::
current_target` reads the pool's desired size (GKE: the sum of the
backing MIGs' `targetSize`; AWS: `DesiredCapacity`). `set_size` is called
only when the computed target is above it, and only when the computed
target is above the pod count. No accepted target is persisted; the
cloud holds it.

**E6. Explicit actuator.** `noop`, `gke`, or `asg` must be named.
`noop` is observe-only: the autoscale step computes and logs, and
mutates nothing. A `gke` or `asg` initialization failure fails startup.
This amends ADR 0122's detect-or-noop fallback.

**E7. Per-RPC timeouts.** The operator's coordinator channel gets a
10 s per-call timeout. `AdminDrainHost` is no longer an operator
dependency.

## Wire and RPC shapes

```proto
// fleet.proto
rpc RetireHost(RetireHostRequest) returns (RetireHostResponse);
message RetireHostRequest  { string host_id = 1; string owner = 2; string reason = 3; }
message RetireHostResponse { string host_id = 1; HostRetirement retirement = 2; uint32 teleports_planned = 3; }
message HostRetirement { string requested_at = 1; string retired_at = 2; repeated RetirementBlocker blockers = 3; }
message RetirementBlocker { string kind = 1; uint64 count = 2; }
// CordonHostRequest / UncordonHostRequest gain `string owner = 2`.
// HostView gains `string cordon_owner = 33; HostRetirement retirement = 34;` and status "retired".
// AdminDrainHostResponse becomes { host_id, repeated string planned, repeated string descended, uint32 skipped }.
rpc TeleportSession(TeleportSessionRequest) returns (TeleportSessionResponse);
message TeleportSessionRequest  { string session_id = 1; optional string target_host = 2; }
message TeleportSessionResponse { string teleport_id = 1; string kind = 2; string dest_host_id = 3; }
```

```rust
// engram-harness-proto
pub enum HarnessFrame {
    Event(HarnessEvent),                        // 0, legacy, un-sequenced
    Command(HarnessCommand),                    // 1
    SeqEvent { binding_epoch: u64, seq: u64, event: HarnessEvent }, // 2
    EventAck { seq: u64 },                                         // 3, cumulative
}
```

## Consequences

- Fewer paths: three coordinator modules, the teleport pin, the evac
  budget, the process-local gate, the operator's gate/drive/repair
  functions, the no-op scaler, and the live-teleport flag are deleted.
  One table, one module, one annotation, one RPC replace them.
- A drain that cannot fit a session no longer queues the session. The
  session stays `Active` on the source and the host's retirement is
  blocked, visibly, by `bound_sessions`.
- Bundle re-bakes: agentd, harness-claude, harness-codex. Deploy the
  coordinator and host agent before the harness bundles: a new
  `SeqEvent` against an old hub is a decode error. A harness resident in
  an old memory image is fenced and respawned as today; it never hangs.
- Migrations 0120 and 0121. The coordinator rolls before the operator.
- The 15 s `Superseded` grace is the one timing-bounded element. It is
  on the fence side only and never delays the success path.

## Phases

| PR | Content | Pull request |
|---|---|---|
| ADR | this document | #1582 |
| S1-S3 | D1-D4: seal is a Result, roles inside the restore and capture transactions, TeardownRegistry and the resident count, start_agent on the capture lock | #1584 |
| S4 | C2, C3: attach token file, SDK reload and fence grace, noop harness on the SDK | #1586 |
| S5-S6 | C4: SeqEvent with the sequencing epoch, EventAck, outbox replay, hub ack after the sink, sink retry, RunContinued; orchestrator run_interrupted | #1587 |
| C1 | A1-A6, B3, B8: migration 0120, Retired, the grant, RetireHost/GetHost/DeleteHost, cordon owner, the reservation arm | #1585 |
| C3 | C1, C5, C6: uniform mint, honest readiness, run settlement with the continued-run exemption, delivery dedup (migration 0121) | #1590 |
| C2 | B: teleport.rs, TeleportSession, snapshot_hold, live payload (migration 0122), deletions, scenarios, e2e | #1591 |
| O1 | E5, E6 | #1583 |
| O2 | E1-E4, E7 | #1588 |
| X1 | the KVM acceptance test: a real SDK harness survives a non-drained snapshot and finishes its run | #1589 |
| C4 | fixture cleanup: assign_session_sandbox retired; fenced binding writes carry strike reset and manifest clear | #1592 |

## Divergence log

- 2026-10-05 (review of the Proposed ADR): C5 as first written settled
  "every open run of an older generation" when the generation advanced.
  On a snapshot teleport the same process continues the same run under
  the new generation, so the run's own first event would have settled
  it: #1549 by policy. C4 and C5 now stamp the epoch at sequencing time,
  add `RunContinued`, and exempt the runs the advancing event references.
- 2026-10-05 (C2): the snapshot kind cannot use the eviction capture
  (`snapshot_begin` runs a finalizer that destroys the source) and cannot
  pre-pause the guest (the capture disarms swap over exec). The host gains
  `snapshot_hold`: the ordinary capture that leaves the VM paused and does
  not re-arm swap; `resume` re-arms once and `destroy` clears the hold. A
  post-capture execution window is not acceptable: events the source emits
  after the capture point carry epoch N and the same sequence numbers the
  destination replays, so dedup would drop the destination's real events.
- 2026-10-05 (C2): a live move persists its full presetup result in
  `session_teleports.live_payload` in the same CAS that records
  `export_id`, so a successor never calls presetup twice; an export the
  host no longer holds rolls the move back with `live_export_lost`. The
  export lifetime on the host is still TTL-driven; making it
  teleport-driven (B10) is the remaining host follow-up.
- 2026-10-05 (C2): a live rollback releases the source through
  `migration_abort` (the export fenced it); only a held snapshot source is
  released through `resume`.
- 2026-10-05 (C3): `HarnessPlan::None` means a dev-VM session. An
  agent-mode session with no persisted harness selection is an error,
  never a silent "no harness"; the previous code swallowed that error.
- 2026-10-05 (C1): `session_teleports.dest_host_id` has no foreign key
  (like `source_host_id`), so a retired host that served as a destination
  stays deletable. Pre-existing cordons are backfilled to the admin owner.
  `EnableWork` counts live materializes only; `CaptureJobs` already
  reports the capture jobs.
- 2026-10-05 (O2): a granted host is removed unconditionally; pressure
  releases only a shed victim that is still pending. A stuck image roll
  with autoscaling disabled still blocks further rolls.
- 2026-10-05 (O1, C1): the heartbeat's `running_sandboxes` is redefined as
  resident (running plus tearing-down) and the grant reads that column;
  no new wire field or table column.
- 2026-10-06 (C2, review): `AdminDrainHost` was a cordon plus a one-shot
  plan, so a resident that did not fit was reported as planned and never
  retried. It is now the retirement request with `owner = admin`, which
  the scanner re-plans every tick and grants when the host is empty.
- 2026-10-06 (C2, review): migration 0122 settles sessions left in
  `evacuating` by the retired scanner (no `session_teleports` row):
  tombstone the bound sandbox, then Idle with a recoverable snapshot and
  Dead without one. Nothing else drives that state from this version.
- 2026-10-06 (C2, review): a rollback declares Active only on a resume
  ack. A consumed live export (`migration_abort` answers NotFound) still
  resumes the source; a source sandbox that no longer exists fails the
  move with `source_lost_during_rollback`.
- 2026-10-06 (S1, review): a role clear runs after the unconditional
  abort, commit, and drain work, and its error is reported last; a
  finished destination drain is remembered so a failed role persist is
  retried by the next `migration_drain_wait` rather than reported as a
  lost drain.
