# 0089 — Generic tool protocol: orchestrator-registered tools for every harness

Status: Proposed

## Context

The orchestrator has no way to hand a tool to the agent running inside a session.
If we want the model to be able to call "add a review comment" or "save this to
memory" — tools whose implementation lives in the orchestrator, next to the task
model and the integrations — today each one would need bespoke code in every
harness, a new wire message pair, coordinator plumbing, and its own delivery
semantics. We have exactly one such tool today, and it proves the point:
**AskUserQuestion** took a dedicated `HarnessEvent::UserQuestion` /
`HarnessCommand::AnswerQuestion` pair (`crates/engram-harness-proto/src/lib.rs`),
its own coordinator event kinds and outbox kind (`OutboxKind::Answer`,
`session_verbs.rs`), an `AnswerQuestion` app-gRPC RPC, hook machinery in the
claude harness (`hook_bridge`/`answers_in_hand`, ADR 0054), and separate
handling in the codex harness. That is the per-tool cost we are eliminating.

Two recent changes make this urgent:

- **Codex is now a second built-in harness** (`de610d22`,
  `crates/engram-harness-codex`). Any tool story that leans on claude-specific
  mechanisms (PreToolUse hooks, stream-json stdin) leaves codex behind.
- The codex harness's question handling shipped with a real gap: its
  outstanding-question map is local to the `drive` loop and keyed by a
  per-process JSON-RPC id, so an `AnswerQuestion` arriving after any app-server
  crash/respawn is **silently dropped** (`engram-harness-codex/src/main.rs`,
  the `questions` map). Same failure class as the AUQ fire-and-forget prod
  incident (2026-06-26). A generic protocol has to fix this class, not
  reproduce it per tool.

Related prior art: ADR 0027 chose CLI-over-MCP for integrations; ADR 0058
designed (and deferred) MCP config delivery for *external* integration servers;
ADR 0054 built the question defer/answer lifecycle this ADR generalizes;
ADR 0073 built the durable outbox the results ride.

**Terms used below.** *MCP* (Model Context Protocol): the standard by which an
agent CLI discovers and calls external tools; supports two transports, HTTP and
*stdio* (the CLI spawns a subprocess and speaks JSON-RPC over its stdin/stdout).
*Dynamic tools*: codex's native mechanism for client-provided tools — declared
at `thread/start`, calls delivered to the supervising process as JSON-RPC
requests. *Defer*: claude's PreToolUse hook verdict that parks a tool call and
ends the turn without executing it. *Outbox*: the coordinator's durable
delivery table (ADR 0073) — rows are commands owed to a session, forwarded
when the session is reachable, surviving restarts and evictions.
*Narrate-past*: a claude-code bug (ADR 0054, upstream #64389) where the model
writes text into a turn that ended on a deferred call, inventing a phantom
tool result that can poison later turns.

## Decision

One protocol with a shared spine and two thin per-harness frontends. A tool is
registered once, in the orchestrator, as data + code. Harnesses expose it to
their model with **zero per-tool harness code** (one bounded exception: native
bindings, §6). Calls travel up the existing session event log; results travel
down the existing durable outbox.

### 1. The registry (orchestrator)

```ts
// zod is the single source of truth: it compiles to the JSON Schema the model
// sees, types the handler/presenters, and validates completion payloads.
tools.register({
  name: "save_memory",
  description: "Save a note to the user's persistent memory store.",
  input: z.object({ text: z.string() }),
  output: z.object({ saved: z.boolean() }),
  handling: "handled", execution: "sync",
  handler: async (ctx, args) => { ... },           // ctx: session, user, capabilities
});

tools.register({
  name: "ask_user_question",
  description: "Ask the user one or more structured questions.",
  input: QuestionsSchema, output: AnswersSchema,
  handling: "session",                              // no global handler
  presenters: { slack: questionEffect, web: "UserQuestionCard" },
});
```

Two handling modes, one execution flag:

- **handled** — a global handler function runs in the orchestrator.
  - `execution: "sync"`: the handler returns the result; round trip is bounded
    by an RPC timeout. For save-to-memory, add-review-comment, create-ticket.
  - `execution: "deferred"`: the handler *starts* something and returns
    nothing; later code calls `tools.complete(sessionId, callId, result)`.
    For wait-for-CI-style tools.
- **session** — no handler exists. The call is routed to whatever is watching
  the session (the Slack thread workflow via the communication policy, the web
  transcript via the event stream), matched **by tool name**, and completed by
  an inbound authz'd RPC. Session-handled tools are deferred by nature (a
  human is in the loop). `ask_user_question` is the first one.

