# ADR 0052: Persistent streaming Claude harness session

Status: 2026-06-16 — **Proposed**.

## Context

Prod session `bf3dbbcb` wedged: a warm-reattached harness (ADR 0037) desynced
after a periodic checkpoint — it emitted a bare `agent_message` with **no
enclosing `run_started` and no `run_completed`**, then went silent, while the
VM stayed healthy. The root cause is an inference defect in the current
harness model.

Today `engram-harness-claude` runs **child-per-prompt**: per `Prompt` it spawns
a fresh `claude --print --output-format stream-json [--resume <id>] "<text>"`,
the child exits after each turn, and **turn boundaries are inferred from
stdout** — `system`/`init` ⇒ run start, EOF + a scraped `result` marker ⇒ clean
end, EOF *without* `result` ⇒ crash. The fatal ambiguity: a clean turn-end and
a crash/stall both surface as the same `Ok(None)` EOF, distinguished only by a
heuristic. When that heuristic misfires, the run never closes and the session
wedges, with no clean recovery (ADR 0034's L3 backstop keys off event silence,
which a desync that keeps emitting can defeat; see the ADR 0034 Track A
addendum for the server-side safety net that backstops this class regardless of
harness).

This ADR is the **root-cause fix**: run `claude` as a *persistent streaming
session* with stdin held open (`--input-format stream-json`). Turn-end becomes
an **explicit `result` message on the live stream**; process EOF/exit becomes
an **unambiguous disconnect/crash**. Every `run_started` is then always paired
with a terminal event (even on crash), and crashes auto-recover instead of
silently wedging.

## Empirical validation (local `claude` 2.1.179)

Direct experiments against a real `claude` CLI confirmed the load-bearing
protocol assumptions:

- **Persistent multi-turn over one process works.** `--print --input-format
  stream-json --output-format stream-json --verbose` holds stdin open, processes
  multiple newline-delimited user messages over one long-lived process, and
  exits 0 cleanly on stdin EOF.
- **Explicit per-turn terminator.** Each turn ends with `{"type":"result",
  "subtype":"success",...,"is_error":false,"num_turns":N}` on the same stream.
  EOF is now a distinct signal = real disconnect.
- **In-process context continuity** — the same `session_id` spans turns; no
  `--resume` between turns. (`system`/`init` is re-emitted *per turn*, so
  `RunStarted` is synthesized on prompt-accept, not from `init`.)
- **Queued messages / steering enabled.** A message pushed to the open stdin
  mid-turn is accepted + acked but does NOT redirect the running turn — it runs
  as the next turn at the boundary. Immediate steering = interrupt + queued
  message composed. (Impossible today: `stdin(null)`, child-per-prompt.)
- **Interrupt-without-teardown.** `{"type":"control_request","request_id":...,
  "request":{"subtype":"interrupt"}}` on stdin is acked with a
  `control_response` success and **aborts the in-flight turn** (`result
  subtype=error_during_execution, is_error=true`) while the process stays alive.
  This retires SIGINT as the interrupt primitive (demoted to a fallback).

## The teleport gate (verified in code)

Both lifecycle paths that matter restore via **UFFD, never File-backend**
(`engram-sandbox-firecracker/src/lib.rs`): idle resume follows
`config.restore_mode` (UFFD in prod, `:1046`); a live teleport/migration forces
`RestoreMode::Uffd` unconditionally (`:1093`). The documented "restored idle
claude won't see its first post-restore stdin turn" wedge (ADR 0037) is
**File-restore-specific** — off the critical path for both idle-resume and
teleport. Consistent with prod (`bf3dbbcb` restored `mode=Uffd` and served two
post-resume turns). So keeping a streaming claude warm across a teleport is
**likely mostly-free** — but a **Phase 0 spike** must prove a held-open in-guest
pipe survives a UFFD restore before Track C relies on it.

## Decision

1. **Idle/resume = respawn-with-resume.** On idle eviction, close claude's
   stdin so it drains to a final `result` and exits cleanly *before* the
   snapshot; on resume, spawn `claude --resume <id>` (reloads context from the
   on-disk JSONL via the existing `/workspace/.engram/claude-session-id`). Fresh
   process = fresh epoll each wake. This is the mainstream-proven path (OpenHands
   and the Claude Agent SDK both resume by reloading a disk transcript).

2. **Live mid-turn teleport (ADR 0045) — warm, gated on the Phase 0 spike.** A
   run actively generating crosses warm in the UFFD memory image; the migrated
   process keeps reading its stdin pipe. If the spike fails, the fallback is a
   Morph-style tmux buffer in front of `claude`, or an explicit
   accept-turn-restart-on-teleport decision — recorded here, not assumed.

