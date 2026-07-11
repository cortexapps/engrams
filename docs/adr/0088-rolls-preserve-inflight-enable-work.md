# ADR 0088: Fleet rolls must not kill in-flight enable work

Status: Accepted

## Context

Every push to `main` auto-rolls the host fleet (ADR 0044 K3: the operator's
drain-gated, node-by-node pod swap). The image roll deliberately does **not**
drain: ADR 0044 K2 keeps the node's microVMs alive across the pod swap
(`hostPID`/`hostNetwork` + pidfd reattach), so `roll_node` cordons and then
deletes the pod immediately — evacuation would rewind sessions for no reason
when the VMs never die.

That reasoning is correct for *session VMs* and wrong for *enable work*. An
in-flight image **materialize** (the streaming `MaterializeImage` RPC: pull →
flatten → pack → chunk, ADR 0080 phase 3b) and a base-snapshot **capture**
(the `capture_jobs` executor + its capture VM, ADR 0084) are the host-agent
*process's own work* — SIGTERM ends them, and nothing in `roll_node` waits.

Prod evidence (dev-brain enable job `83310fad`, 2026-07-10): a deploy rolled
the fleet mid-enable and killed the materialize twice ("h2 protocol error:
error reading a body from connection"), then a third attempt died with the
host-agent ("removed orphaned materialize scratch (previous host-agent died
mid-run)"). Each retry restarts the ~55-min materialize from scratch (only
chunk-store PUTs dedup). The enable took 2h39m; a 2026-07-08 job burned all
5 attempts against roll/incident churn and went terminally `failed`. The
durable-job layer works — it converts "failed" into "slow" — but the attempt
itself has no protection.

What already works (and this ADR does not change):

- **New work is fenced.** All three host pickers (session placement, the
  reserving capture picker, `pick_materialize_host`) funnel through
  `host_is_schedulable`, which excludes `hosts.cordoned` — and `roll_node`
  cordons before deleting the pod. A retry never re-picks a mid-roll host.
- **Scale-down drains sessions.** The wave's `gate_drain` waits for
  `running_sandboxes == 0`, which *incidentally* covers a capture VM (it is
  registered in the backend and counted) — but not a materialize, which
  boots no VM.

The two gaps:

1. **The image roll has no work gate at all** — `roll_node` deletes the pod
   with in-flight materialize/capture on it.
2. **Materialize is invisible to the control plane.** `capture_jobs` carries
   `host_id` + a non-terminal-stage filter, but `enable_jobs` has no host
   column; the materialize placement lives only in the scanner's
   `advance_one` call stack and the host-side `materialize_gate` try-lock.

## Decision

Make in-flight enable work **visible** (PG, operator-queryable) and make both
roll paths **wait for it** (bounded, never wedging a roll).

### 1. Durable materialize placement (`enable_jobs.materialize_host_id`)

Migration 0102 adds a nullable `materialize_host_id UUID` to `enable_jobs`.
`materialize_image_on_host` stamps it (fenced by `claimed_by`, like every
enable-job write) immediately after `pick_materialize_host`, before the
streaming RPC starts — so there is no window where work is running but
unattributed.

**Liveness needs no new machinery**: the host's ≤30 s materialize keepalive
frames already renew the claim (`update_enable_job_materialize_progress` sets
`claimed_at = NOW()`), so *live* materialize on host X is exactly

```sql
state = 'materializing' AND materialize_host_id = X
  AND claimed_at > NOW() - <lease window>
```

A dead stream stops renewing and drops out of the gate within the lease
window (300 s default) — the gate can never wait on a ghost. The column is
never cleared: it is inert outside `state='materializing'` and doubles as a
"where did the last materialize run" breadcrumb.

### 2. Fleet-view surface (`live_materializes` / `live_capture_jobs`)

A new `MetadataStore::live_enable_work_by_host` aggregates, per host:

- **materializes**: the liveness predicate above;
- **captures**: `capture_jobs` rows in a non-terminal stage
  (`stage NOT IN ('done','failed')`, `host_id IS NOT NULL` — WAITING rows
  bind no host). Captures need no freshness filter: the enable scanner's
  stage deadlines already redrive-or-fail a stuck capture row.

