# ADR 0119: Automations as a block-based durable workflow engine

Status: 2026-08-21 — **Proposed.**

Builds on ADR 0051 (the TypeScript orchestration tier), ADR 0060 (external
triggers and durable DBOS workflows), ADR 0100 (PR code review), ADR 0102
(automations v1), ADR 0103 (durable exec), ADR 0104 (the DBOS orphan sweep),
ADR 0106 (first-class OAuth credentials), and ADR 0115 (user-scoped
integration credentials). It supersedes the single-action automation model of
ADR 0102 and, at the end of its final phase, the hardcoded workflow graphs of
ADR 0100 (PR review) and ADR 0060 (the Slack thread brain).

## Context

engrams runs unattended work through three products that share no shape:

- **Automations** (ADR 0102): `trigger → create_task`. The action union has
  exactly one arm. The run ledger marks a run `launched` and the system never
  looks at the session again.
- **PR review** (ADR 0100): a hand-written DBOS graph of ~1,300 lines across
  `orchestrator/src/workflows/{pr-review,review-ingress,review-control-plane}.ts`.
  It verifies a webhook, classifies it, creates a finder session, clones the
  repository, stages files, sends a prompt, parks on a mailbox, runs a
  verifier session, applies a policy gate, and posts to GitHub.
- **Slack thread brain** (ADR 0060): a per-thread DBOS workflow
  (`slack-thread.ts`) that relays a conversation between a Slack thread and a
  session.

Every step of the review graph is a step any customer pipeline wants: create
a session from a profile, run a command in it, write files into it, send a
prompt and wait, branch on a result, call an integration. ADR 0102 named a
generic `workflow` action as its end state and did not build it. This ADR
builds it.

The product decisions are recorded in the design document (artifact
`fa71f664`, 2026-08-21) and were settled with the user:

1. The built-in automations are **real**: the engine runs them, and the old
   graphs retire at parity behind flags.
2. User logic (filters, transformations) is **JavaScript in a QuickJS-on-WASM
   sandbox** (the Code block). Liquid stays for prose templates only.
3. The product keeps its page and its gate: **Settings → Automations,
   admin-only, same name**. One data migration rewrites existing rows.
4. The **Slack thread brain is the second built-in**. Expressing it forces
   three model additions: `join` concurrency, a Wait-for-event block, and a
   Loop block.
5. Sessions a run creates are **kept by default**. An End-session block or
   the `end_sessions_on_finish` setting opts into teardown.
6. Nothing auto-enrolls a repository. Per-repository review policy is an
   automation **input** (`repos: map<repository, {mode, autofix}>`) rendered
   as a form.
7. The name stays **Automations**.

## Decision

### D1 — The model: trigger + blocks + inputs

An **automation** is a named, versioned definition:

- one **trigger**: an integration event (provider + connection + event keys +
  optional scope), a cron schedule, a custom generic webhook, or manual;
- an ordered graph of **blocks**. Control blocks (`filter`, `branch`, `loop`)
  shape the graph; action blocks do work (`create_session`, `send_prompt`,
  `wait_session`, `wait_event`, `end_session`, `run_command`, `write_files`,
  `code`, `integration_action`, and code-registered `system.*` blocks that
  only built-in automations may reference);
- an **inputs schema**: typed parameters (string, number, boolean, enum,
  secret ref, list, map keyed by an integration noun, object rows) whose
  values render as a form. On a built-in, the input values are the only
  editable surface;
- **settings**: concurrency (`{keyTemplate, policy: queue | supersede | skip
  | join}`), a run deadline, and `end_sessions_on_finish` (default false).

Blocks read and write one JSON context: `inputs.*`, `trigger.*`, `event.*`
(curated aliases plus redacted `raw`), `steps.<blockId>.*` (each block's
declared outputs), and run metadata. Liquid (the hardened ADR 0102 setup,
`${{ }}` delimiters) renders prose fields. The Code block computes values.

`kind: user | builtin`. A built-in's graph is read-only and copyable
(Duplicate produces an editable user copy); its input values are editable.

### D2 — One interpreter workflow; definitions are data

DBOS derives the application version from each registered workflow function's
source (`origFunction.toString()`, no recursion into helpers). ADR 0100 kept
the whole review graph inline in one function body for this reason. A generic
engine inverts the guarantee: the registered body stays constant while the
graph it executes is data.

We register **one** workflow, `AutomationRunWorkflow` (the name is kept so
the ADR 0104 sweep policy carries over). Its body is deliberately thin:

1. `step:__snapshot__:0` loads the `automation_version` pinned on the run row
   and resolves inputs. The snapshot is a checkpointed step output, so a
   replay walks the same graph even if the automation is edited mid-run.
2. `interpretAutomation(input, deps)` walks the snapshot.

Two rules make this safe:

- **The step contract.** The registered body contains a literal
  `const ENGINE_STEP_CONTRACT = 1`. Any change to step naming, step order,
  recv semantics, or the finalize position MUST bump the literal. The bump
  rotates the DBOS application version, so in-flight executions from the
  previous deploy strand loudly and the sweep (adopt, 48 h) fails them —
  they are never silently replayed through changed semantics. Additive
  changes (new block types, new outcome fields) keep the contract.
