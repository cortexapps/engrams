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
