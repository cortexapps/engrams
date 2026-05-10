# ADR 0002: Engram is a one-shot agent task runner

Status: **amended by [ADR 0005](./0005-disk-pressure-blob-tier.md), 2026-05-09**

Original status: accepted, 2026-04-29
Phase: Phase 4 cleanup, post-Claude-adapter prep

> **Amended.** ADR 0005 widens the durability contract from
> "session lives ↔ FC snapshot exists on its origin host" to
> "session lives ↔ snapshot exists somewhere (hot tier on a host's
> NVMe, or cold tier in blob)." The one-shot semantics this ADR
> established still hold: sessions don't infinitely migrate, hot
> resume stays same-host, `Dead` is terminal — the new shape just
> moves the Dead boundary from "snapshot lost from this host" to
> "snapshot lost from both tiers."
>
> Specifically:
>
> - The `Dead` terminal state stays. ADR 0005 widens the
>   conditions: now you reach Dead only when the cold blob is also
>   gone (deleted, KEK lost, intentional GC).
> - `HTTP 410 Gone` on resume against an invalidated snapshot stays.
> - `engram session fork` is gone (the git surface it depended on
>   was retired by ADR 0005). Cross-host continuation now happens
>   automatically via cold resume; no caller-driven fork step.
> - The harness contract ("emit events while alive; engram doesn't
>   resurrect you") is unchanged. Cold resume materializes a fresh
>   sandbox + a fresh harness child — same shape as hot resume.
>
> Body kept verbatim below.

---

## Context

ADR 0001 captured the trajectory away from cross-host VM-memory
replication. With that out, the next plausible cross-host story was
"reconstruct the agent's transcript from `transcript_delta` bytes on
a fresh VM in a different host." We built the surface for it
(checkpoint branch + transcript reconstruction plumbing), but the
abstraction was Claude-shaped — it assumed a JSONL conversation file
that could be byte-replayed. Stress-testing against OpenCode (SQLite
conversation store, no externally-settable session id) showed the
abstraction didn't generalize.

User feedback that finalized the simplification: *"Maybe we just
drop the whole cold resume option altogether?  We can still use Git
to store the work we've done so far, but if a session fails or is
expired from disk cache, maybe that's just the end of it."*

## Decision

**Engram is a one-shot agent task runner.** The session lifecycle is
bounded by FC-snapshot lifetime on a single host:

```
create → Active (live VM)
       → idle TTL → Idle (FC snapshot taken, VM destroyed)
       → next prompt/exec → Active (hot resume from snapshot)
       ... loop ...
       → snapshot invalidated (host crash, disk full, hard TTL)
       → Dead (terminal)
       → caller forks if they want to continue from the workspace
```

**Cross-session durability primitive: git.** The workspace is
checkpointed on the `engram/sessions/<id>` branch. A Dead session's
workspace is still recoverable as that branch's HEAD; new sessions
fork from it.

**Conversation log is observable, not replayable.** Live consumers
(Slack bot, web UI, audit) read `GET /sessions/:id/events` SSE while
a session is alive. Engram persists every event in `session_events`
forever, so historical conversations are queryable, but the agent
itself is *not* expected to resume a past conversation across
sandbox death — that's a per-adapter best-effort feature, not an
engram primitive.

## What this removes

- `?from_event_idx=N` "go back in time" resume parameter — gone.
  Use `engram session fork --at N` instead (creates a new session;
  fresh agent; workspace at the fork-point SHA).
- `resume_from_git_checkpoint` cross-host cold-resume code path —
  gone. Resume is FC-snapshot-only.
- `smart_bootstrap_to_branch` and `run_workspace`/`capture_head_sha`
  helpers — gone (only used by the removed cold path).
- `POST /sessions/:id/migrate` operator endpoint — gone. Its only
  semantics were "trigger cold resume on next access," which no
  longer exists. Operators can `engram session delete` to terminate
  or `engram session fork` to start over from the workspace.
- `transcript_delta: Vec<u8>` on `HarnessEvent::ToolCallCompleted` —
  gone (replaced in Track B by structured `result_summary` + a new
  `AgentMessage` event for assistant text).
- `SessionStatus::PendingReassign` → renamed `Dead`. The semantics
  were already "host died, awaiting cross-host rehydrate"; with cold
  resume gone, the state is just terminal.
- The cold-resume `bootstrap`-side transcript reconstruction step
  (was planned, never implemented; dropped before write).

## What this adds

- HTTP `410 Gone` from `POST /sessions/:id/resume` when the
  session's FC snapshot is invalidated. Body: `{"error":
  "snapshot_invalidated", "message": "...; use engram session
  fork <id> to continue from the workspace"}`. Surfaces the
  state-machine clearly to API clients.
- `SessionStatus::Dead` as the terminal state. Distinct from
  `Failed` (= something went wrong creating the session).

## What this preserves

- Hot suspend + auto-resume via FC snapshot. Same-host, sub-second.
  Bread-and-butter idle-eviction loop is unchanged.
- The harness wire protocol's tool-call events. UIs / Slack /
  audit consumers keep working (and Track B improves them).
- `engram session fork` workspace continuity.
- The pre-existing checkpoint primitive (`POST /sessions/:id/checkpoint`)
  and per-Idle auto-checkpoint flow.

## Consequences

**Simpler.** The cleanup removes ~600 LOC across `api/snapshot.rs`,
`api/sessions.rs::migrate`, and several test fixtures, plus the
`transcript_delta` machinery. ADR 0001's "VM-memory replication is
out" stays accurate; this ADR specifies what we replaced it with
(nothing — sessions are bounded).

**Slower preemption recovery.** On a real preemption (host dies),
sessions move to Dead. The caller can fork the workspace to start a
new session, but the conversation is lost. For autonomous workflows
this is fine — re-prompt the new session. For interactive flows
(Slack pair-programming), the user sees "session ended"; click the
fork button to continue.

**Adapter abstraction is now uniform.** The harness contract is
"emit events while alive; engram doesn't try to resurrect you." Each
adapter (Claude Code, OpenCode, Codex, custom) gets the same simple
deal — no per-agent "how do you reconstruct conversation state on a
different host" cleverness required.

## Alternatives considered

- **Keep `transcript_delta` for rich UI rendering.** Considered;
  rejected for v1. Track B's structured `AgentMessage`,
  `args_summary`, `result_summary` fields cover Slack and basic
  web UI cases. Full-fidelity rich rendering can come back as
  `agent_payload: Option<Vec<u8>>` later if a consumer needs it.
- **Keep cross-host cold resume but loosen the contract.** "Best-
  effort restoration" felt like it'd never get used in practice
  and would keep paying complexity tax. Cleaner to delete and
  add back if and when there's real demand.

## Implementation reference

Plan file:
`~/.claude/plans/yup-let-s-plan-out-peaceful-rivest.md` —
Track A "Cleanup".
