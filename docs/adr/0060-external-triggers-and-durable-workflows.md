# ADR 0060: External triggers and durable background-agent workflows (Slack-first)

Status: 2026-06-25 — **Accepted.** Substrate simplified after an adversarial design
review (see §Design review): the reverse channel is DBOS-native (2 workflows, zero
new tables), not the 5-table pump the exploration note sketched. Shipped Slack-first
end to end (events + interactivity, full AskUserQuestion round-trip, enriched closing
summary), unit-tested per tier; the DBOS-engine integration / live-Slack e2e is
deferred by an explicit testing-strategy decision (fast unit tests now).

**Commit chain.**
- P0 (DBOS foundation): `d5bd3603` embed engine, `407b4888` CI Postgres service.
- P1 (reverse channel): `2a2dd231` `ListSessionEvents` RPC, `005ef13a` its authz,
  `74b9c857` bounded reader+curation, `6253d901` ingest pump, `399c9479` single-topic
  mailbox, `6b34268b` thread workflow + `CommunicationPolicy` seam, `37be7d13` defer
  engine integration tests.
- P2 (Slack adapter): `990a4f77` trigger scopes, `f82b7901` `is_default`, `e159a9e2`
  its web toggle, `15eb707c` identity seam, `a1b20eb4` harness `--append-system-prompt`,
  `3a72cf5e`+`7b43ab40` `compileSessionCreateInput` + real `ThreadControlPlane`,
  `1492b5d7` events endpoint, `fcbdeeea` closing-summary enrichment, `5baac59a`
  interactivity + Block Kit answer contract, `5f335c49` Slack policy + rendering,
  `34e691e5` wire into the orchestrator, `16a7ae13` task `source` traceability.

**Divergences from the sketch (all deliberate).**
1. **`append_system_prompt` rides `harness_env`, not a proto field.** Decision 8's
   `CreateSessionRequest.append_system_prompt = 11` was dropped: the orchestrator
   sets `ENGRAM_APPEND_SYSTEM_PROMPT` in the existing `harness_env` map (already
   injected, persisted, and replayed by the coordinator — ADR 0051 Drip A), and the
   harness turns it into `--append-system-prompt`. **Zero coordinator/proto change**;
   P3 collapses into P2. (Superseded text below.)
2. **Slack request verification uses the SDK, not hand-rolled crypto.** Both webhooks
   verify with `@slack/bolt`'s standalone `isValidSlackRequest` (still v0 HMAC + the
   5-min staleness window) against a `slack.signing_secret` **org secret** — resolved
   coordinator-side via the credential path (the only orchestrator secret-read seam),
   not a bespoke env var. "verifyHmac" in the pseudocode below is that call.
3. **No `continue-as-new` in DBOS — the pump self-restarts.** `SessionIngestWorkflow`
   starts a fresh deterministic `ingest:<sid>#<epoch+1>` to bound `operation_outputs`;
   `recv` is single-topic, so the "one recv over {session ∪ trigger} events" is one
   `THREAD_TOPIC` with a tagged-union message. Workflow-local state (`questionTs`,
   the asset recap) is rebuilt deterministically on replay — no table, no hand-off.
4. **Closing summary is enriched** (Open question 1, resolved): the session's last
   assistant message + a recap of the durable assets it produced + a session link.
   The reader surfaces the last assistant `agent_message` per page (no extra RPC; the
   pump already walks the log) and rides it on `session_terminal`.
5. **The `slack_thread` task is persisted server-side** by the `ThreadControlPlane`,
   bypassing the chat-only `CreateTask` RPC (P2.11 needs no RPC change), and records
   the trigger ref (team/channel/threadRoot) on `task.source` for operability.
6. **`botUserId` is unset at wiring time** (follow-up): the Slack policy strips all
   `<@…>` mentions from gathered prompts rather than only the bot's — resolving it via
   `auth.test()` at startup is a cheap later refinement.

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
  harness (Claude → `--append-system-prompt`), set by the **triggering workflow**
  (a constant on its `CommunicationPolicy`, **not** connector config).
