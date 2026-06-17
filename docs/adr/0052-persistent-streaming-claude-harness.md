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

## Progress (bookend updates)

- **2026-06-17 — Phase 1 SHIPPED + prod-validated** (PR #327). The streaming
  engine rewrite merged and was validated end-to-end in prod against
  `demo-claude`: the in-guest harness log shows `spawning persistent claude
  argv=[… --input-format stream-json …]` (one persistent process, no
  per-prompt respawn), and two prompts over that one process produced two
  distinct `run-<uuid>` runs, each cleanly bracketed `run_started → … →
  run_completed → harness_idle`, with context continuity (4 → 40). Resume path:
  `RefreshImage` (gRPC) re-captures the base snapshot from the re-baked image.
  - *Divergence found:* the prompt-after-idle path still hits the pre-existing
    "sandbox not found" / frozen-`last_active_at` idle-eviction desync (filed
    as issue #329) — NOT a Phase-1 regression (the harness was provably alive);
    it's exactly the class Track A backstops and Phase 2 addresses.
  - *Regression fixed:* Phase 1 had set `RunStarted.prompt_summary = Some(text)`,
    which the web renders as a user turn IN ADDITION to the coord's `role:user`
    echo → every prompt double-rendered. Restored `prompt_summary = None` (the
    coord echo is the single authoritative user turn).

- **2026-06-17 — Phase 1b IMPLEMENTED** (this PR). Harness-owned queue, reflected
  up the existing channel: `HarnessCommand` gains `EditQueued`/`DequeueQueued` +
  a `prompt_id` on `Prompt`; `HarnessEvent` gains `PromptQueued`/`PromptEdited`/
  `PromptDequeued` + a `prompt_id` on `RunStarted` (all APPEND-ONLY — the bincode
  `wire_golden.rs` corpus enforces no variant-index shift). The harness queues a
  mid-turn prompt (stays editable), consumes it at the turn's `result`, and the
  consuming `RunStarted{prompt_id}` is the web's "consumed" signal. Down-path
  RPCs (`EditQueuedPrompt`/`DequeueQueuedPrompt`) thread coord→host→hub; the
  client `prompt_id` threads through `SendPrompt` and tags the coord user-echo so
  the web dedupes. Web: a submitted prompt shows an immediate **greyed optimistic
  bubble** (keyed by `prompt_id`) that transitions in place to the authoritative
  echo and stays greyed until consumed — never "lost" in the send→echo gap.
  - *Deferred within the phase:* the editable-queue UI affordance (edit/cancel a
    greyed item via the new RPCs — backend ready) and threading the initial-prompt
    `prompt_id` through `CreateSession` (the web loads the first view from server
    events, so there's no optimistic first-bubble to dedupe).

- **2026-06-17 — Phase 1b follow-up: command-side at-least-once** (this PR).
  Prod session `8c165749` wedged — the user's 2nd prompt ("Hi") never started a
  run and its bubble stayed greyed forever. Root cause: the host→harness
  **command** channel was fire-and-forget, unlike the at-least-once *event*
  channel (`held`). `send_prompt` buffered the `Prompt` into the live
  connection's writer and returned `Ok` (the coord then emitted the durable user
  echo), but if that connection bounced before the harness processed the frame —
  routine in this system: checkpoints, live moves, idle-evict, the SIGUSR1
  reconnect nudge — the prompt died with the connection and was never
  re-delivered. The initial prompt is env-seeded, so "Hi" was the *first* prompt
  ever to cross the wire as a command — which is exactly why turn 1 worked and
  turn 2 wedged. (Ruled out: a version skew — turn-1 events carrying the
  Phase-1b `prompt_id` decoded fine host-side; and a deterministic per-prompt
  break — `wire_golden` pins the `Prompt` round-trip green.) **Fix:** the hub
  holds un-confirmed prompts per sandbox and **replays them on every (re)attach**
  (the command-side twin of `held`), clearing each when its
  `RunStarted{prompt_id}` / `PromptQueued` / … confirmation arrives; the harness
  **dedupes by `prompt_id`**, so a replay of an already-processed prompt is a
  no-op. No wire change. Reproduced + regression-pinned by a host-agent
  integration test (`harness_command_redelivery`) and a harness engine unit test
  (`duplicate_prompt_replay_is_ignored`).
  - *Durable follow-up:* a host-held buffer dies with the host, so this covers a
    same-host connection bounce (the prod case) but not a full eviction/resume —
    the coordinator replaying un-consumed prompts on respawn (the ADR's stated
    "queue survives eviction") remains the broader resilience item.

- **2026-06-17 — Phase 1c IMPLEMENTED: live token streaming** (this PR). Until now
  the persistent engine emitted only the *complete* `assistant` message per block,
  so a pure-prose turn (e.g. "count 1→60") showed a spinner until the whole answer
  landed. Phase 1c streams the tokens, unlocked directly by the persistent engine
  and the `--include-partial-messages` CLI flag (confirmed on the baked `claude`
  2.1.179: `stream_event`/`content_block_delta`/`text_delta` lines arrive *before*
  the complete `assistant`, and `message_start.event.message.id` == that message's
  id — so chunks key onto the same `message_id`).

  **Two tracks, kept strictly separate (the design-of-record).** The complete
  `AgentMessage` stays the **durable** record (persisted to `session_events`, the
  authority). The token deltas are a new **ephemeral** `HarnessEvent::AgentMessageChunk`
  (appended at bincode idx 10 — `wire_golden` pins no shift): streamed live, **never
  persisted, no `idx`**. We deliberately do NOT write a row per token — that would
  put hundreds of PG inserts on the hot path and flood the durable log (rejected
  against the "event log is the authority / latency non-negotiable" rule).

  **Cross-replica without per-token DB load.** Prod runs 2 coordinator replicas
  with no session affinity, so same-replica-local-bus streaming would animate only
  ~50% of sessions. Chunks fan out on a **dedicated `session_event_deltas` NOTIFY
  channel** carrying the full event inline (vs. the persisted-row channel's
  `{session_id, idx}` + re-fetch). The producing replica does NOT publish locally —
  the NOTIFY echo (every replica `LISTEN`s it, including the producer) is the single
  delivery path, so a client on a replica without the harness connection still
  streams. Rate is bounded by the chunk rate (the API already token-batches);
  payloads are clamped (harness `MAX_CHUNK_BYTES` = 6 KiB) under PG's 8 KB NOTIFY
  ceiling, with a defensive skip if exceeded. On the SSE/gRPC merge, ephemeral
  events bypass the replay high-water gate and frame with `idx=None` (the same path
  the `Lagged` sentinel already proves end-to-end → orchestrator forwards it with no
  `id:` line, no change needed there).

  **Merge-safety: the durable record ALWAYS wins** (the load-bearing invariant — it
  must not break when pods cycle mid-message). The web keeps chunks in a live
  **overlay** (`useSessionEvents`, `Map<message_id → text>` → `streamingText`),
  STRICTLY OUT of the durable `events` array. `buildMessages` appends the overlay
  tail to the in-flight assistant turn (gated on `runOpen`), but: the terminal
  durable `agent_message` **prunes the overlay for its `message_id`** (flushed in
  the same update as the event append, so no frame shows both); a `run_completed`/
  `run_interrupted` **clears the overlay** (covers a crashed turn that streamed
  partials but produced no final message — the `ba3ae8d5` crash class); a finalized
  id **drops late/out-of-order stragglers**; reconnect/session-switch **resets** it
  (chunks are never replayed). So under a replica cycle (harness re-dials, durable
  events ride the at-least-once `held` slot; deltas resume from the new replica) or
  a web reconnect (overlay resets, replay rebuilds durable), the worst case is lost
  *animation*, never a wrong or doubled transcript. Pinned by web tests
  (`buildMessages` tail/supersede/`runOpen`-gate + `useSessionEvents`
  accumulate/supersede/straggler-drop/terminal-clear/dedup) and coordinator framing
  tests (ephemeral → `idx=None`).
  - *No wire/coord change reaches the durable path:* `AgentMessageChunk` is
    additive; the sink branches on `kind == "agent_message_chunk"` → `notify_session_delta`
    and returns before any append; `IndexedEvent` gains an `ephemeral` flag (default
    false). Harness-side it's `--include-partial-messages` + a `stream_event` arm in
    `translate_jsonl` tracking `current_message_id`.
  - *Deferred:* harness-side time-coalescing of chunks (the API's batching keeps
    volume reasonable for v1; revisit if a long turn lags the 256-slot bus) and a
    per-chunk `at` ordering guarantee (the durable message corrects any reorder).

- **2026-06-17 — Phase 3 IMPLEMENTED: `control_request` interrupt** (this PR,
  stacked on Phase 1c). Replaces the Phase-1 stopgap interrupt — *SIGINT the whole
  persistent process, then respawn `--resume`* — with claude's **in-band
  `control_request`** on the held-open stdin: claude acks (`control_response`),
  aborts the in-flight turn (surfacing as `result subtype=error_during_execution`),
  and **stays alive**. The engine's `result` handler already had the receiving
  branch (interrupted_run → `RunInterrupted`, process alive); Phase 3 just flips the
  `Interrupt` command from `sigint_child` to `write_control_interrupt` + arms a
  grace deadline.

  **This is the root-cause fix for the prod interrupt bug** (session `ba3ae8d5`):
  interrupting *with a queued message* SIGINT-killed the persistent claude, the
  engine respawned `--resume`, and the queued message was written into that
  freshly-respawned process — which (a) had lost conversation context ("There's no
  prior context in this conversation") because SIGINT-mid-turn kills claude before
  it flushes its transcript, and (b) exited immediately, which the harness — with
  its `interrupted_run` marker reset by the respawn — misread as an abnormal crash
  (`RunCompleted{ok:false}` → the user's "An error occurred"). With `control_request`
  there is no teardown and no respawn: context is preserved (it never leaves
  claude's live memory) and the queued message runs as a clean next turn on the
  same process (`RunInterrupted` → consume-on-result → `RunStarted{prompt_id}`). The
  consume-on-result boundary already auto-runs a queued message after a turn ends,
  so "interrupt auto-sends the queued message" is preserved — but now cleanly.

  **Reliability — SIGINT demoted to a fallback, an interrupt can never wedge.** If
  the control frame can't be written, or isn't honored within `INTERRUPT_GRACE_SECS`
  (= 8s; a pinned build lacking the frame), the sleeper escalates to SIGINT +
  respawn — the old path, now a safety net rather than the default. Empirically
  grounded: Phase 0 confirmed on the baked `claude` 2.1.179 that the
  `control_request` interrupt acks, aborts (`error_during_execution`), and the
  process survives to process the next message.

  Crash recovery (the other half of the plan's Phase 3) was already in place from
  Phase 1 — unexpected EOF mid-turn ⇒ `describe_abnormal_exit` System message +
  terminal `RunCompleted{ok:false}` + auto-respawn `--resume` with a bounded
  fast-crash counter — so Phase 3 adds only the interrupt mechanism. No wire / coord
  / web change: `RunInterrupted` already exists and the interrupt RPC path is wired
  end-to-end (the Phase-1c composer's Esc → `Interrupt`). Pinned by two engine tests
  using a control-frame-aware fake claude that records its PID: a bare interrupt
  aborts with **no respawn** (PID unchanged), and interrupt-with-a-queued-message
  steers onto the **same** process (`RunInterrupted` → `RunStarted{p2}`, PID
  unchanged, no crash artifact) — a direct `ba3ae8d5` regression guard.
  - *Retires:* the SIGINT-vs-persistent-process hazard ([[project_adr0030_operator_interrupt]])
    on the normal path — no signal racing FC suspend/resume.

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