- **The golden test.** A step-sequence test asserts the exact ordered step
  names for linear, branch, loop, retry, deadline, stop, and supersede
  graphs. Accidental contract drift is a red diff at review time.

Determinism rules inside the interpreter:

- Step names are `step:<framePath>:<attempt>`, where `framePath` joins frames
  `blockId[iteration]` with `.`. The frame path doubles as
  `automation_step_run.block_id`, so every loop iteration and every retry
  attempt has one row and one checkpoint.
- Control flow (branch, loop-until, filter conditions) is evaluated **inside
  a step** and replayed from the checkpointed boolean, never re-evaluated.
- Retries are an engine-level loop (attempt N runs under `step:<path>:N`),
  never DBOS `retriesAllowed`. A typed error carries `permanent: boolean`;
  template errors and provider 4xx never retry.
- Deadline math never reads the wall clock in control flow: clock micro-steps
  (`step:<path>:clock:<n>` returning `Date.now()`) checkpoint time.

### D3 — Waits: one mailbox per run

The run id **is** the DBOS workflow id: `autorun:<automationId>:<deliveryKey>`
(deliveryKey = `<provider>:<deliveryId>` for integration events, the
occurrence epoch for cron, a minted uuid for manual runs). The interpreter
parks in `DBOS.recv(AUTOMATION_TOPIC, seconds)`; `null` is a typed `deadline`
outcome, not an exception. The inbox union:

```
session_idle | session_ended | signal | event | stop | supersede
```

Producers: a generic `automation-consumer` session listener (run_completed →
`session_idle`, terminal → `session_ended`, routed via the
`automation_session` binding), the `signal_automation` tool (in-band signals
such as `finder_done`), the dispatcher (`event` for join deliveries,
`supersede`), and operators (`stop`). Every send carries a non-empty
idempotency key (the ADR 0060 empty-key hazard guard is reused verbatim).
Early arrivals go to a workflow-local buffer, which is deterministic because
it is a pure function of the checkpointed recv history.

### D4 — Concurrency as a setting

`concurrency: {keyTemplate, policy}` on the automation, enforced by a PG
leasing row `automation_concurrency_claim (automation_id, concurrency_key) →
run_id` (INSERT … ON CONFLICT DO NOTHING + CAS + DELETE — the house pattern,
never an advisory lock):

- `queue`: the new run row waits `pending`; the holder's finalize releases
  the claim and promotes the oldest pending run (fixed run id, so a replayed
  finalize cannot double-start).