- A **default profile** (`is_default` on the profile model) so a trigger — which has
  no UI to pick one — launches with the org's designated profile.
- A reverse-channel design that is **minimal for v1** yet additively extensible to
  the future fanout cases (§Future).

**Non-goals (this ADR)**

- A new Slack OAuth/install layer — **not needed.** The `slack` connector already
  exists (`orchestrator/src/connectors/slack.json`, ADR 0056–0058): an `oauth` facet
  ("Add to Slack", `routes/integration-oauth.ts`), a KEK-sealed `slack.bot_token`
  org secret, and an in-process authenticated client via `getSlackClient()`
  (`integrations/slack.ts`). This ADR **extends** that connector (adds scopes) rather
  than building a parallel layer. The only out-of-band step is Slack-app-dashboard
  config (Event + Interactivity Request URLs, client creds).
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
8. **`append_system_prompt` rides `harness_env`** (see Divergence 1 — supersedes the
   original "new `CreateSessionRequest` field #11"). Its value is a **constant the
   triggering `CommunicationPolicy` provides** (Slack's is fixed in code): the
   orchestrator folds it into `harness_env` as `ENGRAM_APPEND_SYSTEM_PROMPT` at
   session create, and the harness maps that to `--append-system-prompt`. **Not** read
   from connector config (one connector can back many triggers, so per-connector
   trigger config would not generalize).
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
10. **A triggered session launches with the org's default profile.** A trigger has
    no UI to pick a profile, so the session uses the profile flagged `is_default` (a
    new **at-most-one** flag on the orchestrator's profile model). No default set →
    ❌ + an actionable message; don't start. (Replaces the cut per-connector
    `defaultProfileId`.)

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
POST /api/v1/integrations/slack/events:
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
  profile = await step(() => getDefaultProfile())                 // the org's is_default profile
  if !profile: { await fail(m, `No default profile is configured — set one in ${ENGRAMS_URL} first.`); return }
  prompt  = await step(() => gatherThreadContext(m.channel, m.threadRoot, oldest=null))   // full thread
  try:    session = await step(() => createSession({profile: profile.id,
                                     append_system_prompt: policy.systemPromptAppend, prompt}))
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
systemPromptAppend          // constant flavor for append_system_prompt at create (Decision 8)
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
| `agent_message` (assistant text) | **Coalesced per turn** into one `section`-block message: the first response posts; consecutive ones `chat.update` it (appended) until a question/asset/new-mention seals the bubble, or it exceeds ~8k chars (rolls to a new message). The prompt echo (`user`) + system notes never post. |
| `run_started` / `run_completed` | The per-turn working/idle indicator on the triggering message: `run_started` → ⏳ `onWorking`; `run_completed` (non-terminal) → clear ⏳ + ✅ `onIdle` (the "your turn" signal). |
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

**As-built: full-response thread + working/idle indicator (2026-06-25).** The thread
shows the *whole* conversation, not only artifacts/questions. Assistant `agent_message`s
are forwarded by the pump (assistant role only) and the framework **coalesces consecutive
responses within a turn into one message** (append via `chat.update`); a question, an
asset, or a new mention seals that bubble so the next response opens a fresh message
below it (chronological order). The `onAck` sketch above split into a per-turn reaction
lifecycle on the triggering message — **`onPickup` 👀 (received) → `onWorking` ⏳ (run
started) → `onIdle` ✅ (run completed, non-terminal = idle, "your turn")** — which is the
explicit "session is waiting for the user" signal a static delivered-✅ could not express.
`onStarted` now only posts the session link. True token-by-token streaming (the ephemeral
`agent_message_chunk` deltas, off the durable pump) is intentionally deferred — it needs a
separate live `streamEvents` consumer with rate-limit throttling and single-writer
coordination, a follow-up beyond this ADR. Assistant text posts as raw markdown (Slack
mrkdwn fidelity is a known follow-up).

**As-built: `file_shared` now uploads the actual bytes (2026-06-26).** The `file_shared`
row above was *designed* but shipped, in the original P2.10, as the text-line fallback only
(`📎 <caption>`), so a `share-file` artifact rendered inline in the web UI but reached Slack
as a bare caption with no image and no link. `onAsset` now does what the table specifies:
for a `file_shared` event it fetches the artifact bytes from the coordinator
(`SessionService.getArtifact`, the same server-stream the web artifact route proxies — see
`control-plane/artifact-fetch.ts`, which collects the stream into one buffer under a hard
size cap) and uploads them into the thread via `WebClient.files.uploadV2` (the `@slack/web-api`
helper that wraps the `getUploadURLExternal` → PUT → `completeUploadExternal` trio the table
names), with the caption as `initial_comment`. Two guards keep it safe: artifacts over
`MAX_SLACK_UPLOAD_BYTES` (50 MiB) skip the fetch entirely, and any fetch/upload failure falls
back to a one-line **link to the session** (`🔗 <webUrl|caption>`) — strictly better than the
old un-clickable `📎` line. `onAsset` gained the `session` arg (for the artifact id's session
scope and the fallback link); the closing-summary recap (`summarizeAsset`/`onComplete`) is
unchanged and still lists files as text. **Deployment note:** the upload needs the Slack app's
`files:write` scope (as the table flagged) — without it every upload fails and silently
degrades to the link fallback.

**As-built: the terminal is three-way, not two-way (2026-06-26).** The sketch above
(`if msg.event.ok: onComplete else: onFail "ended in failure"`) collapsed every
non-`completed` terminal state into a failure. But a session reaches `dead` not only
by failing — it also dies when its **sandbox is reclaimed out from under it**: an idle
host roll, a `host_lost` reaper sweep (a host-agent restart, which on the dev stack is
routine), or eviction-durability (ADR 0028) not catching an active session before the
host went away. Reporting that as "The session ended in failure" is wrong and alarming
— the work up to that point stands, the thread just can't continue. So terminal
detection now returns a **`TerminalOutcome`** (`session-events.ts`) mapping
`completed → completed`, `failed → failed`, `dead → neutral`, and the thread workflow
switches three ways: `onComplete` (closing summary), `onFail` (❌ — a genuine run
failure), and a new **`onNeutralClose`** that posts a plain, un-alarming note
("This session is complete. Start a new session if you'd like to continue.") with no ❌
and no reaction. (`host_lost` stays non-terminal — only `dead` exits the loop.) This
also matters on prod, where any host roll that outruns eviction durability would
otherwise mis-report a completed session as failed.

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
deterministically on replay from the checkpointed `onUserQuestion` step outputs.

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
POST /api/v1/integrations/slack/interactivity:
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
  reconstructed deterministically on replay from the checkpointed step outputs —
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

## `append_system_prompt` (via `harness_env` — no proto/coordinator change)

A generic way for triggers to flavor the agent's system prompt (Slack: "You are
running inside an engrams session triggered from a Slack thread; keep replies
concise; ask via AskUserQuestion; the user can't see your terminal").

**Decision (final): ride the existing `harness_env` channel** rather than add a
proto field. The coordinator already injects `harness_env` into the guest launch
env *and* persists it to `session_secrets` for replay-on-resume (ADR 0051 Drip A),
so the replay/persistence the sketch wanted is **already built** — for free.

- **Orchestrator:** `compileSessionCreateInput` takes an optional `extraHarnessEnv`;
  the `ThreadControlPlane` folds the policy's constant in as
  `harness_env.ENGRAM_APPEND_SYSTEM_PROMPT`. No proto, no coordinator code.
- **Harness:** `engram-harness-claude` `build_claude_argv` gains an
  `append_system_prompt: Option<&str>` param and appends `--append-system-prompt
  <value>` when non-empty; the call site reads
  `std::env::var("ENGRAM_APPEND_SYSTEM_PROMPT")`. Other harnesses map the same env
  var to their own mechanism.
- **Value source:** a **constant on the triggering `CommunicationPolicy`**
  (`systemPromptAppend` — Slack's is fixed in code), passed at create time; composes
  *with* the profile. **Not** connector config: one connector can back many triggers,
  so per-connector trigger config would not generalize.
- **Caveat:** the harness is `cfg(target_os = "linux")` — invisible to macOS
  clippy, 0 tests under macOS nextest. Verified via `just test-linux
  engram-harness-claude` (and `cargo clippy --target aarch64-unknown-linux-musl`).

## Default profile (`is_default`, orchestrator-only, net-new)

A trigger has no UI to pick a profile, so a triggered session launches with the
org's **default profile**. Profiles are orchestrator-owned (ADR 0052; the control
plane never learns about them), so this is a purely orchestrator-side addition — no
coordinator or proto change on the control-plane side.

- **Schema:** `profile.is_default boolean not null default false` via a **new
  migration** (applied migrations are checksum-immutable). **At most one** active
  default: setting a profile default clears the prior in the same transaction; a
  soft-deleted default simply leaves none.
- **Proto (orchestrator `profile.proto`):** `Profile.is_default = 15`,
  `CreateProfileRequest.is_default = 11`, `UpdateProfileRequest.is_default = 12`.
- **Store:** `ProfileStore.getDefault(): Promise<ProfileRow | null>` (active only);
  the set/clear-others invariant is enforced inside `create`/`update`.
- **Trigger use:** `SlackThreadWorkflow` resolves it via `getDefaultProfile()`; none
  configured → ❌ + an actionable message, don't start.
- **UI:** a single-select "default" toggle on the profile editor.

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
  ingest workflow **self-restarts** (starts a fresh `ingest:<sid>#<epoch+1>` carrying
  `after` + the last-seen assistant message) to bound its history — DBOS has no
  `continue-as-new`, so a deterministic successor id makes the restart idempotent.
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

## Slack wiring: extend the existing connector

The Slack edge plugs into machinery that already exists; we add to it rather than
rebuild it.

- **Sender = `getSlackClient()`.** Every outbound call (`chat.postMessage`/`update`,
  `reactions.add`, `views.open`, `conversations.replies`, `files.*`) is a method on
  the cached, authenticated `@slack/web-api` `WebClient` from
  `integrations/slack.ts`. The bot token resolves coordinator-side (Mode B), never
  reaching a guest. No hand-rolled HTTP, no bespoke token plumbing.
- **Routes live under the integrations namespace**, beside
  `/api/v1/integrations/:provider/oauth/*`:
  `/api/v1/integrations/slack/events` (Event Subscriptions) and
  `/api/v1/integrations/slack/interactivity` (`block_actions`/`view_submission`).
  Each does its own v0 HMAC verify; they are net-new (today `routes/events.ts` is the
  browser SSE route, unrelated).
- **Scopes to add to `slack.json` `oauth.scopes`** — the connector grants
  `chat:write`, `channels:read`, `groups:read`, `users:read`, `files:write`,
  `files:read` today. This ADR needs **five more** (verified against
  `docs.slack.dev`): `app_mentions:read` (receive the trigger), **`channels:history`
  + `groups:history`** (`conversations.replies` reads messages — `channels:read` only
  grants metadata), `reactions:write` (the 👀/✅/❌ acks), and `users:read.email`
  (the `email` field on `users.info`, required *in addition to* `users:read`).
  `im:history`/`mpim:history` are **not** needed — the surface is `app_mention` in
  channels, not DMs.
- **No trigger config on the connector.** The two former per-trigger inputs live
  off the connector now: the profile is the org default (`is_default`), and the
  system-prompt flavor is a constant on the Slack `CommunicationPolicy`. A connector
  can back many triggers, so a per-connector `trigger` facet would not generalize.
- **Single-workspace for v1.** The connector resolves one `slack.bot_token`. Multi-
  workspace (a per-`team_id` token) is a real future extension — the OAuth
  `tokenSecretRef` is a single ref today.

## Security & authz

- HMAC verification on **both** endpoints; bot token is the existing connector's
  KEK-sealed `slack.bot_token`, resolved coordinator-side via `getSlackClient()`.
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

- **P0 — DBOS foundation (done).** Added `@dbos-inc/dbos-sdk` (un-bundled), `dbos`
  schema, `launch`/`shutdown`, a Postgres service in the orchestrator CI lane.
- **P1 — Reverse channel (done).** No orchestrator migration. The `ListSessionEvents`
  coordinator RPC (Rust lane test) + its passthrough authz; `SlackThreadWorkflow` +
  `SessionIngestWorkflow` with the recv/drain loop, the workflow-local `questionTs`
  map rebuilt on replay, and **self-restart** (DBOS has no `continue-as-new`); reader
  + curation unit-tested (terminal detection, cursor, last-assistant-message).
- **P2 — Slack adapter (done).** Extended the `slack` connector (five scopes); the
  profile `is_default` flag (migration + proto + store `getDefault` + web toggle) and
  `getDefaultProfile()`; the `/api/v1/integrations/slack/{events,interactivity}`
  endpoints (SDK `isValidSlackRequest` / ack / dedupe); the communication policy via
  `getSlackClient()` (👀/✅/❌ reactions, link, Block Kit questions + answer modal,
  asset posts, enriched closing summary); `@mention` context gathering + `SendPrompt`
  follow-ups; identity seam; `append_system_prompt` via `harness_env` (folds in the
  old P3 — no proto/coordinator change).

Each phase is its own PR, the ADR updated between phases per repo convention. The
DBOS-engine integration / live-Slack e2e is deferred (fast unit tests now).

## Future (designed-for, not built)

- **More source adapters:** Linear/Jira (comment-based policy; HMAC webhooks) and
  cron (`@DBOS.scheduled`). Each is a `CommunicationPolicy` impl + an ingest path.
- **Profile classifier:** today the trigger uses the org default profile
  (`is_default`); later an automatic classifier picks per-thread.
- **Branch fanout / CI swarms / workrooms.** When a task needs ≥2 session
  consumers, re-introduce the inbox + a `consumer_name`-keyed subscription/cursor
  (the exact tables cut above) plus control-plane `CreateCheckpoint`/`ForkSession`.
  Additive — the present 2-workflow design does not block it.

## Open questions (resolved)

1. Closing-summary composition — **resolved: enriched.** It posts the session's last
   assistant message + a recap of the durable assets produced + a session link
   (Divergence 4).
2. Should both reactions persist (👀 + ✅) or swap to a single ✅? **Resolved: both
   persist** (👀 on pickup → ✅ on start), per the default.

## References

- `docs/post-adr0039-background-agent-workflows.md` — the exploration note (boundary
  principle + two-direction reverse channel adopted; 5-table pump rejected per
  §Design review).
- ADR 0051 §4 (anticipated webhook→workflow→session→publish), ADR 0054
  (`user_question`/`question_answered` + `AnswerQuestion`), ADR 0056/0057
  (integration assets + profile policy), ADR 0034 (idle-eviction + resume).
- DBOS TypeScript: workflows/steps, single-topic `send`/`recv`, scheduled
  workflows, workflow IDs as idempotency keys (docs.dbos.dev/typescript). Note: no
  `continue-as-new` — bound history via a deterministic self-restart.
- Slack: `app_mention`, `conversations.replies`, request signing + 3s ack +
  retry/`event_id`, `reactions.add`, Block Kit `block_actions` + `views.open`,
  `users.info` (docs.slack.dev).
```
