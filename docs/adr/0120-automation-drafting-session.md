# 0120 — The automation drafting session

Status: 2026-08-25 — **Proposed.**

## Context

Builder v2 makes plain-English composition the front door of Automations: a
person describes a workflow, an agent assembles a draft they can watch,
edit, and enable. The agent must know the live block catalog, save only
validated definitions, and coexist with a human editing the same automation
by hand.

Spec mode (ADR 0114) proved the mechanism for agent-drafted artifacts:
task-type-gated injected tools, soft-refusal mutations, session events as
the conversation. It also carries machinery this feature must NOT inherit:
a CRDT document substrate, phase gates, org-wide multiplayer.

## Decision

**D1 — The versions are the draft.** No document substrate. The drafting
agent's proposals land as ordinary `automation_version` rows on a disabled
automation via `AutomationStore.saveVersion`. The Builder renders them as
they land; history, diffing, and undo come from the version table the
product already has.

**D2 — One column is the whole model, and it is the authorization.**
`automation.draft_session_id` (migration 0085) binds one session to the
automation it drafts. Every draft tool resolves its target by
`draft_session_id = ctx.sessionId` and refuses otherwise. The admin gate
applies once, at `DraftAutomation` (the RPC that creates the disabled
automation and boots the session through `createTaskWithSession` with task
type `automation_draft`); the binding carries that authority to the tool
path, which bypasses Connect. No draft status machine: the person enables
the automation when they are satisfied, and that is the end of drafting.

**D3 — The version fence replaces coordination.** The human may edit in the
Builder while the agent works. `automation_propose` carries
`expected_version`; a mismatch returns `applied: false` WITH the live
definition, so the agent merges intent instead of clobbering. Symmetrically
the web refreshes a non-dirty editor when the version advances and offers a
reload banner when dirty. Optimistic concurrency on a small JSON value —
not a CRDT.

**D4 — The catalog is derived, never transcribed.** `automation_read`'s
`catalog` part projects the block registry live (`z.toJSONSchema` on each
executor's config schema — the same bridge as the tool manifest). The
system prompt stays static and conventions-only; a new block type reaches
the agent without a prompt change, and a golden test makes any registry
change a visible diff.

**D5 — Soft refusals.** Validation failures and fence conflicts are
`applied: false` payloads with block/field addresses, never thrown errors —
the model self-corrects (the spec-tools contract). Idempotency: every id
derives from (org, idempotency key, request hash); a byte-identical retry
replays, changed arguments mint a fresh draft.

## Consequences

- The drafting feature is four tools, one RPC, one column, one prompt
  branch — deletable in one PR if it fails as a product.
- Drafting inherits every future engine feature for free: new blocks appear
  in the derived catalog; validation changes apply to proposals
  automatically.
- The agent's writes are indistinguishable from a human's SaveVersion in
  the data model — parity by construction, at the cost of no agent-specific
  attribution beyond the version's `created_by_user_id` (the draft owner).
- Deep session-block validation (harness/model matrix) runs at SaveVersion
  and snapshot time, not in the propose tool; a proposal can carry a
  harness the catalog would refuse, surfaced when the person saves or runs.

## Addendum (2026-08-26): workstream instances

Amended here per the D9–D11 precedent — the instance model completes the
same middle scope this ADR opened (an automation as a durable product
surface, not a stateless trigger rule). No new ADR.

### The model

Runs stay short (D9) and the engine stays Model-2 stateless; the PRODUCT
gains **instances** ("workstreams" in the UI): one durable entity per
rendered identity key that many runs contribute to.

- `automation_instance`: identity (`key`, rendered from
  `settings.instance.keyTemplate` in the concurrency-key template scope),
  a kickoff-time **input snapshot** (instance-bound runs resolve
  `inputs.*` from it; the automation row demotes to defaults for new
  workstreams), and an open → closed lifecycle. A partial unique holds ONE
  open instance per key; closed instances accumulate as history, and
  re-kicking a closed key mints a fresh instance.
- **The handle ledger** (`automation_instance_handle`): external
  identifiers (`slack:<ch>:<thread_ts>`, `github:<repo>#<n>`) accumulate
  as side effects — the action executor writes its declared `handles`
  templates post-success in the same step; the pr-link consumer binds PRs
  opened by workstream sessions. One handle routes to one instance per
  automation, **forever**: a second claim refuses loudly (a typed
  permanent error in the executor; a loud log in the consumer) and a
  reopen never re-routes old threads. Exclusivity is the lesson of
  Zeebe's duplicate-subscription bug (replies routed to arbitrary stale
  instances); permanence is the audit trail.
