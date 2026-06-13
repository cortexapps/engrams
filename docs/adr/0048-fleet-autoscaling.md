# ADR 0048: fleet autoscaling — queue-aware scale-up, teleport-packed scale-down

**Status:** Proposed (2026-06-12)
**Related:** ADR 0044 K4 (scale-up policy + GKE actuator, shipped inert), ADR 0045 Phase E (scale-down decision engine #138; this ADR ships its actuator and E2), ADR 0046 (placement reservation), ADR 0047 (stateless coordinator — the prerequisite this work forced), ADR 0022/0043/0045 (the density + teleport substrate this rides)

## Context

Live teleport (ADR 0045 C2 post-copy) moves a session between hosts with a
~330 ms guest-observed blackout, and `drain_host` is already live-first. That
makes fleet self-compaction affordable for the first time — the payoff ADR
0045 named ("the fleet continuously self-compacts to its cost-optimal node
count") but parked behind log-only knobs:

- **Scale-up** (K4): `desired_hosts()` + `GkeNodePoolScaler::set_size` exist;
  prod runs `operator.scaler=noop`, no IAM, no autoscaling CR block.
- **Scale-down** (E1): policy + hysteresis shipped; a CONFIRMED decision logs
  *"would drain the least-loaded host"* and returns. The blocker is
  structural: `set_size` is count-only — the cloud picks which node dies.

The target scenario: a 1000-session burst arrives on a small fleet (must not
be rejected while nodes provision — sessions **queue**); later, 100 nodes
each hold one lingering session (the fleet **packs** them onto a handful via
teleport and deletes the empty nodes).

Three placement-side gaps block that scenario:

1. **No queue.** `reserve_placement` finding no room is a 503. During the
   2–3 min a fresh node takes to provision, every create bounces.
2. **Placement spreads.** The capacity-fit tier picks the LARGEST free RAM —
   deliberately anti-packing. Spread placement is what manufactures the
   "100 nodes × 1 session" shape that scale-down must then undo.
3. **RAM-only budgets.** Sessions reserve `mem_budget_mib` only. Packing
   concentrates vCPU contention with no declared bound.

## Decision

### 1. CPU budgets: the image declares its vCPUs; placement reserves them

- The manifest field is `resources.vcpus` (renamed from `suggested_vcpus` —
  it is a declaration, not a hint) and **enable-time validation rejects an
  image that omits it**. Existing images re-bake/re-enable on the standard
  roll (clean break; 0 users).
- `sessions.cpu_budget_vcpus` (migration 0063) records the declared budget at
  reserve time, beside `mem_budget_mib`.
- The host's CPU budget is `total_vcpus × ENGRAM_CPU_OVERCOMMIT` (default
  **4.0**). Strict reservation would defeat packing outright — an 8-vCPU node
  at 2 vCPU/session caps at 4 sessions while RAM fits ~3× that — and FC
  guests idle heavily; the overcommit factor is the calibrated lever, watched
  against `util_cpu_pct` on packed hosts. RAM stays the hard constraint
  (exact, FC-capped); CPU is a declared soft budget.
- Heartbeats already report `total_vcpus` (ADR 0047 groundwork).

### 2. Placement packs: best-fit, 2D

`choose_placement_host` (the reserve transaction's pick) and the resume-path
picker flip from "largest free that fits" to **smallest free RAM that fits
both budgets** (`free_mib ≥ mem_budget AND free_vcpus ≥ cpu_budget`).
Snapshot-affinity stays the top tier; `prefer_host` second; the
unknown-allocatable fallback unchanged. New sessions concentrate onto
already-loaded hosts, so scale-down pressure surfaces without waves having to
manufacture it. A single clean flip — no spread/pack mode knob.

### 3. Queued sessions: no-capacity never rejects

New `SessionState::Queued` (migration 0064): a create that finds no room
INSERTs the row at `queued` (host_id NULL, `queued_at`, `queue_origin =
'create'`, the prompt stashed in `queue_prompt`) and returns **201
`{status:"queued"}`**; a resume that finds no room parks `Idle → Queued`
(`origin='resume'`) and returns 202. The handler never blocks — a
`queue_scanner` (evac_resumer shape, `session_lease`-guarded, replica-safe by
construction) owns the continuation:

- FIFO, **place-until-first-failure** per tick: strict fairness, one
  serialized placement attempt at the head, no thundering herd. Head-of-line
  blocking is deliberate — the operator scales the fleet to fit the head,
  because `queued_mib`/`queued_vcpus` include it.
- Placed ⇒ `Queued → Pending` inside the same reservation transaction shape
  (`place_queued_session`), then the boot continuation runs on a bounded
  JoinSet; boot failure re-queues (the timeout clock keeps the original
  `queued_at`).
- `queued_at + ENGRAM_QUEUE_TIMEOUT_SECS` (default **1800 = 30 min**)
  exceeded ⇒ create-origin → `Failed` + a user-visible `queue_timeout`
  event; resume-origin → back to `Idle` (durable; never Failed). The
  timeout is deliberately generous: it is a backstop for the
  *permanently-stuck* case (maxHosts hit, cloud quota/stockout), NOT a
  budget for normal scale-up. A cold scale-up is node provision (~2–3 min)
  + image prefetch (minutes for a cold image) + base-snapshot warm + the
  occasional wait for a *peer* session to free room when at maxHosts —
  comfortably under 30 min, with headroom. Operators who would rather wait
  even longer than ever drop a request raise the knob; we prefer a long
  queue to a 503.
- FSM edges: `Pending→Queued`, `Idle→Queued`, `Queued→Pending` (placed),
  `Queued→Idle` (resume dequeue + resume timeout), `Queued→Failed`
  (create timeout / cancel). `reserves_host_memory(Queued) = false`.
- The crash-orphan filter on `pending` reservations moves from `created_at`
  to `last_active_at` — a queued→pending flip on an old row must not be
  instantly mistaken for an orphan (overcommit bug otherwise).

`GET /api/admin/fleet/demand` grows `queued_sessions`, `queued_mib`,
`queued_vcpus`, `free_vcpus`, `total_vcpus`, `cordoned_hosts` — the queue IS
the scale-up demand signal, replacing inference from 503 counters.

### 4. Scale-up: one shot, queue-aware

`desired_hosts` covers `max(RAM deficit, CPU deficit)` — each deficit
including its queued component plus the headroom target — in ONE `div_ceil`
jump clamped to `maxHosts` (a 1000-session burst is one `setSize` call, not
one-host-per-tick). All `FleetDemand` additions are serde-default-0
(deploy-order tolerant). The scale-down arm is hard-gated on an empty queue
and returns the full cost-optimal target; the actuator bounds the wave.

**Invariant: `set_size` is grow-only; `remove_node` is the only shrink
path.** This also fixes a live footgun found during design: a failed roll
leaves a host cordoned, the next reconcile reads `schedulable = N−1`, and the
hold arm would `set_size(N−1)` on a physically-N pool — with a live GKE
scaler the MIG would delete an ARBITRARY node, possibly one carrying live
microVMs.

### 5. Scale-down: consolidation waves on the teleport seam

`NodePoolScaler` gains `remove_node(pool, node)` — remove ONE named node,
atomically decrementing the pool target (GKE: Container API
`instanceGroupUrls` → the owning zonal MIG → Compute
`instanceGroupManagers.deleteInstances` with
`skipInstancesOnValidationError` for idempotency).

The operator's wave driver (replacing E1's CONFIRMED-log branch) is
**stateless per reconcile**; in-flight intent rides a Node annotation
(`fleet.engram.io/scaledown-victim`) set at cordon time, so an operator
restart resumes (or a demand spike aborts) a half-done wave by observation:

1. Queue nonempty or scale-up pressure ⇒ **abort**: uncordon + de-annotate
   every not-yet-removed victim (cordoned victims are an instant-return
   capacity reserve), grow if needed.
2. Scale-down confirmed (hysteresis only when ENTERING; an in-flight wave
   continues) ⇒ `plan_wave` picks victims — IdleOnly: only
   `running_sandboxes == 0`; Aggressive: least-loaded first — clamped by
   `maxShedPerWave`, the floor (`max(minHosts, capacityFloor)`), and the 2D
   guard (Σ victim reserved ≤ Σ survivor free − headroom, dropping the
   most-loaded victim until it fits). Cordon + annotate, then per victim
   (≤ `maxConcurrentDrains`): coord drain → gate on `running_sandboxes → 0`
   → `remove_node` → `DELETE /api/admin/hosts/:id`. A drain timeout
   uncordons THAT victim and continues the rest.
3. Hold ⇒ finish annotated victims; else reset hysteresis.

Waves start only when the image-roll planner is `UpToDate`; an in-flight wave
blocks new rolls. Cordoning all victims first means the survivors are the
only legal targets — packing falls out of the cordon, not out of bespoke
placement logic.

**Drain hardening (coordinator side):**
- *Don't-strand guard*: `drain_host` pre-checks per session that SOME
  survivor fits its budgets; no fit ⇒ a `DrainFailure` in the response and
  the move never starts (an Active session must never get parked Idle
  because the fleet was full — the operator aborts the wave instead).
- Budget-aware live-move target picks (real `memory_mib`/cpu, not None).
- `DELETE /api/admin/hosts/:id` deregisters a drained host immediately
  (409 if sessions still bound; idempotent) instead of waiting for the
  dead-host detector.
- `GET /api/hosts` carries `allocatable_mib`/`reserved_mib`/`free_mib`/
  `total_vcpus`/`reserved_vcpus`/`cordoned` — the operator's victim-picking
  and fit-guard inputs.

### 6. Rollout (engrams-internal)

Dedicated operator GSA + custom role (exactly: `container.clusters.get`,
`container.nodePools.{get,update}`, `compute.instanceGroupManagers.{get,update}`)
via Workload Identity — the coordinator keeps zero cloud credentials. Staged:
coord `replicas: 2` (ADR 0047 validation) → `scaler: gke` scale-up only →
one manual supervised wave (verifies the IGM mapping and that
`deleteInstances` decrements `targetSize` without spawning a replacement) →
`scaleDown: idleOnly` soak → `aggressive` + the headline 100→handful
rehearsal.

## Invariants / tradeoffs

- **CPU overcommit 4.0 is a guess until calibrated**; `util_cpu_pct` on
  packed hosts is the signal, and the knob is the response. (4.0 matches
  E2B's production default `R=4` — external validation that the ratio is
  sane.) RAM remains the exact, hard constraint, reserved at the session's
  *configured ceiling* (`mem_budget_mib`) — deliberately conservative; see
  "Future work: memory density" for why and what to revisit.
- **FIFO head-of-line blocking is deliberate** — fairness + a simple
  invariant (the operator scales to fit the head). A 32 GiB head blocks
  smaller queued sessions until capacity fits it.
- **Required `resources.vcpus` opens the standard clean-break window** until
  images are re-baked + re-enabled.
- **Resume placement still bypasses PG reservation** (pre-existing ADR 0046
  gap). The wave's preview narrows it; routing resume through a reserve
  transaction is a named follow-up.
- **The operator stays single-replica** (chart `Recreate`); it is the wave's
  single writer. The COORDINATOR is multi-replica (ADR 0047) — the operator
  doesn't need to be.
- **GKE IGM assumptions** (node-name↔IGM prefix, deleteInstances semantics)
  are UNVALIDATED until the manual wave; the grow-only invariant caps the
  blast radius at "MIG recreates a node", never "deletes a loaded node".
- **Scale up fast, scale down slow** (ADR 0045's flapping risk): hysteresis
  on entry, one wave at a time, abort-on-queue.

## Future work: memory density (measure on prod, then revisit)

We reserve each session's **configured** RAM ceiling, not its measured
resident footprint. That is provably OOM-safe but leaves density on the
table: FC guest memory is demand-paged, so a 4 GiB-configured session that
touches ~1.5 GiB carries ~2.5 GiB of phantom reservation, and ten of them on
a host hide ~25 GiB the packer can't use. E2B captures exactly this gap —
their placement gates on CPU alone (4× overcommit) and reserves **nothing**
for memory, betting that demand-paged residency stays under host RAM. We
don't take that bet today because memory's failure mode is a hard OOM cliff
(ADR 0046), our base residency is `mlock`'d (unswappable by design), and dev
builds touch and *keep* their working set with no active reclaim. (Disk swap
is rejected outright — it trades the ~330 ms teleport latency we hold
non-negotiable for unbounded fault latency and host-wide thrash; the queue is
the correct back-pressure valve, not swap.)

The lever to close the gap, gated on **prod measurement** of
`Σ resident-RSS / Σ configured-budget` across real agent workloads:

1. **A memory-overcommit ratio** symmetric to CPU's (`allocatable ×
   mem_overcommit − Σ mem_budget`), default 1.0, raised toward ~1.2–1.5 once
   the measured gap justifies it. Far more conservative than CPU's 4.0
   because the failure is OOM, not slowdown — but it converts today's
   hardcoded conservatism into a measured, reversible knob.
2. **virtio-balloon + reserve-on-measured-working-set** — the true E2B-style
   density: let guests return idle pages to the host and reserve against an
   estimated working set rather than the configured ceiling. This is the
   bigger structural bet: it interacts with snapshot/restore (FC balloon
   caveats across resume) and with our `mlock`'d shared base, so it is its
   own ADR, not a knob. Note we already have a density axis E2B lacks — the
   shared, PSS-counted base — so the end state is *both*: shared clean base
   (have it) **and** bounded overcommit on the per-session divergent set.

Order of operations: ship strict-1.0 reservation now, instrument the
configured-vs-resident ratio in prod, then introduce the overcommit knob, and
only consider ballooning if the knob proves insufficient.

## Validation

- Pure-policy unit tests (one-shot burst math, CPU-binds-before-RAM,
  any-queue-blocks-scale-down, wave planning/guards) + mock-driven wave
  executor tests (abort/resume/timeout arms).
- Live-PG: queue scanner FIFO + timeout arms; placement packs
  (`burst_packs_one_host_then_overflows_to_next`).
- FC two-host CI: `two_host_drain_wave` (drain → live-teleport → gate →
  delete host; the no-capacity arm leaves no session stranded).
- Prod: the staged rollout above, closing with the 100-lingering-sessions
  consolidation rehearsal (blackout p99 ≈ 330 ms budget; a synthetic burst
  mid-wave aborts and returns cordoned capacity instantly).