Handlers are global code but every invocation receives session context, so
behavior is per-session without per-session registration. Which tools a session
gets is decided at CreateSession (profile/capabilities), compiled into a
**manifest** (names, descriptions, JSON schemas, execution mode, native-binding
flags) and delivered via `harness_env` — the same channel that already carries
`ENGRAM_CLI_INTEGRATIONS` (`orchestrator/src/rpc/task-create.ts`).

Guardrails, because name-matching is a convention rather than a structure:

- **Build-time**: a registry unit test asserts every `handling: "session"` tool
  has a presenter registered for at least one surface, with an explicit
  `presenterExempt: "<reason>"` escape hatch so opting out is a reviewed
  decision, not an accident.
- **Run-time**: sessions outlive deploys, so a session-handled call nobody has
  acted on within a generous window is flagged (the §2 two-phase events give
  the watchdog its signal).

We deliberately did **not** introduce an interaction-category vocabulary
(`interaction: "question"` etc. — rejected in §Alternatives): the model-facing
schema should be domain-shaped (`approve_deploy({environment, changeSummary})`),
and translating domain args into a card is inherently per-tool logic that
belongs in the presenter, keyed by name, typed by the shared zod schema.

### 2. The wire (harness-proto, coordinator)

Two new frames, appended to the existing bincode enums (indices pinned by
`tests/wire_golden.rs`):

- `HarnessEvent::ToolCallRequested { run_id, call_id, name, args_json }` — up
  via the existing harness vsock channel → host-agent `EventSink` → coordinator,
  which appends event kind `tool_call_requested` to the session event log.
- `HarnessCommand::ToolResult { call_id, result_json }` — down via a new
  `OutboxKind::ToolResult` (sibling of `OutboxKind::Answer`,
  `session_verbs.rs::forward_outbox_row`), so delivery is durable and survives
  orchestrator restarts and session eviction.

Payloads are opaque JSON. The coordinator learns the *shape* "a tool call
exists / a result is owed" — never what any tool does. That is exactly how
much the coordinator knows about questions today, so no layering changes.

**Two-phase completion visibility** — a deliberate improvement over AUQ.
Today the only "answered" signal is emitted by the harness *after it consumes
the answer*; between "user clicked in Slack" and "VM resumed and re-fired"
every surface shows the question as open (the June incident hid a submitted
answer for 35 minutes). The generic protocol appends **two** log events:

- `tool_result_submitted` — written by the coordinator when the completion
  RPC lands (who completed it, when). Surfaces lock their cards on this;
  the per-surface optimistic hacks (`answeredToolCallIds` in
  `question-actions.ts`, Slack idempotency keys) become unnecessary.
- `tool_call_completed` — written when the harness confirms the model
  actually consumed the result.

"Submitted but never consumed" becomes an observable, alertable state in the
log instead of an invisible one.

### 3. Orchestrator plumbing

- **Up**: the session-ingest pump already forwards curated events to the
  session's thread workflow (`session-ingest.ts`); `tool_call_requested` joins
  the curated set. Handled tools dispatch to their handler (authz check → zod
  parse → run). Session-handled tools flow to the communication policy, which
  gains one match arm per tool (`ask_user_question` → the existing question
  effect; `toolCallId → ts` bookkeeping in `slack-thread.ts` survives as-is).
- **Down**: `tools.complete()` (internal) and a new authz'd
  `CompleteToolCall` app-gRPC RPC (external surfaces; today's
  `AnswerQuestion` RPC generalized) both validate the payload against the
  tool's zod output schema, append `tool_result_submitted`, and enqueue the
  outbox row.
- **Latency caveat (sync tools)**: the ingest pump is a poll
  (`ListSessionEvents`); a sync tool's round trip includes that poll interval.
  If measurement shows this blows the ~sub-second target, the orchestrator
  holds a live `StreamEvents` subscription per active session for tool events.
  Decide from numbers in P1, not up front.

### 4. Claude frontend

The claude CLI has exactly one headless surface for *adding* tools: MCP. We use
stdio, not HTTP:

- The harness (which already generates `claude-settings.json`) writes an
  MCP config declaring one stdio server: **its own binary** in a bridge mode
  (`engram-harness-claude mcp-bridge`), mirroring the existing `hook-bridge`
  self-invocation. `build_claude_argv` adds `--mcp-config <file>`
  **and `--strict-mcp-config`** (spike-proven necessary: without it the user's
  configured MCP servers all load inside the guest).
- The bridge answers MCP `initialize`/`tools/list` locally from the manifest —
  no network — and forwards `tools/call` to the harness main process over a
  unix socket. In-guest layer is rebundle-only; no image rebake.
- **Sync tools**: the bridge holds the MCP call open until `ToolResult`
  arrives.
- **Deferred tools**: the PreToolUse hook (which already sees every call)
  matches the `mcp__engrams__` name prefix against the manifest and returns
  `defer` — turn ends, VM evictable. Delivery reuses the AUQ machinery:
  result-in-hand + `--resume` re-fire (same `tool_use_id`), hook allows, the
  bridge serves the stashed result. The parked-call stash lives in the harness
  main process (the bridge subprocess dies with each CLI respawn).
- Spike-derived requirements (see §Evidence): the hook must **not** defer
  `ToolSearch` (MCP tools are lazily loaded — the model calls ToolSearch
  first, and the hook fires for it); the narrate-past transcript scrub built
  for AskUserQuestion **must be broadened to all deferred tools**
  (narrate-past reproduced on 2 of 3 MCP defer runs); don't assume
  `system/init` arrives before the re-fired `tool_result` on resume.

Why not remote HTTP MCP: it puts a bearer token in guest env (the exact thing
ADR 0064 removed), permanently holes the egress allowlist for the orchestrator
host, and adds NAT/dev-parity friction. The stdio bridge rides the existing
attach-token-authenticated vsock channel; the guest holds no credential and
needs no egress change.

### 5. Codex frontend

No MCP server, no bridge process. Codex has a native client-tool mechanism:

- The harness declares manifest tools as `dynamicTools` on `thread/start`
  (also accepted on `thread/resume`). **Gate**: `initialize` must send
  `capabilities.experimentalApi: true`; the merged harness sends `false`, and
  with `false` the declaration fails loudly
  (`"thread/start.dynamicTools requires experimentalApi capability"`) — a
  one-line flip, with no silent-degradation risk.
- A model call arrives as the server→client JSON-RPC request `item/tool/call`
  `{callId, tool, arguments, threadId, turnId}`; the harness replies
  `{success, contentItems: [{type: "inputText", text}]}` whenever it likes —
  the await inside codex has **no timeout** (verified: an 8s park; source
  shows an untimed oneshot).
- **Sync tools**: respond when `ToolResult` lands. This is exactly the shape
  of the existing `requestUserInput` handling.
- **Deferred tools**: *don't respond yet.* The parked await, the open request,
  and the harness's correlation state all live inside the guest, so a
  whole-VM snapshot captures them by construction; after restore the harness
  responds to the request it froze with. No re-fire, no stash-and-graft —
  simpler than claude.