Both counts ride `HostView` (api + `app.v1` proto fields 30/31, RUST-WIRE-ONLY
like `ready_images`) so the operator reads them over the same
`FleetService.GetHost` poll the drain gate already uses.

### 3. Operator gates

- **Image roll** (`roll_node`): after the cordon (so no new work can arrive)
  and before the pod delete, poll `GetHost` until both counts are zero, with
  a budget (`spec.enableWorkTimeoutSeconds`, default **5400 s**). Cordoned
  hosts receive no new enable work, so the wait is monotone: at most the tail
  of the one materialize (~55-90 min for dev-brain) or capture (warm timeout
  3300 s + freeze) currently running.
- **Scale-down wave** (`gate_drain`): the drained condition becomes
  `running_sandboxes == 0 && live_capture_jobs == 0 && live_materializes == 0`
  (materialize was invisible to the sandbox count).

**Timeout ⇒ proceed, loudly.** On budget exhaustion (or a small consecutive
RPC-error budget, e.g. the coordinator being down), the gate WARNs and the
roll proceeds — exactly today's behavior. A roll must never wedge on enable
work; the durable-job retry remains the backstop, demoted from the primary
mechanism to the rare fallback. (The scale-down wave keeps its existing
release-the-victim-on-timeout semantics.)

## Alternatives considered

- **Resumable materialize** (persist scratch, resume the stream): the big
  hammer. Most of its value evaporates once rolls stop interrupting; not
  worth the complexity while the gate exists.
- **Heartbeat bit from the host's `materialize_gate` try-lock**: no
  migration, but leaves a pick→RPC-start window where work is running and
  invisible, and puts liveness on the heartbeat path instead of the existing
  claim renewal. The PG binding has neither problem and matches "state in PG
  + scanner" (the house pattern).
- **Long `terminationGracePeriodSeconds` + host-agent SIGTERM handling**
  (finish materialize before exiting): turns every pod delete into a
  potentially-90-min hang for kubelet, invisible to the operator's planner,
  and still doesn't cover the capture VM (whose executor spans heartbeats).
- **Prestage work in the gate**: deliberately excluded — prestage attempts
  are short (1200 s bound), per-host best-effort, and already retried; a roll
  interrupting one costs seconds.

## Consequences

- A fleet roll during a dev-brain enable waits (up to 90 min on the one node
  running the work) instead of destroying 40-90 min of materialize/warm
  progress; enables land in one attempt, and the attempts budget stops being
  consumed by deploys.
- Rolls of *idle* hosts are unaffected (both counts zero → gate passes on the
  first poll).
- `enable_jobs` gains a host column; the fleet view gains two counts — both
  also useful for operability ("what is this host doing right now").
- The 5400 s default budget means a pathological enable can delay (not block)
  a roll by up to 90 min per affected node. Operators can lower
  `enableWorkTimeoutSeconds` (0 disables the gate entirely = today's
  behavior).

## Implementation notes / divergences

- Liveness came out even simpler than proposed: `claimed_at` freshness
  needed **zero** new renewal machinery — the materialize keepalive frames
  were already renewing the claim (`update_enable_job_materialize_progress`).
  The fleet view shares the scanner's default lease window
  (`DEFAULT_ENABLE_JOB_LEASE_SECS`) so gate-release and peer-reclaim happen
  on the same clock.
- `GetHost` deliberately does NOT best-effort the live-work read (unlike
  `reserved`): a failed read rendering zeros would tell the gate "no work"
  and let a roll kill a live materialize. `ListHosts` (view-only) stays
  best-effort.
- The CRD yaml is generated with structural pruning, so
  `enableWorkTimeoutSeconds` had to ride the checked-in CRD too — a CR
  field absent from the schema is silently dropped, which would have made
  the helm knob a no-op.

## Commits

- ADR (Proposed)
- `store: durable materialize placement + per-host live enable work` —
  migration 0102, `set_enable_job_materialize_host`,
  `live_enable_work_by_host`, live-PG tests
- `coordinator: stamp materialize placement; expose live enable work on
  HostView` — the fenced stamp in `materialize_image_on_host`, HostView +
  app.v1 proto fields 30/31
- `operator: gate rolls and drains on in-flight enable work` —
  `gate_enable_work` in `roll_node`, the `gate_drain` enable-work leg,
  `enableWorkTimeoutSeconds` (CRD + helm)
