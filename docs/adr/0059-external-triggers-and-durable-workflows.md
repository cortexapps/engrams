# ADR 0059: External triggers and durable background-agent workflows (Slack-first)

Status: 2026-06-24 — **Proposed.** Substrate simplified after an adversarial design
review (see §Design review): the reverse channel is DBOS-native (2 workflows, zero
new tables), not the 5-table pump the exploration note sketched.

Builds on ADR 0051 (the TypeScript orchestration tier — tasks, auth, the
control-plane boundary), ADR 0052 (the persistent streaming Claude harness +
session event log), ADR 0053/0056/0057 (profiles as the unified session-policy
object + generic integrations), ADR 0054 (harness special messages — the
`user_question`/`question_answered` events + the idempotent `AnswerQuestion`
RPC), ADR 0034 (idle-eviction + resume), ADR 0013 (stateless dial-on-demand
transport), and ADR 0007 (chunked-immutable storage under the session event
log). Supersedes the exploratory note
`docs/post-adr0039-background-agent-workflows.md` (written pre-renumber, when the
TS tier was "ADR 0039"); this ADR keeps that note's *boundary principle* and the
two-direction reverse-channel idea, but **deliberately rejects its 5-table pump
substrate** in favor of DBOS-native constructs now that DBOS is a hard dependency
(rationale in §Design review).

## Context