- **Eviction policy change required**: a parked codex turn never emits
  `turn/completed`, so the session never looks idle and would never evict.
  The harness emits an explicit *parked* signal ("all outstanding work is
  deferred tool calls — treat as idle") and the coordinator's idle-eviction
  machinery (ADR 0034) accepts it. Exact form (reuse `Idle` vs a new
  `Parked` event) is decided in P4; a new event is likelier since resume must
  distinguish "idle, start next prompt" from "parked, do not".
- **Crash degrade** (codex process dies while parked — distinct from
  eviction): on `thread/resume`, codex force-closes the dangling call with a
  synthetic `"aborted"` output (stable call_id) and the model knows it.
  The harness then delivers the late result as a follow-up user message
  ("the earlier `save_memory` call completed; result: …"). Model-visible
  seam, semantically sound — and for questions, an answer *is* user speech.
  The unstable `thread/resume { history }` override ("FOR CODEX CLOUD — DO
  NOT USE") could inject a faithful `FunctionCallOutput` instead; we note it
  and don't build on it.
- This also fixes the merged harness's answer-drop gap: correlation state
  moves out of the drive loop into the harness's durable parked-call table
  (shared logic in `engram-harness-sdk`), and results arriving for unknown
  call_ids are surfaced loudly, never dropped.

### 6. Native bindings — where AskUserQuestion and requestUserInput go

Both agents ship a built-in question mechanism their models are trained on.
Fighting that training (disallow the built-in, inject a clone) is worse than
adapting to it. So a registry tool may be **natively bound** per harness:
provided not by injection but by intercepting the agent's built-in and
translating at the boundary.

`ask_user_question` is bound on both harnesses:

- **Claude**: the manifest marks it natively bound → it is *omitted from the
  MCP config* (no duplicate question tools). The hook's binding table maps
  built-in `AskUserQuestion` → deferred `ask_user_question`; args translate
  to the canonical zod shape (this adapter already exists — it's today's
  question-normalization code, repointed at the generic frame). Delivery
  translates back and rides the re-fire path.
- **Codex**: `item/tool/requestUserInput` (a held-open request) maps to the
  same `ToolCallRequested{name: "ask_user_question"}`; today's
  `parse_questions`/`ids_by_text` code becomes the adapter; delivery responds
  to the frozen request (or crash-degrades).

Upstream of the harness the two are indistinguishable — same event, same
Slack card, same `CompleteToolCall`. A future harness with no native question
tool gets `ask_user_question` by plain injection and the entire upstream stack
works unchanged.

**Honest caveat**: native bindings *are* per-tool harness code — arg/result
translation is written in each agent's private vocabulary and can live nowhere
else. This doesn't break the "zero per-tool harness code" promise in practice
because bindings only exist for tools aliasing something the agent already
ships — a tiny, slow-moving set (questions today) — and every tool we invent
takes the injection path. A binding is only worth writing when the model's
training on its built-in beats a schema description; that is rare by
construction.

### 7. What gets retired (clean break)

`HarnessEvent::UserQuestion`/`QuestionAnswered`, `HarnessCommand::AnswerQuestion`,
`OutboxKind::Answer`, the `AnswerQuestion` app-gRPC RPC, and the harness-side
question special-casing all collapse into the generic frames. The Slack
workflow's structure, idempotency keys, and `toolCallId` bookkeeping survive
with renamed inputs — AUQ was already shaped like a session-handled tool; the
protocol is that shape, generalized.

Migration note: the session event log is historical data. Readers
(web `buildMessages.ts`, the ingest curator) keep rendering the legacy
`user_question`/`question_answered` kinds for pre-migration sessions; new
sessions emit only the generic kinds. The old *write* paths are deleted, not
shimmed.

### 8. Security model

The guest is untrusted — a tool call is an inbound API request from a sandbox
running arbitrary model output, not internal traffic. Accordingly:

- Every handled-tool dispatch authz-checks the session's `capabilities`
  (already on `CreateSessionRequest`) before the handler runs.
- `CompleteToolCall` requires a principal with access to the session (same
  gate as today's `AnswerQuestion`), and only session-handled calls may be
  completed externally — handled tools complete exclusively from orchestrator
  code.
- zod validation runs in both directions: args before any handler/presenter,
  results before anything is sent back to the model.
- No credentials enter the guest for any of this: both frontends ride the
  existing session-bound, attach-token-authenticated channels (ADR 0073).

## End-to-end scenarios

### A. Sync handled tool, claude session

1. Session starts. `compileSessionCreateInput` put the manifest in
   `harness_env`; the harness wrote `mcp-config.json` + manifest next to
   `claude-settings.json` and spawned claude with
   `--mcp-config … --strict-mcp-config`.
2. Model (after a `ToolSearch` load — hook allows it) emits
   `tool_use name=mcp__engrams__save_memory`. Hook: name is in the manifest,
   execution=sync → `allow`. Claude calls the bridge over stdio.
3. Bridge → harness main over the unix socket; harness emits
   `ToolCallRequested{call_id=toolu_…, name="save_memory", args_json}` up
   vsock; coordinator appends `tool_call_requested` (durable from this
   instant).
4. Orchestrator ingest sees it → capability check → `input.parse(args)` →
   handler runs (~50ms) → `tools.complete()` → `tool_result_submitted`
   appended → outbox row.
5. `forward_outbox_row` dispatches `ToolResult` down → harness → bridge
   answers the still-open MCP call; model continues. Harness confirms →
   `tool_call_completed`. Sub-second total; the transcript shows a normal
   tool call (the CLI's own `tool_use` events populate
   `ToolCallStarted/Completed` as usual).
6. Orchestrator restarting at step 4 delays, never loses: the event is in
   the log; the outbox redelivers.

### B. Session-handled question, codex session, answered hours later

1. Codex's model asks a question natively; the harness receives
   `item/tool/requestUserInput` (a JSON-RPC request it must eventually
   answer) and holds it.
2. Native binding: harness emits
   `ToolCallRequested{call_id=<itemId>, name="ask_user_question", args=<canonical>}`
   and signals *parked* (all outstanding work is deferred calls).
3. Policy arm for `ask_user_question` → question effect → Slack card posted;
   web renders the same call from the event stream. Coordinator sees
   *parked* → idle-eviction proceeds → VM snapshotted. The frozen memory
   contains codex's open await and the harness's correlation entry.
4. Hours later: user answers in Slack → thread workflow calls
   `CompleteToolCall` → `output.parse(answers)` → `tool_result_submitted`
   appended — **web card locks now**, not after resume → outbox row → session
   wake.
5. VM restores; harness reattaches (ADR 0073), receives `ToolResult`,
   translates canonical answers to codex's `{questionId: {answers}}`, and
   responds to the request it froze with. Turn continues as if no time
   passed. Harness emits confirmation → `tool_call_completed`.
6. Crash variant: if the codex *process* died instead, `thread/resume`
   closed the call as `"aborted"`; the harness injects the answer as a user
   message. Validated behavior; see Evidence.

### C. Deferred tool, claude session, evicted mid-wait

1. Model calls a deferred tool; hook returns `defer`; CLI stops with
   `stop_reason: "tool_deferred"`; harness emits `ToolCallRequested`; turn is
   over; session idles and evicts normally (no policy change needed on
   claude — defer ends the turn).
2. Result arrives → outbox → wake → restore. If the CLI process survived in
   the snapshot, the harness injects a stream-json `tool_result` on stdin
   (validated). If the CLI was respawned, `--resume` re-fires the pending
   call automatically with the **same** `tool_use_id` (validated); the hook
   sees result-in-hand → `allow`; the bridge serves the stashed result.
3. The narrate-past scrub (ADR 0054, broadened) sanitizes any hallucinated
   "tool result missing" text from the deferred turn before resume.

## Evidence (spikes, 2026-07-12/13)

**Codex 0.144.1 — the exact version pinned in `deploy/harness-codex/stage.sh`
(local binary, real auth, `codex app-server` + JSON-RPC driver):**

- Dynamic tools work end to end; the call arrived as
  `item/tool/call {"callId":"exec-2033078c…","tool":"save_memory","arguments":{"text":"spike hello"}}`;
  response `{success: true, contentItems: [{type:"inputText", text}]}` was
  consumed verbatim by the model after an **8s park**; turn completed.
- Gate is loud: with `experimentalApi: false` →
  `{"error":{"code":-32600,"message":"thread/start.dynamicTools requires experimentalApi capability"}}`.
- Crash path: SIGKILL mid-park → respawn + `thread/resume` (which also
  accepted a `dynamicTools` re-declaration) → pending call did **not**
  re-fire; pre-crash turn `status: "interrupted"`; model's own account:
  *"No; the save_memory tool call was aborted."* — the user-message degrade
  path has the model in the right mental state.

**Claude CLI 2.1.207 (haiku; verdict JSON copied from the harness source):**

- `defer` on `mcp__engrams__save_note`: parked **before** reaching the MCP
  server (server log shows no `tools/call`), `stop_reason:"tool_deferred"`,
  process alive.
- stdin injection of a `tool_result` for the parked `tool_use_id` resumed the
  turn; the model echoed the injected token exactly.
- SIGKILL + `--resume`: the pending call re-fired automatically with zero
  stdin input, **same** `tool_use_id`
  (`toolu_01Efm9u9kRaDoELsiqWGaMaX` before kill and after resume); hook
  `allow` let it reach the MCP server and complete.
- Caveats found: narrate-past reproduced on 2/3 defer runs (model wrote
  ``The tool returned: `[Tool result missing due to internal error]` `` into
  the deferred turn and sometimes parroted it post-resume) → scrub must cover
  all deferred tools; `--strict-mcp-config` required; MCP tools lazy-load via
  `ToolSearch` (hook fires for it — filter by `mcp__engrams__` prefix, never
  defer ToolSearch); re-fired `tool_result` can precede `system/init` on
  stdout; bonus: MCP `tools/call` carries `_meta["claudecode/toolUseId"]` for
  native correlation.

**Not yet validated**: park → *VM snapshot/restore* → respond on codex (the
process-alive and crash halves are proven; the snapshot half needs a VZ
session with a wired harness — P3 exit gate); the parked-eviction policy
interaction (P4).

## Alternatives considered

- **Remote HTTP MCP (orchestrator-hosted endpoint)** — rejected: bearer token
  in guest env (anti-ADR-0064), permanent egress hole, NAT/dev friction. The
  stdio bridge gets the same MCP surface with no credential in the guest.
- **Synchronous vsock side-channel** (a one-request/one-response channel
  modeled on the forge credential bridge) — collapses into the event-log
  design anyway: the coordinator cannot call the orchestrator (the dependency
  points the other way), so pending calls must be parked in Postgres and
  picked up by the orchestrator — which is what the event log already is.
- **Event-log ride with the call held open for deferred tools** — durable but
  pins the VM for the whole wait; all the plumbing, none of the eviction win.
- **Tools as generated in-guest CLI binaries (ADR 0027 style)** — the only
  fully harness-agnostic option (no MCP, no hooks) and kept in the back
  pocket for a future harness with neither MCP nor dynamic tools; costs
  model-side schema enforcement, soft prompt-based discovery, and Bash-shaped
  transcripts.
- **`interaction: "question"` category vocabulary** — rejected: it forces UI
  shapes into model-facing schemas (the args-translation problem);
  per-tool presenters keyed by name, typed by shared zod, are the honest
  design.
- **Hook `deny` with the result embedded in the denial message** — works,
  horrifying; noted for completeness.

## Risks and open questions

1. **`dynamicTools` is experimental in codex.** Mitigation: the version is
   pinned and checksum-verified (`stage.sh`), and
   `check-app-server-schema.sh` gains assertions for
   `DynamicToolCallParams`/`DynamicToolCallResponse` + the initialize gate,
   so a rebake against a release that moved the API fails at bake time, not
   in a session.
2. **Snapshot-park on codex is unproven across a real evict/restore.** P3's
   exit gate. Fallback if it fails: codex deferred tools use the crash-degrade
   path universally (interrupt the turn at evict time, inject the result as a
   user message on resume) — worse transcripts, same correctness.
3. **Sync-tool latency through the ingest poll.** Measure in P1; escalate to
   a live `StreamEvents` subscription only if needed.
4. **Narrate-past scrub broadening** touches the most delicate code in the
   claude harness (ADR 0054 Part C). It is `cfg(target_os = "linux")` —
   compile-check via `--target aarch64-unknown-linux-musl`, run via
   `just test-linux`.
5. **Tool set is fixed at session start** (refreshed on resume). Mid-session
   registry changes don't propagate to live sessions; acceptable v1 scope.
6. **Manifest size** rides `harness_env` (a `map<string,string>` in
   CreateSession); fine for tens of tools, revisit (file/bundle delivery) if
   schemas balloon.

## Plan

- **P1 — spine.** Wire frames + golden tests; coordinator event kinds
  (`tool_call_requested` / `tool_result_submitted` / `tool_call_completed`) +
  `OutboxKind::ToolResult`; orchestrator registry (zod), manifest compilation
  into `harness_env`, handled-tool dispatch, `CompleteToolCall` RPC, the
  presenter-coverage test; latency measurement.

  *As built (2026-07-13):* landed test-first, red→green. Divergences and
  notes: (1) applied migration 0084's `CHECK (kind IN ('prompt','answer'))`
  required the new migration `0105_outbox_tool_result_kind.sql`; (2)
  `tool_call_completed` joined the curated ingest set too, so the
  orchestrator's `pending_tool_calls` ledger (new drizzle table — powers the
  §8 session-handled-only external gate and the §1 watchdog) can stamp
  consumption; (3) the ToolResult outbox row is retired by the existing
  `ToolCallCompleted` harness event (`tool_result:<call_id>` ack id); (4)
  handled-tool failures complete with the protocol error envelope
  `{"error":"<message>"}` — the one exception to a tool's output schema; (5)
  unknown tool names in the ledger default to `handling:"handled"` (fail
  closed for external completion), which also means a call naming a tool
  since deleted from the registry is never completed — acceptable while
  manifest and registry ship together, revisit if registry becomes dynamic;
  (6) both harnesses carry a loud-warn placeholder arm for
  `HarnessCommand::ToolResult` until their P2/P3 frontends land; (7) sync
  latency measurement deferred to the P2 live smoke (needs a real session).
- **P2 — claude frontend.** `mcp-bridge` subcommand + config generation +
  `--strict-mcp-config`; hook manifest matching (prefix filter, ToolSearch
  exemption); deferred delivery via the AUQ machinery; scrub broadening.
  Rebundle-only. Exit gate: scenario A + C live on a VZ session.

  *As built (2026-07-13):* landed as designed (ToolSearch/built-ins
  short-circuit in the hook client — only AUQ + `mcp__engrams__` names
  round-trip the socket), plus three live-smoke-driven fixes: (1) the
  narrate-past double-fire (#64389) reproduced for manifest deferred tools —
  `duplicate_auq_ids` generalized to `duplicate_deferred_ids` so a same-turn
  duplicate defers without a second `ToolCallRequested` and is scrubbed;
  (2) **idle eviction drains the harness before the snapshot** ("claude-free"
  snapshots), so resume always presents a *fresh* harness — the
  "CLI survived in the snapshot" fast path never occurs post-eviction, and an
  unknown `ToolResult` now mirrors AnswerQuestion (stash → SIGINT →
  `ResumeForDeferred` → id-stable re-fire served from stash); (3) dispatch
  had been wired into the Slack-only ingest pump — web tasks had no pump at
  all — so bookkeeping/dispatch moved to a per-session
  `toolDispatchWorkflow` started from `createTaskWithSession`, the funnel all
  creation surfaces share. Scenario A measured sub-4s submit→reply through
  the 1s ingest poll — the §3 latency escalation (live `StreamEvents`) is
  not needed.
- **P3 — codex frontend.** `experimentalApi: true`; `dynamicTools`
  declaration on start/resume; parked-call table in `engram-harness-sdk`
  (fixes the answer-drop gap); sync + deferred + crash degrade; schema-check
  additions. Exit gate: scenario A analog live, **and the snapshot-park
  spike** (park → evict → restore → respond, same callId).

  *As built (2026-07-13):* implemented against a fake-codex stdio seam
  (shell script speaking the app-server newline-JSON protocol, mirroring the
  claude crate's fake-CLI pattern) — 22 red→green tests including the
  answer-drop regression (post-respawn answer delivered as a follow-up user
  message, unknown ids logged loudly) and the crash degrade.
  `check-app-server-schema.sh` pins `DynamicToolCallParams`/`Response` + the
  `experimentalApi` capability against the pinned 0.144.1 binary. NOTE: the
  P2 finding that idle eviction produces harness-free snapshots applies here
  too — the §5 "whole-VM snapshot captures the parked await" assumption does
  not hold for *idle-evicted* sessions; the durable parked-call table +
  crash-degrade path (already built) are the actual delivery mechanism after
  eviction. Exit gates (scenario A analog + snapshot-park) still open: the
  dev stack has no codex profile yet.
- **P4 — parked eviction.** The parked signal (`Parked` event vs `Idle`
  reuse), ADR 0034 state-machine integration, wake-on-result.

  *As built (2026-07-14):* added the trailing `Parked` wire variant (kind
  `harness_parked`). Codex emits it for deferred dynamic tools and
  `requestUserInput`; claude emits it when a turn ends `tool_deferred`. The
  coordinator classifies it like `harness_idle` for the soft TTL, and the ADR
  0091 resume-rewind excludes it for the same reason. Wake-on-result is
  unchanged (`ToolResult` outbox → `Resume`); after idle eviction, codex uses
  the existing crash-degrade delivery path because snapshots are harness-free.
- **P5 — AUQ convergence.** Native bindings both harnesses; policy/web/Slack
  switch to generic kinds (legacy kinds still render for old sessions);
  delete the bespoke question wire types, RPC, and outbox kind.
- **P6 — first real tools.** `save_memory`, `add_review_comment`; flip this
  ADR to Accepted with the commit chain and as-built divergences.
