# 0104. DBOS orphan-sweep: adopt workflows stranded by version-gated recovery

Status: Accepted

Issue: cortexapps/engrams#872

## Context

Every orchestrator deploy that changes DBOS workflow code (or bumps the
`@dbos-inc/dbos-sdk` version) silently strands the in-flight workflows of the
previous build. DBOS computes **one application version for the whole
process** — an MD5 over the source of *every* registered workflow function
plus the SDK version (`dbos-executor.js:887`) — and both startup recovery and
queue dispatch are version-gated: a `PENDING` workflow stamped with a version
no live pod runs is unreachable by every built-in path.

Confirmed in prod (session `af28cac4-047c-4097-8790-48f6ac0df463`,
2026-07-21/22 UTC): a 23:44:08 helm deploy changed `PrReviewWorkflow` (not the
Slack workflow), bumping the shared version `29fc9960… → d8de0dd9…`; the old
pod was SIGTERM'd mid-`recv`; the new pods logged
`No workflows to recover from application version …` and skipped it forever.
253 notifications piled up unconsumed in `dbos.notifications` while the agent
kept working — the Slack thread just went quiet. A prod audit found 15
orphaned `PENDING` `SlackThreadWorkflow`s across 4 dead app versions dating to
2026-06-26 (worst: 403 queued messages). Manual resume was run twice; the very
next deploy re-created the population.

The version gate exists for a reason: replay correlates a workflow body's
durable calls to recorded `operation_outputs` rows **positionally**. Replaying
a changed body can misalign the sequence — a loud `DBOSUnexpectedStepError` at
best, silently handing the wrong cached output to the wrong call at worst.
So the fix must un-gate only provably-abandoned workflows, not remove the
gate.

### Alternatives rejected

- **Pin `applicationVersion` fleet-wide** (ganglia's approach: a constant pin
  + boot reconciler + CI lint). Right for ganglia — all their workflows are
  minutes-long idempotent run-to-completion jobs with zero `DBOS.recv`. Wrong
  for us: our workflows are hours-long conversations, and the pin removes the
  version gate for everyone all the time, which then demands
  `enablePatching` + `DBOS.patch()` bookkeeping on every body edit.
- **Boot-time adopt** — same surgery run exactly once at the most fragile
  moment, with replicas racing; a periodic reconciler is strictly more robust
  and matches the repo's scanner-drives-transitions pattern.
- **Drain-before-deploy** — couples every deploy to a ≤1h recv horizon;
  unworkable at ~10 deploys/day.
- **Embedded conductor ("maestro")** — explored in depth and deferred; the
  conductor protocol's `recovery` command is keyed by dead executor id,
  version-gated to the receiver (it cannot see version-NULL rows), and dumps a
  whole dead pod's workload onto one engine.

## Decision

A **sweep reconciler inside the orchestrator** — no new service, no conductor
layer. The lever is the queue runner's claim query
(`system_database.js`, `findAndMarkStartableWorkflows`):

```sql
WHERE status = 'ENQUEUED' AND queue_name = $2
  AND (application_version IS NULL OR application_version = $3)
...
FOR UPDATE SKIP LOCKED
```

A `NULL`-version `ENQUEUED` row is claimable by **any** live pod, atomically,
exactly once, load-spread by pull. The sweep never assigns work — it flips
abandoned rows back into the queue and pods pull.

### Components

1. **Version heartbeat table** (new drizzle migration): each pod upserts
   `(application_version, pod_name, last_seen)` on an interval. `last_seen`
   uses PG server time (`now()`), never pod clocks. A version is *dead* when
   no row is fresher than the grace window.
2. **Sweep loop**: plain in-process interval timer with the house
   `runOnce()`/`start()`/`stop()` split (mirrors `AutomationScheduler`).
   Deliberately **not** a DBOS scheduled workflow (it would strand itself on
   the next deploy) and not a k8s CronJob (a new deployable for no gain). A
   PG lease row picks one sweeping pod per cycle — mainly to avoid duplicate
   alerts; the flip itself is idempotent under concurrency (status guards +
   row locking).
3. **The flip** (batch-capped per cycle):

   ```sql
   UPDATE dbos.workflow_status
   SET status = 'ENQUEUED', queue_name = '_dbos_internal_queue',
       application_version = NULL,
       workflow_deadline_epoch_ms = NULL, deduplication_id = NULL,
       started_at_epoch_ms = NULL, completed_at = NULL,
       updated_at = (EXTRACT(EPOCH FROM now())*1000)::bigint
   WHERE status = 'PENDING' AND workflow_uuid = ANY($1)
   ```

   Stuck `ENQUEUED` rows on dead versions get only the
   `application_version = NULL` part.