engrams can create a **task** today only one way: a human picks a profile in the
web UI and `TaskService.CreateTask` boots a `chat` session
(`orchestrator/src/rpc/tasks.ts`). There is no way for an **external or internal
system** to kick off work — no Slack `@mention`, no Linear/Jira ticket, no cron.
Every comparable software factory (Ona/Gitpod background agents, Factory Droids,
Fabro, Warp, Ramp's internal agents) treats third-party triggers + a reverse
communication channel as table stakes.

The architecture already anticipates this. `task` carries a `type` field (only
`chat` today, with `linear_issue`/`incident`/`dependabot` named as planned), a
`source` jsonb, and a `workflow_run_id` column **explicitly reserved for a DBOS
run id** (`orchestrator/src/db/schema.ts`). ADR 0051 §4 sketches exactly this
flow: webhook → verify → insert `task` → start a durable workflow →
`CreateSession` → consume the session event stream → publish results back. DBOS
is **not yet a dependency**; adopting it is net-new.

This ADR specifies the **durable trigger framework** and wires **Slack** as the
first end-to-end source. Linear, Jira, and cron are designed as adapters that
slot into the same framework but are **out of scope** here.

## Goals / non-goals

**Goals**

- A durable, restart-safe path from an external event to a running session and
  back, built on **DBOS** workflows embedded in the orchestrator tier.
- **Slack** end-to-end: `@mention` → two-stage reaction ack → run a session →
  mirror `AskUserQuestion` into the thread (answerable from Slack *or* the web
  UI) → post PRs/artifacts/media → post a closing summary; clear ❌ + message on
  failure. Follow-up `@mention`s feed fresh thread context into the live session.
- A generic, optional **`append_system_prompt`** on session create, mapped per
  harness (Claude → `--append-system-prompt`), configured **per trigger source**.
- A reverse-channel design that is **minimal for v1** yet additively extensible to
  the future fanout cases (§Future).

**Non-goals (this ADR)**

- Slack OAuth app registration / bot-token storage — **handled separately**; this
  ADR consumes the bot token + per-workspace trigger config that layer provides,
  and **requires it to grant the scopes in §Slack scopes**.
- An identity-linking subsystem — replaced by a single email-match seam.
- Linear / Jira / cron adapters — designed-for, not built.
- Branch fanout, CI repair swarms, multi-agent workrooms, fork/checkpoint
  primitives. The substrate is forward-compatible (§Future) but they are separate
  work.
- Producing screenshots/video — the session **already** emits `file_shared`; we
  only mirror it.

## Decisions

1. **Reverse channel is DBOS-native, not a bespoke pump.** The coordinator's
   append-only session event log is *already* the durable, ordered, replayable
   inbox. A per-session `SessionIngestWorkflow` tails it in bounded reads and
   `DBOS.send`s curated events into the thread workflow's mailbox; the thread
   workflow drains via a single `DBOS.recv`. Cursors and posted-message refs are
   workflow-local state; **no new tables** — replay dedupe and provider refs ride
   DBOS step checkpoints.
2. **No separate ingest/router workflow.** The HTTP handler does an idempotent
   `startWorkflow` + an idempotent `DBOS.send` for *every* mention; Slack's
   retry-until-2xx provides at-least-once delivery; the thread workflow treats its
   first `recv` as the initial mention. (See §Design review for why this is safe
   without a durable router.)
3. **DBOS embedded in the Bun orchestrator**, un-bundled, on a `dbos` schema in
   the orchestrator's existing Postgres.
4. **Identity = email match**, behind one seam `resolveEngramsUser(provider,
   externalUserId)`. Unlinked → ❌ + "log in first" message, don't start.
5. **`@mention` is the "act now" signal.** Each mention re-pulls new thread
   context (`conversations.replies` since `last_processed_ts`); the first starts
   the session, later ones `SendPrompt` (auto-resuming an idle VM) with a
   deterministic `prompt_id = slack:<event_id>` (Decision 9).
6. **Two-stage reaction ack + failure react:** 👀 on the mention when picked up →
   ✅ when the session starts → ❌ + an actionable message on any failure. Plus one
   link message at session start.
7. **`onComplete` always posts a closing summary** + marks the task done.
8. **`append_system_prompt`** is a new optional `CreateSessionRequest` field (#11),
   delivered to the harness and mapped per-CLI, configured per trigger source.
9. **Prompt delivery is idempotent via a deterministic `prompt_id` — no coordinator
   change.** A DBOS step replay can re-issue `SendPrompt` (steps are at-least-once
   across the service boundary). This is already covered downstream: the host→guest
   command channel is *itself* at-least-once and the harness already dedupes prompts
   by `prompt_id` (ADR 0052, `seen_prompt_ids` — survives a claude respawn *and* an
   FC snapshot/restore). So the workflow simply mints a **deterministic** id —
   `slack:<event_id>` — and a replayed `SendPrompt` re-sends the same id, which the
   harness swallows. The only uncovered window is a replay landing *after* a cold
   guest boot cleared the set; closeable later with additive coordinator dedup on
   `(session_id, prompt_id)`, **not built now**. Session-creation replay is ignored
   on purpose: a stray VM idle-evicts cheaply, so create needs no idempotency key.

## The boundary principle

Retained from the exploration note as the load-bearing invariant:

> **Sessions propose. Orchestrator decides. Control plane forks.**

The agent inside the guest never knows about Slack and never receives
control-plane credentials. It emits ordinary session events (an `AskUserQuestion`
tool call → `user_question`; a `gh pr create` observed at the egress proxy →
`integration_asset`; a shared file → `file_shared`). The **orchestrator** is the
only thing that maps those to outbound side effects, after policy/ownership
checks. This keeps the agent harness-generic and the coordinator
task/user/integration-blind (ADR 0051).

## Architecture overview

```
Slack ──HTTP──▶ orchestrator-api
 (events +        │  startWorkflow(SlackThreadWorkflow)  +  DBOS.send(mention, idem=event_id)
  interactivity)  ▼   (both idempotent; ack 200 only after both commit)
            DBOS workflows (embedded, Bun)
             • SlackThreadWorkflow  (per thread)
                 │   ▲   single DBOS.recv multiplexes { session events ∪ slack mentions }
   CreateSession │   │ DBOS.send(curated session events)
   SendPrompt    │   └──── • SessionIngestWorkflow (per session): bounded tail of the log
   AnswerQuestion▼
            Rust control plane
             • CreateSession · SendPrompt · AnswerQuestion
             • StreamEvents (low-latency tail) · ListSessionEvents (NEW unary, catch-up)
             • append-only session event log  ← the durable, replayable inbox

 No new orchestrator tables — cursors, posted-message refs (the question `ts`),
 and outbound replay-dedupe all ride DBOS step checkpoints + workflow-local state.
```

This introduces an **orchestrator-worker** role (DBOS executor + Slack sender)
distinct from **orchestrator-api** (public API + webhooks). They may be
one Bun process initially or two deployables; both share the orchestrator
Postgres.

## The workflows

**HTTP handler** — no Slack API calls, so trivially under the 3s ack budget.
Ordering matters: ack 200 **only after** both durable writes commit; a crash
before the ack is covered by Slack's retry (≤3: immediate, +1m, +5m).
```
POST /slack/events:
  verifyHmac(rawBody, headers)                          // v0 HMAC, reject >5min stale, constant-time
  if url_verification: return challenge
  evt = parse(body)
  threadWfId = `task:${hash(evt.team_id, evt.channel, evt.thread_ts ?? evt.ts)}`
  await DBOS.startWorkflow(SlackThreadWorkflow, {workflowID: threadWfId})    // idempotent: 1st mention creates
  await DBOS.send(threadWfId, {team,channel,threadRoot,user,ts,eventId}, "slack",
                  {idempotencyKey: evt.event_id})                            // EVERY mention; dedup'd
  return 200
```

**`SlackThreadWorkflow`** (per thread) — the brain; one `recv` waits on both
sources; reactions are the ack:
```
SlackThreadWorkflow():
  m = await DBOS.recv("slack")                                   // first @mention = initial trigger
  await step(() => reactAdd(m.channel, m.ts, "eyes"))            // 👀 picked up (very top)
  user = await step(() => resolveEngramsUser("slack", m.user))
  if !user: { await fail(m, `You don't have a user in engrams — go to ${ENGRAMS_URL} to log in first.`); return }
  cfg     = await step(() => getTriggerConfig("slack", m.team))   // default profile + system_prompt_append
  prompt  = await step(() => gatherThreadContext(m.channel, m.threadRoot, oldest=null))   // full thread
  try:    session = await step(() => createSession({profile: cfg.default_profile_id,
                                     append_system_prompt: cfg.system_prompt_append, prompt}))
  catch:  { await fail(m, "Couldn't start a session for this request."); return }
  await step(() => reactAdd(m.channel, m.ts, "white_check_mark")) // ✅ session started
  await step(() => persistTask({type:"slack_thread", source, owner:user, workflow_run_id:self}))
  await step(() => postSessionLink(m, session.webUrl))           // one link message, with the ✅
  let lastTs = m.ts
  let questionTs = new Map()                                     // tool_call_id → posted question ts (for chat.update)
  await DBOS.startWorkflow(SessionIngestWorkflow,
        {workflowID:`ingest:${session.id}`, input:{sessionId:session.id, threadWfId:self}})

  while true:
    msg = await DBOS.recv(["session","slack"], TIMEOUT)
    if msg.topic == "session":
      if msg.event.kind == "terminal":                          // terminal status_changed ONLY
        if msg.event.ok: await step(() => onComplete(m, accumulated))   // always posts closing summary
        else:            await fail(m, `The session ended in failure. <web link>`)
        return
      await step(() => dispatch(msg.event))                     // onUserQuestion / onAsset; idempotent
    if msg.topic == "slack":                                    // follow-up @mention
      ctx = await step(() => gatherThreadContext(m.channel, m.threadRoot, oldest=lastTs))   // only NEW msgs
      await step(() => sendPrompt(session.id, ctx, promptId=deterministic(msg.eventId)))  // see Open Q1
      lastTs = ctx.maxTs

async fail(m, message):
  await step(() => reactAdd(m.channel, m.ts, "x"))              // ❌  (already_reacted = success)
  await step(() => postMessage(m.channel, m.threadRoot, message))   // DBOS step (checkpointed); actionable
```

**`SessionIngestWorkflow`** (per session) — the pump replacement: bounded reads,
no lease (its `workflowID` is the one-pump-per-session guarantee):
```
SessionIngestWorkflow({sessionId, threadWfId}):
  after = 0
  while true:
    {events, nextAfter, terminal} =
        await step(() => readSessionEventsBounded(sessionId, after))   // ListSessionEvents catch-up,
                                                                       // then bounded StreamEvents tail
    for ev in events where curated(ev.kind):
      await DBOS.send(threadWfId, {event: ev}, "session")              // checkpointed → once-only on replay
    after = nextAfter
    if terminal: { await DBOS.send(threadWfId, {event:{kind:"terminal", ok}}, "session"); return }
```

`after` and `lastTs` are **workflow-local variables**, reconstructed
deterministically on replay from checkpointed step outputs — that *is* the cursor,
with no table. Curated kinds: `user_question`, `question_answered`,
`integration_asset`, `file_shared`, terminal `status_changed`,
`run_started`/`run_completed`.

**`gatherThreadContext` cursor.** A Slack message's `ts` *is* its identifier
(there is no separate message id; `thread_ts` is itself a `ts`). So we use it as a
**server-side cursor**, not a client-side filter:
`conversations.replies(channel, ts=thread_root, oldest=lastTs, inclusive=false,
cursor=…)` returns only messages after `lastTs` (paginate via
`response_metadata.next_cursor`); we advance `lastTs` to the newest `ts` returned.
The cursor is workflow-local state during a run (so replay can't skew it); it is
persisted to `task.source` only on terminal, so a successor workflow on a dormant
thread (the thread-reuse epoch case) knows where to resume.

## Communication policy (the per-source seam)

"Where does this go" is **code**, not config. Each source implements:
```
onAck(m, session)           // Slack: 👀→✅ reactions + link · Linear: ticket comment
onUserQuestion(q)           // Slack: Block Kit · Linear: comment + web link
onAsset(asset)              // integration_asset(pull_request) | file_shared(media)
onComplete(summary)         // always posts (Decision 7)
onFail(reason, message)     // ❌ + actionable message
mapInbound(sourceEvent)     // → follow-up SendPrompt | question answer
```
The framework owns event classification, identity, session create/resume, the
recv/drain loop, and outbound-effect replay-dedupe (DBOS step checkpoints). The
adapter owns only the provider mechanics.

## Session-event rendering (Slack)

How each curated event becomes a thread effect (each a checkpointed DBOS step):

| Event (real kind) | Slack rendering (proper API) |
| --- | --- |
| `integration_asset` (asset_kind=`pull_request`) | `section` block (PR title · repo · #num) + a **"View PR"** url `button` |
| `file_shared` (image/video — "show your work") | **Upload bytes to Slack** so it renders inline: `files.getUploadURLExternal` → PUT → `files.completeUploadExternal(channel_id, thread_ts)`. Fallback to a web-UI artifact link for large/unsupported media. (needs `files:write`) |
| `user_question` (AskUserQuestion) | **Adaptive Block Kit**, see below |
| `question_answered` | `chat.update` the question message (via the `ts` held in workflow-local state) to a locked "✓ answered: X" |
| terminal `status_changed` | `onComplete` summary (ok) / `onFail` ❌ + message |

**AskUserQuestion → adaptive Block Kit.** The `user_question` event carries
`tool_call_id` + questions (each `header`, `question`, `multiSelect`,
`options[{label, description}]`). Rendering adapts to question shape:

- **Single-select, few options →** an `actions` block of option **buttons** in the
  thread; the click *is* the answer (lowest friction). `block_actions` →
  `actions[].value` (the chosen label).
- **Multi-select, many options, or free-text "Other" →** an **"Answer"** button
  that opens a **`views.open` modal** (`trigger_id`, 3s) with one input block per
  question — `radio_buttons`/`checkboxes`/`static_select` for choices + an optional
  `plain_text_input` for "Other"; `view_submission` returns all answers in
  `view.state.values`.

In both, the interactive payload carries `{task_id, session_id, tool_call_id}` (in
the button `value` / modal `private_metadata`) so the Interactivity endpoint maps
it to the right `AnswerQuestion(session_id, tool_call_id, answers)` call. Answers
are a `StringList` of chosen labels keyed by question text; a custom "Other" string
is passed as its label.

## Bidirectional AskUserQuestion (Slack ↔ web parity)

Reuses the shipped ADR 0054 path, so parity is automatic:
- Agent `AskUserQuestion` → harness defers → `user_question` event (carries
  `tool_call_id`). The ingest workflow forwards it; `onUserQuestion` renders the
  adaptive Block Kit from §Session-event rendering as a checkpointed DBOS step,
  stashing the posted message `ts` in workflow-local state.
- A human answers **in Slack** (interactivity endpoint → `block_actions`) **or in
  the web UI** — both call the same idempotent `AnswerQuestion(session_id,
  tool_call_id, answers: map<string, StringList>)` (labels keyed by question text;
  ADR 0054 = at-least-once + no-op on duplicate).
- The `question_answered` event flows to the browser SSE **and** the ingest
  workflow; `onUserQuestion`'s message is `chat.update`d to the locked answer
  (using the `ts` from workflow-local state). One source of truth ⇒ no divergence.

## Worked example: `dispatch` + interactivity (AskUserQuestion + `file_shared`)

The concrete handlers. Every outbound call is wrapped in a DBOS `step()`, so a
completed post returns its checkpoint on replay instead of re-firing. The one piece
of cross-event state — the question message's `ts`, needed later for `chat.update` —
lives in a workflow-local `questionTs` Map (`tool_call_id → ts`), reconstructed
deterministically on replay and carried across `continue-as-new`.

**`dispatch(event)` — runs per curated session event inside `SlackThreadWorkflow`:**
```js
async function dispatch(m, event):                       // m = {channel, threadRoot, taskId, sessionId}
  switch event.kind:

  case "user_question":                                  // AskUserQuestion (ADR 0054)
    meta = { task_id:m.taskId, session_id:m.sessionId, tool_call_id:event.tool_call_id }
    ts = await step(() =>                                 // checkpointed: replay returns this ts, no re-post
      simpleSingleSelect(event.questions)
        ? chat.postMessage(m.channel, m.threadRoot, blocks=[   // inline option buttons (click = answer)
            section(event.questions[0].question),
            actions(event.questions[0].options.map(o =>
              button(text=o.label, action_id="auq_pick", value=json({...meta, qIdx:0, label:o.label})))) ])
        : chat.postMessage(m.channel, m.threadRoot, blocks=[   // multi-select / many / Other → "Answer" → modal
            section(summarize(event.questions)),
            actions([ button(text="Answer", action_id="auq_open", value=json(meta)) ]) ]))
    questionTs.set(event.tool_call_id, ts)               // workflow-local; the handle for the later chat.update

  case "file_shared":                                    // media — "show your work" (ADR 0026)
    if event.size_bytes <= MAX_UPLOAD && uploadable(event.media_type):
      await step(() => upload(m.channel, m.threadRoot, event))   // fetchArtifact →
      //   getUploadURLExternal(name,len) → PUT bytes → completeUploadExternal(file, channel, thread_ts, caption)
    else:                                                // too big / unsupported → link
      await step(() => chat.postMessage(m.channel, m.threadRoot,
        `📎 ${event.caption ?? event.media_type} — ${artifactWebUrl(event.artifact_id)}`))

  case "integration_asset" where event.asset_kind == "pull_request":
    await step(() => chat.postMessage(m.channel, m.threadRoot, blocks=[
      section(`*${event.title}*\n${event.repo} #${event.number}`),
      actions([ button(text="View PR", url=event.url) ]) ]))

  case "question_answered":                              // answer landed (from Slack OR web)
    ts = questionTs.get(event.tool_call_id)              // the question post we stashed above
    await step(() => chat.update(m.channel, ts, blocks=[ section(`✓ answered: ${fmt(event.answers)}`) ]))
```

**Interactivity endpoint — a separate Request URL; must ack ≤3s; never calls
`AnswerQuestion` directly (it `DBOS.send`s so a 200-then-crash can't lose the
answer); the only thing it does synchronously is `views.open` (`trigger_id` dies
in 3s):**
```js
POST /slack/interactivity:
  verifyHmac(rawBody, headers); p = parse(payload)
  switch p.type:

  case block_actions, action_id "auq_open":              // open modal NOW (trigger_id 3s)
    meta = JSON.parse(action.value)
    await views.open(p.trigger_id, modal({ private_metadata: json(meta),
      blocks: perQuestionInputBlocks(meta) }))           // radio/checkboxes/static_select + Other plain_text_input
    return 200

  case block_actions, action_id "auq_pick":              // inline single-select: the click IS the answer
    v = JSON.parse(action.value)                          // {task_id, session_id, tool_call_id, qIdx, label}
    await DBOS.send(`task:${v.task_id}`,
      { kind:"answer", tool_call_id:v.tool_call_id, answers:{ [questionText(v.qIdx)]: [v.label] } },
      "slack", { idempotencyKey:`${v.tool_call_id}:${v.label}` })
    return 200

  case view_submission:                                  // modal submitted
    meta    = JSON.parse(p.view.private_metadata)
    answers = readState(p.view.state.values)              // {questionText: [labels…]}, incl. any "Other" text
    await DBOS.send(`task:${meta.task_id}`,
      { kind:"answer", tool_call_id:meta.tool_call_id, answers },
      "slack", { idempotencyKey:`${meta.tool_call_id}:submit` })
    return 200                                            // empty body closes the modal
```

**The answer is just another inbound message in the recv loop:**
```js
  while true:
    msg = await DBOS.recv(["session","slack"], TIMEOUT)
    if msg.topic == "slack" && msg.kind == "answer":
      await step(() => answerQuestion(m.sessionId, msg.tool_call_id, msg.answers))   // idempotent (ADR 0054)
    else if msg.topic == "slack":                          // follow-up @mention (see SlackThreadWorkflow)
      ctx = await step(() => gatherThreadContext(m.channel, m.threadRoot, oldest=lastTs)); …
    else if msg.topic == "session":
      if msg.event.kind == "terminal": …; return
      await step(() => dispatch(m, msg.event))             // user_question / file_shared / asset / question_answered
```

**End-to-end trace of one question:**
```
agent AskUserQuestion → harness defers → user_question event
  → SessionIngestWorkflow.send → thread workflow recv → dispatch → post Block Kit in thread
human answers in Slack (button/modal)  ──OR──  human answers in web UI
  Slack: interactivity endpoint → DBOS.send(answer) → thread workflow → answerQuestion step ─┐
  web:   web UI → AnswerQuestion RPC ───────────────────────────────────────────────────────┤
                                                                                              ▼
  coordinator resumes the agent (ADR 0054) → emits question_answered
  → SessionIngestWorkflow → thread workflow → dispatch → chat.update("✓ answered: X")
```

## Outbound effects: idempotency without a table

Outbound Slack calls need no bespoke ledger — DBOS step semantics already supply the
three things a table would:

- **Replay dedupe.** Every post/upload/update is a DBOS `step()`. A *completed*
  step is never re-run on replay; DBOS returns its checkpointed result. So a
  workflow that crashes and replays does not re-post what already went out.
- **`provider_ref`.** The question message's `ts` is just a step return value. The
  thread workflow holds the live map (`tool_call_id → ts`) in workflow-local state,
  reconstructed deterministically on replay and threaded through `continue-as-new` —
  no durable row needed to find the message later for `chat.update`.
- **Retries.** Steps opt into `retriesAllowed` + backoff natively for transient
  Slack 5xx.

Single ingest + single thread workflow means each session event flows through
**once** (the ingest cursor walks `after_idx` forward; the `ingest:<sid>` workflowID
is the one-pump guarantee), so there is no cross-path/cross-instance duplication for
a semantic key to dedupe — that only arises under branch-fanout (a non-goal).

- **Honest limitation (unchanged by dropping the table).** The irreducible window
  is a crash *between* Slack's HTTP ack and the step-checkpoint commit — a table has
  the identical window (`ack → row commit`), because `chat.postMessage` has **no
  native idempotency key**. Accepted for fire-and-forget posts (link, asset,
  closing summary); a rare duplicate there is low-harm. **Reactions are exempt** —
  `reactions.add` is naturally idempotent (`already_reacted` → success).
- **Escape hatch (deferred, additive).** To actually *close* the window where it
  matters most — questions (a duplicate = two live button sets) — embed the semantic
  key in Slack message `metadata` and reconcile on recovery: before re-posting,
  read the thread for a message already carrying that key; adopt its `ts` if found.
  That reconciliation wants a durable "intended-but-unconfirmed" set, so it
  re-introduces a small effects table **at that point, not now** — the same
  "additive when a real need exists" rule applied to the cut 5-table substrate.

## `append_system_prompt` (cross-tier, net-new)

A generic, optional addition so triggers can flavor the agent's system prompt
(Slack: "You were triggered from a Slack thread; a human may answer your questions
there; be concise.").

- **Proto:** `CreateSessionRequest.append_system_prompt = 11` (fields 1–10 taken;
  4 reserved). Optional.
- **Coordinator:** thread it through `grpc_app/convert.rs` →
  `api/sessions::create_session_core`; **persist it** so it replays on resume —
  this is **net-new persistence** (there is no session-prompt storage today),
  mirroring the `harness_env`→`session_secrets` replay discipline.
- **Harness:** `engram-harness-claude` `build_claude_argv` (`src/main.rs:2132`,
  sole caller `:1315`) gains a parameter and appends `--append-system-prompt
  <value>` when set. **This is a call-site signature change, not a one-line flag
  add.** Other harnesses map the same field to their own mechanism.
- **Config:** value is per-trigger-source (`system_prompt_append` from
  `getTriggerConfig`), composed at create time; composes *with* the profile.
- **Caveat:** the harness is `cfg(target_os = "linux")` — invisible to macOS
  clippy, 0 tests under macOS nextest. Verify via `cargo clippy --target
  aarch64-unknown-linux-musl -p engram-harness-claude` and `just test-linux
  engram-harness-claude`.

## Coordinator change: `ListSessionEvents`

`ListSessionEvents(session_id, after_idx, limit) → {events, next_after_idx}` — a
unary, **unfiltered** paginated read of the existing append-only log (curation is
the consumer's concern). `StreamEvents` already exists for the low-latency tail;
the unary form is required for catch-up/backfill and is far more testable (a CI
test asserts exact batches without racing a stream). This is the **only**
coordinator change in the reverse path — prompt idempotency (Decision 9) lands
entirely orchestrator-side, reusing the harness's existing ADR-0052 dedupe.

## DBOS adoption

- `@dbos-inc/dbos-sdk` (4.x) **embedded in the Bun orchestrator-worker**,
  **un-bundled** (never `bun build` it — mark external; the two historical Bun
  bugs #1126/#1127 and #991 are fixed on 4.x). `DBOS.launch()` before serving,
  `DBOS.shutdown()` on exit, `runAdminServer: false` in prod.
- **System DB:** `systemDatabaseSchemaName: "dbos"` in the orchestrator's existing
  Postgres. Pre-create with `npx dbos schema` in privilege-restricted prod.
- **Idempotent starts:** `workflowID` is the idempotency key — `task:<id>` for the
  thread, `ingest:<session_id>` for the pump.
- **Human-in-the-loop waits:** `DBOS.send`/`recv` (durable Postgres mailbox via
  LISTEN/NOTIFY + polling) — a workflow waiting hours on a Slack answer is
  suspended in PG, not a pinned thread.
- **`operation_outputs` growth:** a long session means many `SessionIngestWorkflow`
  steps; bounded reads keep step count proportional to *event activity*, and the
  ingest workflow `continue-as-new`s (carrying `after`) to bound its history.
- **Versioning:** workflows tagged with an app version; a deploy leaves old-version
  in-flight workflows dormant unless a blue-green old-version worker drains them or
  `DBOS.patch()` branches logic in place.
- **Cron (future):** `@DBOS.scheduled({crontab})` exactly-once-per-interval is the
  cron-source mechanism.

**Risk:** Bun support is unofficial. Fallback if a Bun release breaks DBOS is a
**Node sidecar** running the engine, driven via `DBOSClient` — additive, since all
durable state is in Postgres.

## Identity seam

One function, no schema: `resolveEngramsUser(provider, externalUserId) →
engramUserId | null`. Slack: `users.info` → `profile.email` (needs
`users:read.email`) → better-auth user by email. Resolved user becomes the task's
`created_by_user_id`, so existing CASL applies unchanged. Unlinked → ❌ + log-in
message.

## Slack surface: `app_mention`, not the Assistant feature

We deliberately do **not** use Slack's "Agents & AI Apps" (Assistant) feature as
the trigger surface. It is a **1:1 DM / split-pane** experience: it cannot be
`@mention`ed to act on a **channel thread's** context (our trigger), and its
affordances (`assistant.threads.setStatus` "thinking…", suggested prompts) render
only inside assistant DM threads, not channel threads. Enabling it adds the
`assistant:write` scope and appears to gate Slack Marketplace submission to "select
partners" — a constraint a plain `app_mention` HTTP app does not carry. The choice
is **surface-only**: the Assistant feature changes none of our plumbing
(`chat.postMessage(thread_ts)`, `conversations.replies`, Block Kit underneath), so
the framework is surface-agnostic and the Assistant pane can be added later as an
optional secondary 1:1 entry point feeding the same `SlackThreadWorkflow`.

## Slack scopes (for the separate install layer)

`app_mentions:read` (trigger), `chat:write` (post/update), `reactions:write`
(the 👀/✅/❌ acks), `files:write` (upload "show your work" media),
`channels:history`/`groups:history` (thread context + `conversations.replies`
cursor), `users:read` + `users:read.email` (identity). Plus the **Interactivity
Request URL** configured (distinct from the Events URL) for the
`block_actions`/`view_submission` payloads.

## Security & authz

- HMAC verification on **both** endpoints; bot token sealed in the org-secret store
  (provided by the install layer).
- Triggered sessions run with the **profile's** capabilities/network/secrets
  exactly as a UI task — **no new privilege path**. The agent never receives
  control-plane creds; the orchestrator mediates all side effects.
- DBOS step checkpointing prevents replay-driven double-posting (a completed send
  is not re-run).
- Task owned by the mapped engram user ⇒ it appears in their web UI under their role.

## Correctness invariants (pin these in implementation)

1. **Effect-before-cursor ordering.** In the ingest workflow, the per-event
   `DBOS.send` (a checkpointed step) commits **before** the cursor (`after`)
   advances; a crash in between replays the send, which DBOS returns from its
   checkpoint (no re-send). This is what makes the workflow-local cursor safe
   without a table.
2. **`run_completed` is not terminal.** A session re-runs on a follow-up
   `@mention`. Exit the loop only on terminal `status_changed`
   (Completed/Failed/Dead); drain to it, then summarize.
3. **3s-ack ordering.** verify → `startWorkflow` committed → `DBOS.send` committed →
   200. No Slack API calls in the handler; identity/context fetches happen inside
   the workflow, off the ack path.
4. **Thread-reuse epoch.** `workflowID = task:<hash(thread)>` is deterministic; if
   the prior thread workflow is in a terminal DBOS state and a new mention arrives
   (thread reused weeks later), the start is a no-op and the mention is swallowed.
   Include an epoch/successor rule so a new ask on a dormant thread starts a fresh
   workflow.
5. **`CreateTask` type guard.** `tasks.ts` currently rejects non-`chat` types — add
   `slack_thread` to the accepted set.

## Durability matrix

| Failure | Recovery |
| --- | --- |
| Slack retries an un-acked event | handler only 200s after `startWorkflow` + `send` commit; retry re-runs idempotently |
| Crash after start, before send, before ack | no 200 → Slack retry → `startWorkflow` no-op + `send` lands; `event_id` idempotency = no dup |
| `SlackThreadWorkflow` crashes mid-loop | replays from top; completed steps return checkpoints (so a completed post isn't re-sent); durable `recv` message redelivered |
| `SessionIngestWorkflow` ("pump") dies | DBOS recovery from checkpointed `after`; `workflowID` = no second pump; no lease/takeover |
| Session idle-evicted between mentions | `SendPrompt` → `ensure_active` auto-resume (Dead → 410 → ❌ + message) |
| `SendPrompt` step replayed after crash | deterministic `prompt_id = slack:<event_id>` → harness `seen_prompt_ids` dedupes (ADR 0052); no double-run (Decision 9) |
| Answer delivered twice (Slack + web) | `AnswerQuestion` idempotent (ADR 0054) |
| Duplicate reaction on replay | `reactions.add` → `already_reacted` → success |

## Design review (why the substrate is small)

An adversarial review challenged the exploration note's 5-table pump
(`session_event_inbox`, `workflow_session_cursor`, `workflow_session_subscription`,
`session_event_ingest` lease, `slack_event_inbox`) + a per-event router workflow.
Findings, adopted here:

- The **coordinator log is already a durable, ordered, replayable inbox**, so
  `session_event_inbox` is a redundant (and curated-lossy) second copy. **Cut** —
  read the log directly.
- The **cursor is a checkpointed scalar** = DBOS workflow state. `workflow_session_
  cursor` **cut**.
- The **pump's lease** is subsumed by `workflowID` uniqueness on a per-session
  ingest *child-workflow*. `session_event_ingest` + the separate leased worker
  role **cut**.
- The **subscription** is the parent→child workflow relation. `workflow_session_
  subscription` **cut**.
- **Inbound durability is Slack's retry + idempotent `startWorkflow`/`send`**, not a
  router workflow; `slack_event_inbox` + `SlackIngestWorkflow` **cut**.
- **`external_effect` also cut** — DBOS step checkpoints already give replay dedupe,
  `provider_ref` (the question `ts` lives in workflow-local state), and retries; the
  cross-path/cross-instance dedupe a semantic key would add isn't reachable with a
  single consumer. Re-introduced additively only to close the question-dup window
  (recovery reconciliation) or under branch-fanout.

Net: 5 tables + 2 extra workflows → **0 new tables + the 2 workflows that do the
work.** All cut machinery is **re-introducible additively** in the branch-fanout ADR
when a second consumer actually exists; the present design precludes none of it.

## Phasing

- **P0 — DBOS foundation.** Add `@dbos-inc/dbos-sdk` (un-bundled), `dbos` schema,
  `launch`/`shutdown`, a trivial workflow + a crash/restart recovery test (with a
  Postgres service in the orchestrator CI lane).
- **P1 — Reverse channel.** No orchestrator migration. The `ListSessionEvents`
  coordinator RPC (Rust lane test); `SlackThreadWorkflow` + `SessionIngestWorkflow`
  skeleton with the recv/drain loop, the workflow-local `questionTs` map, and
  `continue-as-new` (threading the map through); tests for crash/restart, duplicate
  sends, cursor reconstruction, terminal exit.
- **P2 — Slack adapter.** Events + interactivity endpoints (verify/ack/dedupe); the
  communication policy (👀/✅/❌ reactions, link, Block Kit questions, asset posts,
  closing summary); `@mention` context gathering + `SendPrompt` follow-ups; identity
  seam; consume the install layer's token + trigger config.
- **P3 — `append_system_prompt`.** Proto #11 → coordinator persistence + replay →
  harness flag; per-trigger config; Linux-target verification.

Each phase is its own PR, the ADR updated between phases per repo convention.

## Future (designed-for, not built)

- **More source adapters:** Linear/Jira (comment-based policy; HMAC webhooks) and
  cron (`@DBOS.scheduled`). Each is a `CommunicationPolicy` impl + an ingest path.
- **Profile classifier:** today the trigger uses the source's `default_profile_id`;
  later an automatic classifier picks per-thread.
- **Branch fanout / CI swarms / workrooms.** When a task needs ≥2 session
  consumers, re-introduce the inbox + a `consumer_name`-keyed subscription/cursor
  (the exact tables cut above) plus control-plane `CreateCheckpoint`/`ForkSession`.
  Additive — the present 2-workflow design does not block it.

## Open questions

1. Closing-summary composition — templated (status + asset links) vs. enriched by
   the session's last assistant message?
2. Should both reactions persist (👀 + ✅) or swap to a single ✅? (Default: persist.)
3. Trigger-config home — co-located with the separately-built Slack install record,
   or its own `trigger_source` table? (Coordinate with that work.)

## References

- `docs/post-adr0039-background-agent-workflows.md` — the exploration note (boundary
  principle + two-direction reverse channel adopted; 5-table pump rejected per
  §Design review).
- ADR 0051 §4 (anticipated webhook→workflow→session→publish), ADR 0054
  (`user_question`/`question_answered` + `AnswerQuestion`), ADR 0056/0057
  (integration assets + profile policy), ADR 0034 (idle-eviction + resume).
- DBOS TypeScript: workflows/steps, `send`/`recv`, `continue-as-new`, scheduled
  workflows, workflow IDs as idempotency keys (docs.dbos.dev/typescript).
- Slack: `app_mention`, `conversations.replies`, request signing + 3s ack +
  retry/`event_id`, `reactions.add`, Block Kit `block_actions` + `views.open`,
  `users.info` (docs.slack.dev).
```