3. **Turn lifecycle from explicit signals.** `RunStarted` synthesized on
   prompt-accept (owns the `run_id` before any output, eliminating the
   run_id-less bare-event hazard); `result` ⇒ `RunCompleted`; `result
   subtype=error_during_execution` ⇒ `RunInterrupted`; mid-turn EOF ⇒
   unambiguous crash ⇒ terminal `RunCompleted{ok:false}` + `Idle` + a System
   artifact + auto-respawn `--resume` (bounded fast-crash counter).

4. **Interrupt via `control_request`** on stdin (process survives); SIGINT only
   as a fallback for `claude` builds lacking the control frame.

5. **Queued / steered messages — the harness owns the queue.** The harness is
   the single-writer owner; it buffers pending prompts in its own memory and
   writes one to claude's stdin at the consumption boundary (turn `result` for
   type-ahead; after a `control_request` interrupt for immediate steer). The
   queue is reflected upward over the existing channel (no new authoritative
   store): `HarnessCommand` gains `EditQueued`/`DequeueQueued` (down);
   `HarnessEvent` gains `PromptQueued`/`PromptEdited`/`PromptDequeued` + a
   `prompt_id` on `RunStarted` (up), auto-flowing through `harness_event_sink` →
   `session_events` → SSE. Consumption (`RunStarted{prompt_id}`) is unambiguous
   and single-writer, so a `DequeueQueued` arriving post-consumption is a no-op.
   The web renders a greyed/editable composer item (reusing the `rewound`
   greying) that moves into the thread on `run_started{prompt_id}`; the queue
   reconstructs from replayed `session_events` (refresh / multi-client). Because
   every mutation is a durable event, the coordinator can replay un-consumed
   queued prompts to the harness on respawn (queue survives eviction).

## Rollout (stacked PRs; ADR bookends)

- **Track A — server-side resilience** (ships first, independent; ADR 0034
  addendum, *not* this ADR). The desync watchdog + `last_event_at` +
  non-destructive re-handshake. Makes the wedge class self-heal regardless of
  harness. (PR #324.)
- **Phase 0** (this PR) — ADR 0052 Proposed + the spike: pin the baked-`claude`
  version floor (`control_request` support), the `#41230` mid-turn-queued-
  message persistence check, and the FC held-pipe-survives-UFFD-restore test
  wired into `ci.yml`.
- **Phase 1** — streaming engine rewrite (`engram-harness-claude`), respawn-with-
  resume. **Phase 1b** — harness-owned queued/steered messages. **Phase 2** —
  clean-idle-shutdown (drain stdin before the idle capture; gated to
  `CheckpointReason::Idle`). **Phase 3** — `control_request` interrupt + crash
  recovery. **Phase 4** — warm mid-turn teleport (gated on Phase 0); its merge
  flips this ADR to **Accepted**.

## Prior art

Respawn-with-resume for idle is the norm (OpenHands cold-loads `base_state.json`
+ per-event JSONL; the Claude SDK resumes by transcript JSONL keyed on
session-id+cwd). Explicit turn-end status is universal (OpenHands
`RUNNING/FINISHED`; the SDK's typed stream). Warm-snapshot vendors (E2B, Fly,
Morph) snapshot the whole VM but none document a held-open-stdin agent resuming
mid-turn — Track C is novel, hence the spike. Morph runs the `claude` CLI inside
tmux, the documented Track-C fallback.

## Consequences / risks

- The `--input-format stream-json` held-open-multi-turn contract is
  semi-undocumented (anthropics/claude-code#24594); Phase 0 pins the version
  floor against the real sidecar binary.
- Mid-turn messages may not persist to the resume JSONL until their turn
  completes (#41230) → a crash between enqueue and persist could drop a queued
  message; Phase 0 measures this, and the harness may need to locally buffer
  un-persisted queued input across a respawn.
- The SDK V2 removed `SDKSession` interrupt (0.3.142); we target the CLI
  `control_request` protocol directly, not an SDK method.
- Wire-breaking changes (new `HarnessCommand`/`HarnessEvent` variants, `prompt_id`
  on `RunStarted`) are acceptable per the repo's clean-break policy (re-bake +
  roll); event `kind` strings stay stable.

Cross-ref: ADR 0030 (interrupt), 0034/0039 (idle eviction + the Track A
addendum), 0037 (warm-harness findings), 0045 (teleport), 0022 (memory modes),
0028 (checkpoint durability).
