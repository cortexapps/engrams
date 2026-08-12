# ADR 0116: Binding leases — ownership is granted and released, never inferred

- Status: Proposed
- Date: 2026-08-12
- Implementation record: PR #1212 (workstream B, PR B1: the boot
  materializer + cold-boot slot fix; merged 2026-08-12) landed as
  groundwork before this document. Later PRs are recorded here as they
  land.
- Related: ADR 0090 (sandbox ownership is coordinator truth — refined
  here), ADR 0068 (probe-before-flip), ADR 0079 (session-op executor +
  fencing), ADR 0028 (eviction durability, Fix B cold boot), ADR 0045
  (drain-only evacuation), ADR 0088 (rolls must not destroy enable work),
  ADR 0098/0099 (DST + invariants), ADR 0044 (K8s host fleet, K3 roll
  operator), ADR 0055/0062/0080 (dynamic slots), ADR 0049 (NBD device
  slots), ADR 0007 (chunked storage).

## Terms used in this document

- **Binding**: the pair `sessions.host_id` / `sessions.sandbox_id` — the
  coordinator's record that a host holds a live VM for a session.
- **Host-affirmed absence**: a signal derived from a successfully
  delivered report by the host generation that holds the lease, which
  positively asserts a sandbox is not running.
- **Silence**: the absence of heartbeats or probe answers. Silence is
  not a report.
- **Planned operation**: a roll, drain, or scale-down that an operator
  or the host itself initiates and can announce in advance.

## Context

On 2026-08-12 one session tree hit three failure classes in one
afternoon. The incident is the motivation; the pattern behind it is the
subject of this ADR.

1. **Control plane.** A routine host-agent deploy roll replaced a pod in
   5.5 minutes. The dead-host detector (staleness threshold + probe
   strikes) orphaned a session on that host 44 seconds before the
   successor pod adopted its still-running VM. The successor reattached
   the VM successfully; then the host's teardown reconciler destroyed
   the healthy VM, because PG no longer pointed at it. Every component
   followed its own contract. The system still killed a healthy VM.