4. **Sweep ledger** (second table): per-workflow sweep count + timestamps +
   per-instance `suppressed` flag. Caps re-sweeps (default 3), dedupes
   alerts, records which cleanup callback ran, gives operators history.
5. **Sweep policy registry** — keyed by registered workflow name, lives in
   code, **exhaustive by construction**: at boot every registered workflow
   must have a policy entry (`adopt` / `cancel` / `ignore`) or the
   orchestrator fails to start (same discipline as the wire-proto
   exhaustiveness guards).

   ```ts
   const SWEEP_POLICIES: Record<string, SweepPolicy> = {
     SlackThreadWorkflow: { mode: "adopt", staleAfterHours: 48, onTerminalFailure: notifyThread },
     PrReviewWorkflow:    { mode: "adopt", staleAfterHours: 48, onTerminalFailure: failReview },
     ToolExecWorkflow:    { mode: "adopt", staleAfterHours: 1 },
   };
   // onTerminalFailure?: (ctx: SweepContext, wf: FailedWorkflow) => Promise<void>
   ```

   `onTerminalFailure` is an optional callback: deps arrive via the injected
   `SweepContext`; **at-least-once** — the ledger marks cleanup done only
   after the callback returns, so callbacks must be idempotent; failures are
   contained (logged, retried next cycle up to the cap) and can never crash
   the sweep. The generic ops alert fires *outside* the callback,
   unconditionally. DB names with no registration: renamed/deleted workflow
   types → alert-only, never adopt; `temp_workflow-send-*` → adopt (a single
   idempotent send); other unknowns → alert-only.
6. **Failure alerting**: the same loop scans for *newly* terminal-failed
   workflows (`ERROR`, `MAX_RECOVERY_ATTEMPTS_EXCEEDED`) via a watermark and
   posts to a configured Slack ops channel. Alert failures log and never
   crash the sweep.
