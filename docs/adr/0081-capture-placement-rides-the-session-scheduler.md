# 0081 — Capture placement rides the session scheduler

Status: Accepted (implemented in one pass with this ADR's PR)

## Context

A base-snapshot capture boots a real VM sized from the image's
`resources` block (dev-brain: 24 GiB / 8 vCPUs, warm-booting a full
`tilt up` stack) — the same footprint as a session of that image. But
capture placement never joined the ADR 0046/0048 reservation system:

- `pick_capture_host` checked schedulability + base capabilities + the
  ADR 0078 disk floor and took the **first** match — no RAM/CPU fit.
- The capture VM held **no `ReservedBudget` entry**, so session
  placement couldn't see it, and concurrent captures couldn't see each
  other.
- A capture that found no host surfaced as `ApiError::Unavailable` and
  **burned the enable job's attempts budget** (5), failing the job
  outright on a busy fleet.

Prod incident (2026-07-08, session `8a80c3fb`): the enable scanner
started a dev-brain re-capture on host `n05d` thirty seconds before the
queue scanner placed a dev-brain *session* there. Both "fit" —
2 × 24 GiB VMs + ~7 GiB base-shm on a 64 GiB node — until the kernel
`SystemOOM` shot a firecracker process, the node went NotReady, the
session took a `host_lost` → rung-1 checkpoint recovery (57 events
rolled back), and the enable job died on its 5th attempt.

The operational consequence has been an informal "don't enable/refresh
while sessions are running" rule and effectively one capture at a time.
Meanwhile the fleet already has exactly the machinery captures need:
2D best-fit reservation at pick (ADR 0046/0048), a queue whose demand
the K4 autoscaler scales toward (ADR 0044/0047/0048), and a state
machine (`enable_jobs`) that survives coordinator crashes.

## Decision

Captures become first-class tenants of the session scheduler:

1. **Budgets on the job row.** `enable_jobs` gains `mem_budget_mib` /
   `cpu_budget_vcpus`, stamped at job creation from the same
   `resolved_memory_mib` / `resolved_vcpus` derivation sessions use
   (FC requires capture and restore to agree on `mem_size_mib` anyway).

2. **Atomic reserve-at-pick.** A new `MetadataStore::
   reserve_capture_host(job_id, candidates, mem, vcpus)` runs the SAME
   `FOR UPDATE` transaction shape as `reserve_and_persist_create`
   (identical lock order, identical fit map, `choose_placement_host`)
   and stamps the winner into a new `enable_jobs.capture_host_id`
   column — or returns `None` when nothing fits. The reserved-SUM every
   placer reads (`reserve_and_persist_create`, `place_queued_session`,
   `per_host_reserved`, `fleet_free_mib`) becomes
   `sessions ∪ capturing enable_jobs`, so sessions and captures are
   mutually visible.

3. **No capacity ⇒ queue, don't fail.** A `None` pick leaves the job in
   `capturing` with `capture_host_id NULL` and stamps
   `capture_waiting_since` (first miss only). The scanner treats this
   as *waiting*, not a pipeline error: no attempts bump, retry next
   tick. `queued_demand()` (the K4 autoscaler's scale-up signal, and
   the scale-down hard gate) counts these waiting captures alongside
   queued sessions, so a full fleet **grows** for a capture exactly as
   it does for a session. A capture that waits longer than the queue
   timeout (`ENGRAM_QUEUE_TIMEOUT_SECS`, default 30 min — the same
   backstop sessions get for maxHosts/quota/stockout) fails the job
   with a legible error. The wait deadline is measured from the
   **DB-persisted `capture_waiting_since`** (the first miss), not a
   wall-clock instant captured at the top of the wait loop:
   `reserve_capture_host` returns the COALESCE-stamped anchor in its
   no-fit arm and the scanner compares `now − anchor` against the
   timeout the same way the queue scanner times a session out. A
   process-local deadline would reset on every pod restart / lease
   re-claim, so under coordinator churn the 30-min backstop would
   never fire.

4. **Release on capture exit.** `capture_host_id` (and
   `capture_waiting_since`) clear when the capture step returns —
   success or failure — and on any terminal job state. A crashed pod's
   stale `capture_host_id` is cleared the instant a peer re-claims the
   expired lease: `claim_enable_jobs` nulls it in the claim's `SET`
   clause, so the re-run's `reserve_capture_host` re-reserves from
   scratch instead of counting the job's OWN phantom reservation
   against the only viable host (a self-block, on a tight fleet, that
   would otherwise never fit its own budget → `CapacityTimeout`; and
   `queued_demand` excludes a host-reserved job, so the autoscaler
   wouldn't rescue it either). `capture_waiting_since` deliberately
   **survives** the reclaim — it is the first-miss wait anchor the
   timeout measures from (§3), cleared only when the capture actually
   fits, is explicitly released, fails, or is retried.

5. **The n≤1 posture is lifted.** With reservations, concurrent
   captures (of different images — the per-image in-flight dedupe in
   `create_or_get_enable_job` stays) and capture-next-to-session are
   safe by construction: enable/refresh whenever.

Materialize keeps its disk-floor-only pick: it runs no VM (docker pull
+ ext4 pack + chunking), so RAM reservation would be pure overcommit
insurance at the cost of serializing materializes behind capture-sized
budgets. `pick_capture_host` is renamed `pick_materialize_host` and
stays disk-gated.

## Consequences

- The fleet-demand wire shape is unchanged: waiting captures fold into
  the existing `queued_sessions` / `queued_mib` / `queued_vcpus`
  fields, so the host-operator needs no change — scale-up sizing and
  the scale-down gate both behave correctly for capture demand.
- A capture on an autoscaler-shed victim host still dies (it has no
  session to evacuate) and retries via the attempts budget; its
  reservation now at least makes the host's load visible to the
  2D shed guard.
- An `allocatable_mib`-unmeasured host (brand-new / dev) keeps the
  existing capacity-soft posture for captures, mirroring sessions.
- Migration `0095` (add columns). No wire-version bump: coordinator-
  internal only.

## Divergences / pitfalls (updated during implementation)

- **In-process wait, not release-and-requeue.** The plan sketched
  "leave the job in `capturing` and retry next tick"; implementation
  showed that path re-runs the pipeline from the top on every re-claim
  — and materialize *re-pulls the docker image* (minutes for
  dev-brain-class images) each time. The capture step instead holds its
  claim and loops `reserve → sleep 5s` in-process (each reserve write
  renews the lease, so a long wait doesn't get stolen), and `run_once`
  drives claimed jobs concurrently (a `JoinSet`) so a waiting capture
  parks its own task without starving the *other* jobs in the same
  claim batch (the whole tick still joins before it returns — see the
  `run_once`-join note below). The concurrent drive is also the actual
  lift of the old one-enable-at-a-time-per-pod serialization — the
  serial loop, not any explicit gate, was the n≤1.
- **`CapacityTimeout` is a `CaptureFailureKind`.** Instead of a new
  `ApiError` variant or string matching, the wait-deadline failure
  rides the existing issue-#539 structured taxonomy
  (coordinator-emitted; a host never sends it), so
  `classify_capture_error` gets bail-fast semantics for free and the
  kind lands machine-readable on the job row.
- **The shared pick got factored, retiring two copies.**
  `reserve_and_persist_create` and `place_queued_session` carried the
  same FOR-UPDATE fit block verbatim; both now call `pick_host_2d`
  (which is also where the capture UNION lives, so no reader can drift).
- **`delete_host` also guards captures.** The "sessions still bound"
  refusal now counts capturing jobs — the autoscaler's shed path can't
  deregister a host out from under a capture VM silently.
- **Job wire shape deliberately unextended.** The budgets /
  `capture_host_id` / `capture_waiting_since` are scheduler-internal;
  the proto `EnableJob` doesn't surface them (a waiting job is legible
  via the coord's waiting log and the `capacity_timeout` failure kind).
  Revisit if the dashboard wants a "waiting for capacity" badge.
- **Pre-0095 fallback budgets are PERSISTED, not just used for the
  pick.** In-flight jobs migrated onto 0-default budget columns would
  reserve *nothing* — a budget-0 `capture_host_id` folds 0 into every
  other placer's reserved-SUM (the exact 2026-07-08 OOM), and a
  budget-0 waiting job folds 0 into `queued_demand` so the autoscaler
  never grows for it. The capture step resolves the config-derived
  budget for a 0 row, and `reserve_capture_host` STAMPS it onto
  `mem_budget_mib` / `cpu_budget_vcpus` in **both** its branches (fit
  AND no-fit, fenced by claimant) — so the moment the capture step
  touches a mid-rollout job the row carries a real budget that every
  reserved-SUM reader and `queued_demand` see, not just the in-process
  pick. The derivation stays in one place (the caller resolves; the SQL
  persists).
- **`run_once` joins its whole `JoinSet` before returning**, so a
  capacity-waiting capture parks one of the pod's `claim_limit` (default
  2) claim slots and delays *that pod's* next sweep until the capture
  places or times out. Bounded by `claim_limit` and mitigated by
  multiple replicas + the claim's `FOR UPDATE SKIP LOCKED` (peers keep
  sweeping the rest of the fleet's jobs); acceptable for now. The proper
  fix — decoupling the wait from a claim slot — rides the capture-jobs
  redesign (issue #546).
- **A lease-loss re-claim can briefly double-run a capture.** If a pod
  loses its lease while its capture RPC is still executing host-side, a
  peer re-claims and can start a second capture before the first
  notices. The reservation bookkeeping here is correct on both sides
  (each re-reserves from scratch after the claim-time clear), but the
  duplicate host-side execution is a pre-existing enable-pipeline
  hazard that the capture-jobs redesign (issue #546) owns; ADR 0081
  does not close it.

Validated by `placement_reservation_live_pg` (5 new tests: mutual
visibility both directions, wait-clock + queued-demand semantics,
fencing + failure-path release, pre-0095 fallback-budget persistence +
session-visibility, and reclaim-clears-stale-host-but-keeps-wait-anchor)
— in CI's live-PG lane.