2. **Lifecycle.** The subsequent resume took the disk-only cold-boot
   path, which built a guest whose harness argv pointed at
   `/opt/engram/dyn/0/…` with nothing mounted there. The retry loop
   re-ran the identical failing finish 60 times over 50 minutes, then
   failed the session. (Fixed by PR #1212; see workstream B.)
3. **Data plane.** A neighbor session's checkpoint capture uploaded
   chunks at 100-300 × 512 KiB PUTs per second. A live session's NBD
   serve loop missed the kernel timeout; the kernel declared the
   connection dead; the guest saw permanent EIO. The host-side flush
   then failed silently every 30 seconds for more than 7 hours, and a
   rung-2 park landed on the wedged device with nothing durable
   captured. The device stayed netlink-configured and re-servable the
   whole time; nothing re-served it.

The diagnosis, common to all three: **the system treats silence as
evidence of death, and treats unbinding or rebuilding as the recovery
primitive.** About 3,500 lines of scanner and reconcile machinery
(`dead_host.rs`, `teardown_reconcile.rs`, `evac_resumer.rs`,
`live_attach.rs`, `harness_desync.rs`) soften the consequences. Each
heuristic in them is a scar from a prior incident (prod 4497bd2f, #762,
#769, #776, #777, 7eddce62, campaign B1, #1012). The heuristics race
each other: on 2026-08-12 the coordinator-side rescue machinery (#777)
lost the race to the host-side reaper, because the #777 defer only
protects rows stuck at `HostLost`, and this row had already settled to
`Idle`.

Two structural observations sharpen the diagnosis:

- The roll was a *planned* operation. The operator cordoned the host
  before deleting the pod. But `hosts.cordoned` is one boolean with no
  reason and no deadline; the only protection it grants is a silent 10×
  staleness multiplier in `list_stale_hosts`. The roll exceeded that
  implicit grace by under a minute. The mechanism existed; it was an
  approximation, not a contract.
- The host says nothing at shutdown. The SIGTERM ladder aborts the
  heartbeat task first, flushes, and deliberately leaves VMs for the
  successor — and no part of that intent reaches the coordinator.
  Adoption is equally implicit: a successful reattach is only visible
  as the sandbox re-appearing in the next heartbeat's
  `running_sandboxes`.

## The invariant

> A session→sandbox binding — and, on the data plane, a served NBD
> connection — is released only by the party that holds it
> (host-affirmed), or by expiry of an explicit lease whose deadline
> covers every planned operation. The coordinator never revokes on
> inference. Recovery re-derives its plan from current state; it never
> re-executes a failed plan verbatim.

This **refines ADR 0090**, it does not reverse it. The coordinator
remains the single authority on ownership. What changes is the set of
admissible inputs to *revocation*: a host report, or lease expiry. Row
staleness, failed probes, and missing host-local state are no longer
grounds to clear a binding.

The invariant has a data-flow corollary, learned during B1's review
(#1212): **an input whose necessity only the callee knows is passed as
a computation, not a pre-resolved value.** `evacuate_dead_source` used
to take an eagerly-materialized cold-boot spec that only its disk-only
rung consults; every caller then had to guess an error posture blind —
swallow (conflates structural with transient) or propagate (gates the
memory-snapshot rung on reads it never makes). Both postures were
wrong because the parameter shape was wrong. The spec is now an
un-awaited future the disk-only rung alone polls, and the rung-1 tests
pass a poison future that panics if polled. Apply the same test to new
seams this ADR introduces: if a caller must pre-resolve something a
callee may not need, move the resolution behind the seam.

The success metric is a net-negative diff: the lease and the tombstone
replace the strike counters, grace windows, and rescue heuristics that
approximated them.

## Decisions

### Workstream A — the binding lease (control plane)

**A-D1. Lease shape: per-host columns on `hosts`.** Migration
`0114_host_binding_lease.sql` adds `lease_expires_at TIMESTAMPTZ`
(NULL = legacy host; enforcement falls back to
`last_heartbeat_at + LEASE_TTL`), `lease_state` (`none` | `active` |
`handoff`), and `lease_epoch BIGINT` (host-agent generation, bumped by
register). The host is the failure domain — the incident was a pod
replacement that affected every VM on the node — and
`touch_host_heartbeat` is already the single per-heartbeat writer, so
renewal is one more column in the same UPDATE with the same injected
clock. Renewal uses `GREATEST(existing, now + LEASE_TTL)`: a
predecessor's last racing heartbeat can never shrink a handoff deadline.
Defaults: `LEASE_TTL` 45 s; operator handoff = drain timeout + 120 s;
host-ladder handoff 600 s. All env-tunable.

This is not the `session_lease` that migration 0093 removed. That was a
wall-clock lease that arbitrated mutual exclusion between coordinator
replicas, and the durable op log replaced it correctly. The binding
lease is a host↔coordinator liveness contract whose deadline is written
data — a host- or operator-declared fact, not a timeout heuristic
arbitrating peers.

**A-D2. Handoff: one endpoint, two writers.** A new route
`POST /api/hosts/:id/handoff {ttl_secs}` computes the deadline on the
coordinator's clock. The **operator** (which knows roll-vs-scale-down
intent) calls it immediately after `cordon`, before the pod delete; a
handoff failure aborts the roll exactly as a cordon failure does. The
**host SIGTERM ladder** is the belt for kubelet-initiated restarts the
operator never sees: a new first rung (before the heartbeat task is
aborted) writes a durable `handoff.json` marker beside the binding
records and makes one best-effort POST. Scale-down writes no handoff —
there is no successor by intent; the drain discipline is unchanged.
Where no ladder runs (SIGKILL, preemption), the lease expires at
`LEASE_TTL` and the death path is the correct, explicit outcome.

**A-D3. Adoption renewal.** Register bumps `lease_epoch`, sets
`active`, and renews; the successor's adoption ends a handoff early.
The coordinator renews the lease on **every** heartbeat regardless of
host version — correctness is entirely coordinator+operator-side, and
legacy hosts get the fallback. A host that answers the death-path probe
gets a durable renewal written on its behalf; this replaces the
in-memory `probe_rescue_grace` / `ProbeMemory` machinery with a written
fact. Per-sandbox custody stays on the existing channels
(`rehydrate_sandboxes` expected list; `running_sandboxes` actual list).

**A-D4. The death path.** `dead_host.rs` shrinks to:
`list_lease_expired_hosts()` → the existing `dead_host_inflight`
cross-replica claim (unchanged — an executor lease, a different domain)
→ one Ping probe (answered ⇒ renew + warn + return) →
`mark_host_dead_if_lease_expired` (the bulk orphan, amended: expiry is
re-checked under the row lock so a racing renewal aborts the mark, and
a tombstone is written per cleared binding in the same transaction) →
one consolidated `settle_host_lost()` stage-2.

Retired, each subsumed by the lease: `stale_threshold`,
`min_probe_failures`, `probe_rescue_grace`, `ProbeMemory(Map)`,
`probe_failure_permits_eviction`, `list_stale_hosts` and the cordon 10×
multiplier, the "no client and no host_addr ⇒ legacy immediate
eviction" arm, `straggler_serving_strike_cap` + `StragglerStrikeMap` +
the destroy-despite-alive arm, and the three duplicate `HostLost`
stage-2 implementations (consolidated; `recovery_target` remains THE
predicate).

**A-D5. Host-affirmed absence, and tombstones.** The qualifying
channels stay: absence from `running_sandboxes` with
`running_sandboxes_known = true` (flip_missing's `missing_strikes`,
plus the ADR 0068 probe-before-flip), `unreachable_guests`,
`quarantined_survivors`, and the host's own destroy. Row staleness,
failed probes, and missing host-local bindings no longer qualify.

Migration `0115_sandbox_tombstones.sql` adds
`sandbox_tombstones(host_id, sandbox_id, session_id, created_at)` —
explicit disownment the host consumes. Written by the bulk orphan
in-transaction and by every site that unbinds after a failed destroy
RPC (audited via the `binding_writer_inventory` ratchet). Delivered via
a new `#[serde(default)]` `HeartbeatResponse.tombstoned_sandboxes`
field (JSON heartbeat: additive, no wire bump); acknowledged by the
sandbox leaving `running_sandboxes`, at which point the row is deleted.
This closes the partition-heals hole: a marked-dead host that returns
destroys its OWN disowned VMs. On the host, `teardown_reconcile`'s
bound arm (the PG-ownership poll) and `ORPHAN_STRIKES` retire; the
unbound arm keeps its coordinator-confirmed-absence discipline and
re-keys its create→bind debounce on sandbox-manifest age.

**A failure matrix** (proved in design review; the DST scenarios in
`## Verification` hold each row): planned-slow-roll, SIGKILL without a
ladder, node preemption, coordinator restart mid-roll (a per-replica
startup grace of one TTL), partition-then-heal, partial adoption, and
scale-down. In every row, a VM that some host reports alive is only
ever destroyed BY that host, acting on an explicit coordinator fact —
a tombstone or a confirmed no-owner answer.

### Workstream B — one materializer, re-plan resume (lifecycle)

**B-D1. The materializer** (landed, PR #1212). One module,
`boot_materializer.rs`, owns dynamic-slot assembly for every boot
flavor with per-flavor typed outputs. The create path and the disk-only
cold boot derive the same `SlotPlan` (create from request inputs; cold
boot from the session's persisted harness/skills, with read errors
propagating rather than degrading). Capture keeps the sentinel shape by
design. `check_argv_slot_agreement` — a spec must be able to exec what
its argv names — is an always-on `invariant!` (ADR 0099 site 7).

**B-D2..D4 (planned).** The snapshot-resume agent-spec builder folds
into the materializer, retiring the dropped-mount-tuple pattern. Spawn
failures gain a typed `HarnessSpawn{kind}` error carried over the
existing gRPC status with a marker message (the `wire_skew_message`
precedent — no wire bump, mixed-fleet safe). The resume verb re-plans
instead of re-trying: a deterministic spawn failure routes to
`rebuild_binding` — factored from the Unreachable-recovery arm and
gated on `confirm_source_teardown`, consistent with the invariant
(host-affirmed release) — so the next attempt genuinely
re-materializes. The crash-shortcut / full-dispatch fork collapses into
plan derivation from (recorded step, binding state, failure class).
`RESUME_MAX_ATTEMPTS` stays as the outer budget; attempts become
monotonic progress. The misleading "falling through to full dispatch
(re-restore)" log becomes true by construction.

### Workstream C — NBD data-plane resilience

**C-D1. Serve-loop supervisor.** The invariant's data-plane form: a
serve loop may be down only via Drop, `abandon()`, shutdown-abandon
mode, or destroy. Any other exit is re-served — fresh socketpair +
netlink RECONFIGURE against the same live backend (the mechanism the
pod-roll rehydrate already uses), within the kernel's
`dead_conn_timeout` park window. A restart budget (5 per 10 minutes)
escalates to quarantine on exhaustion. Multi-connection NBD is
rejected: a spare socket served by the same starved process dies of
the same cause.

**C-D2. Upload QoS.** The starvation mechanism (one background
checkpoint re-chunk consuming the whole host-global 96-permit upload
budget on the single shared runtime) is confirmed by PR C1's metrics
before tuning. The mitigation is upload classes (background capture
capped well under the global budget) plus a dedicated two-thread serve
runtime for the NBD reader/writer, converting "neighbor storm ⇒ device
death" into "neighbor storm ⇒ guest latency".

**C-D3. Loud failure.** N consecutive device-class flush failures fire
a `DataPlaneHealth` report into `quarantined_survivors` (new additive
`reason` field), engaging the existing quarantine-evict ladder for a
bounded capture. `pause()` refuses with a typed non-retryable error
when the data plane is failed, so a park can never again "succeed"
non-durably on a wedged device; the full evict never refuses — it is
the remediation.

## Phasing

Three PR tracks proceed in parallel after this document lands; each PR
is independently green.

- **A1** lease substrate (migration 0114, store methods + conformance,
  shadow-disagreement metric) → **A2** handoff writers (route, operator
  call, SIGTERM rung) → **A3** death-path cutover (the net-negative PR;
  DST incident scenario) → **A4** tombstones + straggler slim +
  `settle_host_lost` → **A5** host-side retirement (gate: coordinator
  fleet ≥ A4) → **A6** discipline audit (`delete_host`,
  `settle_evicted_session_idle`, `delete_pending_session`; operator
  comment fix).
- **B1** (landed, #1212) → **B2** resume fold → **B3** typed
  `HarnessSpawn` → **B4** re-plan + fork collapse.
- **C1** observability → **C2** serve supervisor → **C3** upload QoS →
  **C4** loud failure + park refusal.

Mixed-fleet rules: the coordinator renews for every heartbeat
regardless of host version; `lease_expires_at IS NULL` falls back to
`last_heartbeat_at + TTL` (no flag day); a new operator against an old
coordinator treats a handoff 404 as warn-and-proceed (the 10× cordon
shield still applies); A5 requires every coordinator ≥ A4.

## Verification

The invariants become checked DST oracles, swept per sim step /
quiescence over the seed corpus, plus always-on prod sites per ADR 0099
H6 (each gets a line in 0099's site list):

- **Lease-liveness** (engram-dst): no session loses its binding while
  its host's lease or handoff deadline is unexpired in the world model.
- **No-coordinator-destroy-of-running**: no destroy effect targets a
  sandbox present in the world's host running set without a tombstone
  or confirmed-no-owner fact.
- **Tombstone convergence** (quiescence): every tombstone is consumed
  or its host row is dead.
- **Re-plan progress** (recovery oracle): a deterministic spawn failure
  changes plan within the shortcut budget; no two identical consecutive
  plans; the session reaches Active or Failed within the outer budget.
- **Serve-loop supervision** (engram-dst-host): a configured device's
  serve loop is running or explicitly released; an unsolicited exit is
  re-served within the restart budget or escalates.
- **Flush-livelock**: no sandbox accumulates more than N device-class
  flush failures without a `DataPlaneHealth` report.
- The keystone scenario replays 2026-08-12: cordon + handoff, 5.5
  sim-minutes of predecessor silence, successor adoption — the session
  must never leave Active and its binding must never clear; a control
  leg without a handoff must settle Idle after expiry.
- Prod alerting: shadow lease-vs-staleness disagreement (A1, until
  cutover), serve-restart budget exhaustion, flush-escalation fires,
  tombstone age high-water (alert policies tracked in engrams-internal).

## Non-goals

- Executor-lease timeouts stay: `session_ops` reclaim,
  `PENDING_ORPHAN_GRACE`, the queue timeout, `ACK_TIMEOUT`, the
  `dead_host_inflight` TTL, and the evac attempt budget arbitrate
  replica failover and op progress, not VM liveness.
- No register-time resurrection of already-orphaned sessions from the
  handoff marker: that would be re-binding by inference.
- The 2026-08-12 operational cleanup (the wedged sandbox, the stale
  cordon, autoscaler churn) is handled operationally, not here.

## Risks

- **The double-run window.** A partitioned host's VMs keep running
  until the partition heals and tombstones arrive. If a user resumes
  the session elsewhere in that window, two VMs briefly exist — the
  same exposure as today; the resume-from-idle confirmed-teardown gate
  covers the Idle arm, and the tombstone bounds the window where today
  nothing does.
- **Heartbeat-persist shared fate.** Lease renewal rides
  `touch_host_heartbeat`; a PG outage stops renewals fleet-wide. The
  per-replica startup grace and the death-path probe-rescue-renew bound
  the blast radius: a probe-answering host cannot be marked dead.
- **Handoff TTL misestimation.** A roll slower than the operator
  handoff deadline reverts to today's behavior (orphan on expiry). The
  deadline derives from the operator's own drain timeout, and the
  adoption metric (A2) makes real roll durations observable before A3
  cuts over enforcement.