- **Admission** runs before the run id mints, handle route first, then
  the key route under the entrypoint's `admit` policy: `open`
  (render/open/join), `require` (join an open workstream or drop — the
  instance-level continueOnly), `handle_match` (ledger only). Drops mint
  NO run row and land in the capped `automation_drop` ring — silent drops
  are the recurring support failure of every keyed routing system.
- **Scoping** is automatic: run ids gain `i-<instanceId>`, state is
  transparently prefixed `i/<id>/` (no automation-global escape hatch in
  v1 — a deliberate, revisitable decision), concurrency keys prefix
  `i:<id>:`, and session adoption never crosses workstreams.
- **Cron fans out**: one occurrence per open workstream per tick
  (occurrence identity includes the instance; the delivery identity stays
  instance-blind so one external delivery lands in at most one
  workstream). The schedule advances once per tick.

### The closed-instance event matrix (v1)

| Event class | Behavior |
|---|---|
| Handle names a CLOSED workstream | drop, audited in the ring |
| `require` entrypoint, no open workstream | drop, audited |
| `handle_match` entrypoint, no ledger hit | drop, audited |
| Kickoff (`open` policy) of a closed key | a FRESH workstream opens |

Deliberately typed by event class, not global (the PagerDuty dedup-key
matrix precedent; Temporal's `signalWithStart` regret is coupling
route-and-create without designing revive-vs-drop-vs-successor). Named
future options, NOT in v1: per-entrypoint closed policies and **successor
instances** (an event on a closed workstream opens a successor carrying
the handle forward).

### Precedence (rung 1) and the routing ladder

When a handle-bound OPEN workstream owns a slack event's thread, the
`slack_brain` catch-all stands down for that delivery — dispatcher
suppression (`result.suppressed`) honored by the legacy route too. One
thread, one responder. The suppressible set is a single reviewed
constant. The documented ladder: explicit address > thread handle >
channel handle (future rung 2: an instance claims `slack:<channel>`) >
the brain — with rung 3 (the brain as router: ListInstances + a
forward-into-workstream tool, LLM arbitration only for unbound mentions,
its decision made durable as a handle) as the designed follow-up.

### Kickoff and surfaces

`RunNow` gains `instance_key`/`instance_inputs_json`; an instanced
automation REJECTS the legacy `inputs_json` (which silently mutated every
future run — the wart the snapshot retires), and joining an open
workstream with non-empty inputs is an error (a snapshot is never
silently ignored). `ListInstances`/`GetInstance` (with handles)/
`CloseInstance`/`ListRecentDrops` are the read/lifecycle surface;
`instance_close` is the in-graph close (a run may close only its OWN
workstream). Product naming: "instance" stays out of the UI — surfaces
label the entity by its rendered key; generic chrome says "workstream".

### Flagged consequences

- Reconciliation is push-only in v1; a staleness sweep polling the
  handle's source of truth is the designed follow-up (push-only
  correlation is fragile — the Temporal-practitioner lesson).
- No explicit rebind override yet (Devin's `!new` gesture) — follow-up.
- `continueOnly` and slack-brain stay byte-identical; migrating the brain
  onto instances is a later campaign.