- `supersede`: CAS the claim to the new run, send `supersede` to the old one
  (the review product's one-active-per-PR rule, generalized).
- `skip`: record the new run terminal as `filtered` — auditable, never
  started.
- `join`: deliver the event into the active run's mailbox instead of starting
  a run (the Slack thread case). This retires the epoch-suffixed thread
  workflow ids of ADR 0060.

### D5 — Integration-owned triggers

Verification lives with the integration, not with a user-minted registration.
Each provider has one ingress route (`/api/v1/integrations/<provider>/events`)
whose verifier and signing secret come from the connector's `webhook.ingress`
facet (GitHub App webhook secret, Slack signing secret, a new Linear ingress
with `Linear-Signature` HMAC). Every verified delivery is written once to an
`integration_event` ledger (redacted; retention 20 per connection+event key
plus 7 days) and dispatched to every enabled automation whose trigger matches
(provider, connection, event key, scope). The connector's `webhook` facet
grows into an **event catalog** (key, label, description, typed field schema,
sample fixture) and a new **actions catalog** (`actions[]`: label, input
schema, HTTP/GraphQL/builtin execution, output mapping, idempotency
strategy) that the `integration_action` block executes with the connection's
org credential through `runIntegrationOp`.

The provider HMAC schemes (`github_hmac_sha256`, `slack_v0`) retire from
custom webhook registrations; `generic_hmac_sha256` remains as the escape
hatch for providers without a connector.

### D6 — User code in a QuickJS-on-WASM cage

The Code block runs `export default ({event, steps, inputs, trigger}) => value`
in QuickJS compiled to WASM (`quickjs-emscripten-core`, sync variant): no
host functions, no I/O, no fetch/timers/require; 32 MiB memory limit, 250 ms
interrupt-handler CPU budget, 512 KiB stack, frozen JSON input, JSON output
capped at 256 KiB, console capture for the editor. Boolean mode maps `false`
to the run status `filtered`. It executes inside a DBOS step, so replay reads
the checkpoint and never re-executes. An `EvalCode` RPC gives the editor a
Run button with the same limits.

Structured filter rows (`{path, op, value}` groups with all/any) compile to
one in-process evaluator shared by the filter block, branch, loop-until, and
trigger scope. Code is the escape hatch, not the default UI.

### D7 — Built-ins are real, seeded, and flag-gated

PR review and the Slack thread brain ship as definitions in code
(`orchestrator/src/automations/builtins/`), seeded at boot with the
seed-profile pattern (idempotent, unique-violation tolerant, content-hash
version bumps; user input values are merged with new-key defaults, never
overwritten). Review-specific product logic that is not a generic primitive
stays in code-registered **system blocks** (`open_review_pass`,
`review_policy_gate`, `slack_thread_relay`) that only built-in definitions
may reference. If a system block proves generic, it graduates to the catalog.

The old graphs stay live during a parallel window: per-repository
(`review_enrollment.engine`) and per-channel (membership in the built-in's
`channels` input) flags select the engine, with env kill switches. At parity
the legacy files, tables, and RPCs are deleted — a clean break, with the
interpreter-driven built-in tests replacing the legacy workflow tests in the
same PRs.

`review_enrollment` lifts into the review built-in's `repos` input at first
seed and is dropped at the end. GitHub `installation_repositories` events
reach the ledger but never write the input.

### D8 — Sessions are kept by default

A run that creates a session leaves it alive when the run ends (today's
automation behavior — the task stays in the list for a human). Teardown is
explicit: an `end_session` block, or `end_sessions_on_finish` on the
automation, honored by the single trailing `step:__finalize__:0` that every
exit path reaches (terminal status, claim release/promotion, session
teardown). The review built-in ends its worker sessions explicitly.

## Statuses

Run: `pending → running ⇄ waiting → completed | filtered | failed |
superseded | halted | deadline`. Step: `pending running waiting succeeded
failed skipped`. `filtered` (a filter or skip policy ended the run) is
distinct from `failed` so "why did it not fire?" has an answer.

## Data model

Phase 1 (migration 0075): `automation_version (automation_id, version)`
{trigger, blocks, inputs_schema, settings}; `automation_step_run (run_id,
block_id, attempt)`; `automation_session (session_id PK, run_id, block_id,
role, keep)`; `automation_concurrency_claim`. `automation` gains
kind/builtin_key/current_version/inputs/concurrency/end_sessions_on_finish
and drops trigger/action; `automation_run` gains version/delivery_key/
concurrency_key/context/started_at/ended_at and a unique
`(automation_id, delivery_key)`. The same migration rewrites every existing
automation into version 1 with a single `create_session` block and every
existing run into a run plus one step run; non-terminal legacy runs are
force-failed (`migrated: engine rebuild`) so no old-body DBOS execution is
adopted by the new body.

Phase 2 adds `integration_event`. Phase 4 adds `review.automation_run_id`
and the window flag, then drops `review_session`, `review_enrollment`,
`webhook_sample`, and `review.workflow_id`.

## API surface

Phase 1 keeps `automation.proto` byte-identical: the RPC layer maps the
legacy action to a single-block version 1 and back, and one
`legacyRunStatus()` maps new run statuses onto the strings the current page
renders. Phase 3 rebuilds the proto as a clean break: versioned saves,
structured per-block errors, inputs, Duplicate, TestRender/DryRun/EvalCode/
RunNow, and a new `AutomationRunService` (list/get with step runs, stop,
retry). `IntegrationService` gains `ListEventCatalog` and `ListActionCatalog`.

## Phasing

1. **Engine + migration** — ADR, inert engine core, the atomic schema/runtime
   swap (0075), session signals. The existing editor keeps working.
2. **Integration triggers + actions** — facet growth + fixtures, ingress
   spine + `integration_event`, Linear ingress, trigger matching + catalogs,
   run_command/write_files, QuickJS sandbox, action executor, HMAC
   retirement.
3. **Automations UI** — proto v2, list/editor (Build|Inputs|Runs|Settings),
   variable picker + samples, CodeMirror Code inspector, run timeline,
   versions, Reviewed-repos redirect.
4. **Built-ins** — extraction groundwork, review system blocks + built-in +
   per-repo flag (window opens), Slack relay + built-in + per-channel flag
   (window opens), then the two deletion PRs and the ADR bookends
   (this ADR → Accepted; ADR 0060/0100/0102 amended).

## Correctness and security invariants

- Unattended sessions never carry human credentials
  (`integrationPrincipalId = workflow:<automationId>`, no owner).
- The orchestrator is the only writer to providers; integration actions use
  the connection's org credential scoped to the action's declared powers.
- Payloads are data: redaction on ingress, the untrusted-input framing
  wherever `event.*` enters a prompt, one hardened Liquid sandbox, one caged
  JS sandbox.
- Idempotency everywhere it exists today: delivery-key run ids,
  session-scoped prompt ids, attempt-scoped exec ids, marker-checked
  integration writes, non-empty DBOS send keys.
- Every registered workflow keeps an ADR 0104 sweep policy; the boot
  assertion stays.
- Admin-only authoring; member visibility unchanged; worker session
  transcripts follow the null-owner rule.

## Consequences

- One durability substrate and one product surface for unattended work; the
  review and Slack products become data plus three system blocks.
- ~1,300 lines of bespoke review workflow and the whole thread-workflow
  epoch machinery retire.
- The interpreter's step contract becomes a load-bearing invariant with a
  named literal and a golden test — the cost of definitions-as-data.
- A parallel-run window temporarily keeps two paths alive for review and
  Slack; the flags and kill switches bound the risk, and the deletion PRs
  close the window.