7. **Thread black-hole guard**: when a `SlackThreadWorkflow` fails terminally
   (typically a changed-body replay after adoption), post one note to the
   affected thread itself ("this conversation hit a snag — start a fresh
   thread") and cancel the workflow. Without this, new messages in that
   thread route to a dead workflow id and vanish — the original bug through
   the side door.
8. **CI hash warning (non-blocking)**: a script that imports the workflow
   modules, hashes each registered workflow's `origFunction.toString()` plus
   the SDK version, and diffs a committed snapshot. On change it emits
   GitHub Actions `::warning::` annotations and always exits 0; `--update`
   refreshes the snapshot.

### Config knobs (defaults)

| knob | default |
|---|---|
| heartbeat interval | 30s |
| sweep interval | 60s |
| grace window (version considered dead) | 10 min |
| staleness cutoff (cancel instead of adopt) | 48h |
| batch cap per cycle | 5 workflows |
| max sweeps per workflow | 3 |

### Safety properties

- **Rolling deploys**: old pods heartbeat while alive; grace window must
  exceed heartbeat interval + pod termination grace. Current-version PENDING
  work is recovered by DBOS's built-in self-recovery — the sweep only ever
  touches dead versions.
- **Wedged pod** (alive, not heartbeating): the sweep may steal its
  workflows → bounded double-execution: `(workflow_uuid, function_id)`
  uniqueness in `operation_outputs` kills the loser with
  `DBOSWorkflowConflictError` at its next step write. Ceiling: one duplicated
  step side effect. Mitigating symmetry: a pod that can't reach PG to
  heartbeat also can't record steps.
- **Changed body after adoption**: replay fails loud → terminal `ERROR` →
  alert + thread note + cancel. Terminal states are never re-swept (natural
  poison-pill backstop).
- **Ancient orphans**: past the staleness cutoff, cancel instead of adopt —
  never necro-post a weeks-dead conversation.

## Operations (replaces the manual resume recipe)

- **Stranded workflows recover themselves**: within one sweep interval of a
  version going dead (no heartbeat for the grace window), fresh orphans are
  re-enqueued and claimed by live pods. The manual
  `UPDATE … SET status='ENQUEUED', application_version=NULL` recipe is
  retired.
- **Ops signal**: one log line per cycle (`component: dbos-sweep`) with live
  versions and per-action counts; alerts (terminal failures, alert-only
  orphans, cancels) go to `ORCHESTRATOR_SWEEP_ALERT_CHANNEL`, or the log when
  unset.
- **Exclude one workflow**: `update dbos_sweep_ledger set suppressed = true
  where workflow_uuid = '…'` (insert the row first if it was never swept).
- **Kill switch**: `ORCHESTRATOR_SWEEP_DISABLED=1` stops the sweeper only;
  heartbeats continue so the pod's own version stays provably live.
- **A workflow cancelled or failed by the sweep**: Slack threads get the
  in-thread "start a fresh thread" note; reviews flip to failed and
  `RetryReview` mints a successor epoch.

## Consequences

- Stranded-forever becomes at-least-once with a tiny duplication window.
- The flip SQL and the claim-query semantics are DBOS internals, not API.
  The integration suite (child-process pods with `DBOS__APPVERSION`
  overrides; the SDK-upgrade canary strand → flip → real claim) is the
  contract test that makes a future SDK bump fail in CI instead of prod.
- One-time prod cleanup of the existing orphan population remains a separate
  operator action (documented runbook replaces the manual recipe).

## Implementation record

Landed as a commit chain on `fix/872-dbos-orphan-sweep`: `96d46732` (this
ADR) → `eb817c31` (T1 stores/migration) → `0b65bc5b` (T2 sweep core) →
`f165b61b` (T3 alerting/cleanups) → `f1a0a39d` (T5 CI hash warning) →
`a7350879` (T4 wiring/config) → `a98a028d` (T6 integration suite) →
`02524bc6` (adversarial-review hardening). Divergences from the design
above, found in review:

- **Adversarial-review hardening** (all six findings fixed in `02524bc6`):
  keyset-paginated scan with a logged budget (starvation); the flip SQL
  fences on expected version + fresh-heartbeat NOT EXISTS (TOCTOU);
  flip+ledger-count are one transaction and cancels record intent first
  (burned-cap cancellations); the failure-scan watermark rewinds to
  first-blocked−1 / last−1 on full batches (timestamp ties); abandonment
  alerts post before the durable gave-up mark; the CI hash script also
  diffs against the merge-base snapshot so committing `--update` cannot
  silence the PR annotation. Residual accepted risk: the window between
  `DBOS.launch()` recovery and the first heartbeat on a rolled-back pod is
  a few ms and remains bounded by the `(workflow_uuid, function_id)`
  step-conflict guard; cleanup attempt counts stay in process memory
  (restarts retry an idempotent-required callback, alarms are never lost).

- **Drizzle array params**: the `sql` template JSON-stringifies a raw JS-array
  param (PG 22P02 under `::text[]`); array params are expanded element-wise
  via a `textArray()` helper in `db/dbos-sweep.ts`.
- **`temp_workflow-` prefix broadened** (was `temp_workflow-send-` only): every
  DBOS temp wrapper is a single idempotent operation, so all adopt. The
  registry also covers `AutomationRunWorkflow` (adopt, 1h) — the design's
  example listed three workflows; four are registered.
- **Ledger alert/cleanup marks are UPSERTs**: an `alert-only` or
  naturally-failed workflow has no prior sweep row; plain UPDATEs meant alert
  dedup never stuck and the ops channel re-alerted every cycle.
- **Watermark hold, not break**: a wedged cleanup holds the failure-scan
  watermark at its own row but later rows are still processed, so one failing
  cleanup cannot block alerting for everything behind it.
- **Alerting is log-only without a channel, never off**: cleanups (the thread
  black-hole note, failReview) run regardless of whether
  `ORCHESTRATOR_SWEEP_ALERT_CHANNEL` is configured; only the alert transport
  degrades to the orchestrator log.
- **Heartbeat is unconditional**: `ORCHESTRATOR_SWEEP_DISABLED` disables only
  the sweeper. A live pod must still heartbeat its version or another pod's
  sweeper would adopt its in-flight workflows.
- **SDK seams**: app version reads public `DBOS.applicationVersion`; workflow
  registry enumeration needs `getAllRegisteredFunctions` via a relative dist
  import (the SDK exports map hides it) — the SDK-upgrade canary test guards
  both, plus the claim-path contract.
- **The flip also nulls `completed_at`** (present in 4.21.6's schema despite
  its absence from older docs), for parity with SDK `resumeWorkflows`.
