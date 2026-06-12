# Post-ADR 0039 background agent workflows

Status: design note / product architecture sketch.

Related:
- [ADR 0039](./adr/0039-typescript-orchestration-tier.md): TypeScript
  orchestration tier, task aggregate root, control plane as raw resource API.
- [ADR 0045](./adr/0045-live-session-teleport.md): unified memory substrate,
  forkable machine-state direction, live teleport, density.
- [orchestration-tier-plan](./orchestration-tier-plan.md): concrete ADR 0039
  implementation plan.

## Thesis

After ADR 0039, Engram's product advantage should not be "we run an agent in a
cloud VM." Ona, Factory, Fabro, and similar products can all do some version of
that.

The product advantage should be: **machine state is a first-class product
primitive.** We can checkpoint it, fork it, race it, judge it, fuse it, rewind
it, move it, and prove what happened inside it.

That changes the UX from "one agent tries a task" to:

- branch a warm agent session into many strategies;
- run speculative repair swarms from an identical reproduced failure state;
- spin up multi-agent workrooms where every specialist gets the same exact
  prepared environment;
- compare real diffs, tests, logs, screenshots, and videos across branches;
- select or synthesize the winning patch;
- publish the outcome back to Slack, GitHub, Linear, or another source system.

ADR 0039 provides the first required cut:

```text
React UI / Slack / GitHub / Linear
        |
        v
TypeScript orchestrator
  - tasks, auth, policy, workflows, integrations
  - durable workflow state
  - product semantics
        |
        v
Rust control plane
  - sessions, checkpoints, forks, artifacts, event log
  - microVM scheduling, restore, teleport, COW, storage
```

The next phase turns `task = chat with one session` into
`task = durable workflow that may own many sessions`.

## Where we are versus off-the-shelf products

Publicly, Ona, Factory, Fabro, and similar products are strongest at productized
background-agent workflows:

- Slack/GitHub/Linear-style task entrypoints;
- cloud or containerized execution environments;
- headless agent runs;
- human gates and approvals;
- checkpoints, retries, and observable workflow graphs;
- team and enterprise controls;
- artifacts such as logs, diffs, PRs, screenshots, or summaries.

ADR 0039 does not, by itself, make Engram product-complete against those
systems. It gives us the right spine:

- a TypeScript orchestrator that can own product semantics, auth, tasks,
  integrations, workflows, and policy;
- a Rust control plane that can stay focused on machine resources: sessions,
  snapshots, artifacts, event logs, secrets, fleet, and images;
- a task aggregate root that can grow from one chat session into many sessions,
  branches, artifacts, approvals, and external effects.

The important difference is that Engram can make machine state itself a product
primitive. Off-the-shelf agent products generally expose "run an agent in an
environment." Engram can expose "checkpoint this exact machine, fork it into
many competing futures, compare the real outcomes, fuse a result, and keep
the winning session live."

Directional progress:

```text
ADR 0039 implementation:          roughly 40-45% complete
Off-the-shelf product parity:     roughly 20-30% complete
Substrate-native product moat:    early; primitives exist conceptually, but
                                  fork/checkpoint/join are not productized
```

The remaining ADR 0039 work is mostly product/application plumbing:

- build the Hono/Connect TypeScript orchestrator package;
- add orchestrator persistence and the real `TaskService`;
- proxy session streams, shell relay, artifacts, and tokens through the
  orchestrator;
- move the web app to the task-first API;
- cut over public clients so the Rust coordinator is no longer the product API.

The post-ADR 0039 work is what creates parity and then differentiation:

- durable DBOS-backed workflows;
- integration inboxes for Slack, GitHub, Linear, CI, and webhooks;
- `session_event_inbox`, subscriptions, and workflow cursors;
- control-plane checkpoint/fork/diff/export-patch APIs;
- branch boards, compare views, human gates, proof artifacts, and publish flows;
- policy, budgets, audit logs, and enterprise controls.

## Product surfaces

### 1. Agent Branching / Parallel Futures

User promise:

> Try several implementation strategies from the same exact machine state,
> then compare the real outcomes.

Typical flow:

1. A parent session reaches a useful checkpoint: repo cloned, dependencies
   installed, tests reproduced, context loaded.
2. The user clicks **Fork Strategies**, or the agent requests it through a
   structured tool.
3. The orchestrator creates a checkpoint and forks N child sessions.
4. Each child gets a different strategy prompt.
5. The UI shows a branch tree and live branch board.
6. The user or a judge workflow selects a winner, asks for changes, or creates
   a fusion branch.

Branch examples:

- `minimal_fix`: smallest patch that makes the failing test pass.
- `root_cause`: deeper investigation and cleaner fix.
- `test_first`: write regression test before implementation.
- `dependency_hypothesis`: check dependency or lockfile causes.
- `refactor_path`: larger cleanup with better long-term shape.

UX:

```text
Task: Fix checkout session refresh bug

Branches
----------------------------------------------------------------------------
minimal_fix        running tests      2 files     low risk      $0.41
root_cause         found culprit      5 files     medium risk   $0.73
test_first         green              3 files     low risk      $0.62
refactor_path      needs review       12 files    high risk     $1.10

[Open] [Compare] [Select winner] [Fork from here] [Fuse selected changes]
```

What makes this substrate-specific:

- Every branch starts from the same warmed machine state, not from N fresh
  environments.
- The marginal cost of branch creation should fall as the memory/disk substrate
  improves.
- The branch tree can preserve exact machine checkpoints, not just Git commits
  or chat transcripts.

MVP:

- Manual fork from a checkpoint.
- Three strategy prompts.
- Branch board with status, diff size, test result.
- Manual winner selection.

Killer version:

- Strategy planner chooses branch prompts.
- Tournament mode eliminates weak branches early.
- Judge branch ranks outputs.
- Fusion branch combines selected hunks from siblings.
- Budget policy stops exploration automatically.

### 2. Speculative CI Repair Swarm

User promise:

> Reproduce the failing CI state once, fork many repair attempts from that
> exact point, and return the first credible green candidate.

Typical flow:

1. GitHub check fails on a PR.
2. Engram creates a CI repair task.
3. One triage/reproducer session checks out the PR and reproduces the failure.
4. Engram checkpoints the reproduced failure state.
5. Engram forks repair branches from that checkpoint.
6. Branches run targeted tests first, then broader validation.
7. The workflow selects the first green candidate or asks a judge branch to
   compare candidates.
8. The user gets a Slack/GitHub update with patch, tests, logs, and proof.

Branch examples:

- `minimal_patch`: smallest source change.
- `test_expectation`: check whether the test is stale.
- `dependency`: inspect dependency or lockfile regressions.
- `environment`: isolate OS/toolchain/config failure.
- `revert_path`: bisect or revert recent local changes.
- `instrumentation`: add logging/assertions to feed other branches.

UX:

```text
CI Repair: PR #1842 checkout_tests failed

Reproduction
  status: reproduced
  command: pnpm test checkout/session-refresh.test.ts
  checkpoint: chk_01j...

Repair swarm
----------------------------------------------------------------------------
minimal_patch      green              1 file      ready to publish
dependency         failed             lockfile conflict
test_expectation   green              tests only, needs human approval
revert_path        failed             regression still present
instrumentation    findings only      root cause: stale auth cookie

[View candidate] [Open PR update] [Ask for smaller diff] [Run more branches]
```

What makes this substrate-specific:

- Reproduction is the expensive part. Once reproduced, branches do not need to
  reclone, reinstall, rebuild, and rediscover the same failure.
- The branch result can include exact command logs, workspace diffs, screenshots,
  videos, and artifacts from the same machine lineage.

MVP:

- GitHub failed-check webhook.
- One reproducer session.
- Four repair branches.
- Select first branch that passes the failing command.

Killer version:

- Failure taxonomy chooses strategies.
- Learned strategy selection per repo.
- Patch fusion.
- CI cache preservation across branches.
- Early stop after confidence/cost threshold.

### 3. Multi-Agent Workrooms

User promise:

> Split one incident, migration, or feature into specialist agents that all
> start from the same prepared state.

Examples:

- Incident: log investigator, metrics investigator, recent-deploy reviewer,
  patch author, runbook writer.
- Large migration: codemod branch, compiler-error branch, tests branch,
  API-compat branch, docs branch.
- PR review: security reviewer, performance reviewer, readability reviewer,
  test reviewer.

The product should present this as a workroom:

```text
Workroom: Investigate latency spike

Agents
----------------------------------------------------------------------------
logs             found 500 spike after deploy 8c12
metrics          p95 correlated with cache miss rate
recent deploys   suspect PR #1842
patch            implementing cache key fix
runbook          drafting incident notes
```

Each agent is a separate session/branch with isolated writes, but all share the
same starting checkpoint and task context.

### 4. Other substrate-native workflow ideas

- **Review gauntlet:** after an implementation branch finishes, fork reviewer
  branches for security, performance, coverage, and maintainability.
- **Flaky test lab:** fork a failing test environment with controlled
  variations in seed, concurrency, time, and dependency versions.
- **Performance race:** fork optimization attempts from the same benchmark
  harness and rank by measured speedup and code complexity.
- **Design alternatives with real diffs:** implement three architecture options
  far enough to compile, then compare tests, complexity, and risk.
- **Patch fusion studio:** select commits or hunks from multiple branches, then
  create a new branch to apply and validate the synthesis.
- **Rewind and ask:** rewind to a checkpoint before a bad agent turn, then try a
  different strategy without paying setup cost again.

## Post-ADR 0039 architecture delta

### New orchestrator responsibilities

The orchestrator needs to own:

- task types beyond `chat`;
- durable workflows;
- integration webhooks and outbound effects;
- task/branch data model;
- session event ingestion into durable application tables;
- workflow-specific cursors over session events;
- Slack/GitHub/Linear connection state;
- branch fanout, branch scoring, joins, and fusion;
- product UI APIs for work queue, task detail, branch board, and compare views.

The orchestrator must still enforce the ADR 0039 boundary:

```text
Sessions propose.
Orchestrator decides.
Control plane forks.
```

Agents should never receive broad control-plane credentials. A session can emit
structured intent, such as "spawn branches" or "ask the Slack user a question."
The orchestrator validates policy, budget, ownership, and task state before
performing any side effect.

### New control-plane primitives

The Rust control plane should remain task/user/integration blind. It needs
more machine-state primitives:

```proto
service SessionService {
  rpc CreateCheckpoint(CreateCheckpointRequest) returns (CreateCheckpointResponse);
  rpc ForkSession(ForkSessionRequest) returns (ForkSessionResponse);
  rpc GetWorkspaceDiff(GetWorkspaceDiffRequest) returns (GetWorkspaceDiffResponse);
  rpc ExportPatch(ExportPatchRequest) returns (ExportPatchResponse);
  rpc ApplyPatch(ApplyPatchRequest) returns (ApplyPatchResponse);
}

message ForkSessionRequest {
  string source_session_id = 1;
  string checkpoint_id = 2;
  optional string prompt = 3;
  optional string harness_secret_id = 4;
  optional string branch_label = 5;
}
```

Early implementation can be checkpoint-backed fork. Later implementation can
become cheaper and more live as ADR 0045's substrate work matures.

### New session-to-orchestrator intent events

Add structured harness events for product intents:

```json
{
  "kind": "orchestrator_request",
  "request": "ask_user",
  "request_id": "req_01j...",
  "payload": {
    "question": "Which environment is failing?",
    "choices": ["production", "staging", "local"]
  }
}
```

```json
{
  "kind": "orchestrator_request",
  "request": "spawn_branches",
  "request_id": "req_01j...",
  "payload": {
    "strategies": [
      {"name": "minimal_fix", "prompt": "..."},
      {"name": "root_cause", "prompt": "..."},
      {"name": "test_first", "prompt": "..."}
    ],
    "join": {"mode": "first_green"}
  }
}
```

```json
{
  "kind": "orchestrator_result",
  "request": "submit_plan",
  "request_id": "req_01j...",
  "payload": {
    "summary": "...",
    "implementation_plan": ["...", "..."],
    "risks": ["..."],
    "needs_approval": true
  }
}
```

These flow through the existing control-plane session event log. The
orchestrator consumes the log and handles them as product events.

## Data model

ADR 0039 adds `task` and `task_session`. Post-ADR workflows need more tables.

### Core task tables

```sql
CREATE TABLE task (
  id TEXT PRIMARY KEY,
  type TEXT NOT NULL,                 -- chat | slack_issue | ci_repair | linear_issue | ...
  title TEXT,
  status TEXT NOT NULL,               -- open | triaging | planning | implementing | reviewing | done | failed
  created_by_user_id TEXT,
  source_json JSONB NOT NULL DEFAULT '{}',
  workflow_run_id TEXT,
  priority INTEGER NOT NULL DEFAULT 0,
  budget_json JSONB NOT NULL DEFAULT '{}',
  policy_json JSONB NOT NULL DEFAULT '{}',
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE task_session (
  task_id TEXT NOT NULL REFERENCES task(id) ON DELETE CASCADE,
  session_id TEXT NOT NULL,
  role TEXT,                          -- triage | implementation | branch | judge | fusion
  parent_session_id TEXT,
  parent_checkpoint_id TEXT,
  branch_id TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (task_id, session_id)
);

CREATE INDEX task_session_session_idx ON task_session(session_id);
```

### Branch tables

```sql
CREATE TABLE task_branch (
  id TEXT PRIMARY KEY,
  task_id TEXT NOT NULL REFERENCES task(id) ON DELETE CASCADE,
  parent_branch_id TEXT REFERENCES task_branch(id),
  session_id TEXT,
  checkpoint_id TEXT,
  name TEXT NOT NULL,
  strategy_prompt TEXT NOT NULL,
  status TEXT NOT NULL,               -- pending | running | passed | failed | selected | abandoned
  result_json JSONB NOT NULL DEFAULT '{}',
  score_json JSONB NOT NULL DEFAULT '{}',
  selected BOOLEAN NOT NULL DEFAULT false,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX task_branch_task_idx ON task_branch(task_id);
```

### Task events and artifacts

```sql
CREATE TABLE task_event (
  id BIGSERIAL PRIMARY KEY,
  task_id TEXT NOT NULL REFERENCES task(id) ON DELETE CASCADE,
  branch_id TEXT REFERENCES task_branch(id),
  kind TEXT NOT NULL,
  payload_json JSONB NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE task_artifact (
  id TEXT PRIMARY KEY,
  task_id TEXT NOT NULL REFERENCES task(id) ON DELETE CASCADE,
  branch_id TEXT REFERENCES task_branch(id),
  kind TEXT NOT NULL,                 -- plan | patch | screenshot | video | log | report
  label TEXT,
  uri TEXT,
  payload_json JSONB NOT NULL DEFAULT '{}',
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

### Integration inboxes

Inbound webhooks should be deduped before they wake workflows.

```sql
CREATE TABLE slack_event_inbox (
  slack_event_id TEXT PRIMARY KEY,
  team_id TEXT NOT NULL,
  channel_id TEXT NOT NULL,
  thread_ts TEXT NOT NULL,
  user_id TEXT,
  event_type TEXT NOT NULL,
  text TEXT,
  payload_json JSONB NOT NULL,
  inserted_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  processed_at TIMESTAMPTZ
);

CREATE INDEX slack_thread_idx
  ON slack_event_inbox(team_id, channel_id, thread_ts);
```

### Session event inbox and cursors

The control plane already has the authoritative session event log. Workflows
need a durable application-facing copy and workflow-specific read positions.

```sql
CREATE TABLE session_event_inbox (
  session_id TEXT NOT NULL,
  idx BIGINT NOT NULL,
  kind TEXT NOT NULL,
  payload_json JSONB NOT NULL,
  inserted_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (session_id, idx)
);

CREATE TABLE workflow_session_subscription (
  workflow_run_id TEXT NOT NULL,
  task_id TEXT NOT NULL REFERENCES task(id) ON DELETE CASCADE,
  session_id TEXT NOT NULL,
  consumer_name TEXT NOT NULL,         -- triage | implementation | branch:<id> | judge
  active BOOLEAN NOT NULL DEFAULT true,
  start_idx BIGINT NOT NULL DEFAULT 0,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (workflow_run_id, session_id, consumer_name)
);

CREATE TABLE workflow_session_cursor (
  workflow_run_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  consumer_name TEXT NOT NULL,
  last_processed_idx BIGINT NOT NULL DEFAULT 0,
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (workflow_run_id, session_id, consumer_name)
);

CREATE TABLE session_event_ingest (
  session_id TEXT PRIMARY KEY,
  owner_id TEXT,
  lease_expires_at TIMESTAMPTZ,
  last_ingested_idx BIGINT NOT NULL DEFAULT 0,
  stream_generation BIGINT NOT NULL DEFAULT 0,
  last_error TEXT,
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

`session_event_inbox` stores facts. `workflow_session_cursor` stores a
particular workflow consumer's progress through those facts.
`session_event_ingest` is pump infrastructure: it tracks which orchestrator
worker owns ingestion for a session, how far ingest has copied events, and
where another worker should resume after failure.

### External effects

Every outbound Slack/GitHub/Linear side effect needs an idempotency record.

```sql
CREATE TABLE external_effect (
  idempotency_key TEXT PRIMARY KEY,
  provider TEXT NOT NULL,              -- slack | github | linear
  effect_type TEXT NOT NULL,           -- post_message | update_message | create_pr | comment
  status TEXT NOT NULL,                -- pending | sent | failed
  provider_ref TEXT,
  request_json JSONB NOT NULL,
  response_json JSONB NOT NULL DEFAULT '{}',
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

Examples:

```text
task:task_123:slack:triage-question:req_abc
task:task_123:session:sess_abc:event:42:post-plan
task:task_123:github:create-pr
branch:br_123:slack:post-video-artifact:art_456
```

## DBOS role

DBOS is the durable workflow engine. It should own:

- workflow IDs and exactly-once starts;
- durable sleeps, waits, messages, and cancellation;
- step retries for transient external failures;
- durable workflow state and observability;
- transactions that commit app state and DBOS checkpoints together.

The TypeScript snippets below are architectural pseudocode. Names like
`DBOS.send`, `DBOS.recv`, `DBOS.runStep`, `DBOS.workflowID`, and
`appDb.runTransaction` stand in for the exact DBOS TypeScript and database
wrapper APIs we select during implementation. The important contract is where
durability, idempotency, and event ownership live.

DBOS should not be the only place where source events live. The source event
facts should live in application inbox tables:

```text
Slack/GitHub/Linear event facts -> integration inbox tables
Control-plane session event facts -> session_event_inbox
Workflow read progress -> workflow_session_cursor
Workflow wakeups -> DBOS.send(...)
```

This keeps DBOS as the workflow brain while keeping event facts queryable,
replayable, and shareable by multiple consumers.

## Why `session_event_inbox` is not just a DBOS workflow detail

DBOS provides durable workflows, steps, transactions, queues, and workflow
messages. Those are useful here. However, a control-plane session event stream
is an open-ended, nondeterministic network source. Putting that stream directly
inside a long-running workflow step makes the wrong thing the checkpoint
boundary: the step may run for hours and only complete when the session ends.

Instead:

```text
control-plane StreamEvents/ListEvents
  -> event pump
  -> INSERT session_event_inbox(session_id, idx) ON CONFLICT DO NOTHING
  -> DBOS.send(workflowID, {sessionId}, "session-events")
  -> workflow drains inbox according to workflow_session_cursor
```

Reasons to keep the inbox as an application table:

- **Fanout:** more than one workflow or projection can consume the same session
  events.
- **Replay:** support/debug tooling can inspect and replay event handling.
- **Backfill:** a workflow created later can catch up from stored events.
- **Idempotency:** `PRIMARY KEY (session_id, idx)` is a simple dedupe boundary.
- **Versioning:** workflow code can change without losing raw facts.
- **Recovery:** if `DBOS.send` succeeds but processing crashes, the cursor says
  what remains; if `DBOS.send` fails after insert, timeout polling can still
  find the event in the inbox.

DBOS messages should wake workflows. They should not be the only copy of the
session event.

## Session event pump

The session event pump runs in the TypeScript orchestrator tier, usually in an
`orchestrator-worker` process next to the DBOS workflow executor and external
effect sender. It is not the Slack workflow, not the branch workflow, and not a
session harness. It is shared ingest infrastructure.

```text
orchestrator-api
  - public API
  - Slack/GitHub webhooks
  - task APIs

orchestrator-workers
  - DBOS workflow executor
  - session event pump
  - external effect sender

orchestrator-db
  - task tables
  - session_event_inbox
  - workflow_session_subscription
  - workflow_session_cursor
  - session_event_ingest

rust control plane
  - authoritative session event log
  - StreamSessionEvents
  - ListSessionEvents
```

The pump's job is narrow:

```text
Rust control plane event log
  -> session event pump
  -> INSERT session_event_inbox(session_id, idx) ON CONFLICT DO NOTHING
  -> load active workflow_session_subscription rows
  -> DBOS.send(workflow_run_id, "session-events", {sessionId, idx})
  -> DBOS workflow drains inbox using workflow_session_cursor
```

The control plane remains the authoritative source for session events. The pump
creates the application-facing copy and sends wakeups. DBOS workflows consume
the copied facts.

### Responsibility boundary

Reconnect responsibility should split this way:

```text
Browser/Slack client reconnects to orchestrator.
Session event pump reconnects to control-plane event streams.
Control plane reconnects or restores session runners and harnesses.
DBOS workflow waits on durable facts; it does not own sockets.
```

That boundary keeps product workflows deterministic enough for DBOS and keeps
control-plane stream handling out of business logic.

### Pump lifecycle

When a workflow creates or attaches to a session, it inserts a subscription:

```text
workflow_session_subscription(
  workflow_run_id = "task:123",
  task_id = "task_123",
  session_id = "sess_abc",
  consumer_name = "triage",
  active = true,
  start_idx = 0
)
```

The pump manager periodically scans for sessions with active subscriptions and
tries to acquire a per-session lease in `session_event_ingest`:

```text
claim lease where
  session_id = sess_abc
  and (owner_id is null or lease_expires_at < now())

set
  owner_id = this worker id
  lease_expires_at = now() + lease ttl
  stream_generation = stream_generation + 1
```

Only the worker holding the lease should actively ingest that session. The
worker heartbeats the lease while it streams. If it dies, another worker takes
the lease after expiration.

Before opening a live stream, the pump catches up:

```text
last_ingested_idx =
  session_event_ingest.last_ingested_idx
  or max(session_event_inbox.idx for session)
  or min(active subscription.start_idx) - 1

ListSessionEvents(session_id, after_idx = last_ingested_idx)
insert batch into session_event_inbox
wake subscribers for inserted events
advance session_event_ingest.last_ingested_idx
```

Then it opens the low-latency stream:

```text
StreamSessionEvents(session_id, since_idx = last_ingested_idx + 1)
```

For every event:

```text
insert session_event_inbox(session_id, idx) on conflict do nothing
load active workflow_session_subscription rows
send DBOS wakeup to each subscribed workflow
advance session_event_ingest.last_ingested_idx
heartbeat lease
```

### Reconnect and recovery

If the stream disconnects, the pump reconnects with backoff:

```text
stream error
record last_error
sleep with bounded backoff
renew or reacquire lease
read last_ingested_idx
ListSessionEvents to backfill any gap
resume StreamSessionEvents
```

If the whole worker dies:

```text
lease_expires_at passes
another orchestrator-worker claims session_id
new worker starts from last_ingested_idx
ListSessionEvents backfills gaps
stream resumes
```

Duplicates are safe because `session_event_inbox` is keyed by
`(session_id, idx)`. A duplicate event insert becomes a no-op. Duplicate DBOS
wakeups are also safe because workflows drain by cursor from durable inbox
rows.

### Workflow consumption

The business workflow receives a wakeup:

```text
DBOS.recv("session-events")
```

Then it reads durable facts:

```text
cursor = workflow_session_cursor.last_processed_idx
events = session_event_inbox where session_id = ? and idx > cursor order by idx
process events
advance workflow_session_cursor
```

The wakeup can be lost, delayed, or duplicated without corrupting the workflow.
The inbox and cursor provide correctness. The wakeup only improves latency.

This also supports fanout:

```text
triage workflow cursor
branch judge cursor
UI projection cursor
debug/replay cursor
audit export cursor
```

All can read the same `session_event_inbox` facts with different cursors or
query patterns.

### Minimal worker pseudocode

The event pump can be implemented with ordinary orchestrator workers first. It
can use DBOS queues later, but conceptually it is independent:

```ts
async function runPumpManager(workerId: string) {
  while (true) {
    const sessions = await findSessionsWithActiveSubscriptions();

    for (const sessionId of sessions) {
      if (isAlreadyPumpingLocally(sessionId)) continue;

      const claimed = await tryClaimSessionIngestLease(sessionId, workerId);
      if (claimed) {
        startLocalPumpTask(sessionId, workerId);
      }
    }

    await stopLocalPumpsWithoutActiveSubscribers();
    await sleep(1000);
  }
}
```

Per-session pump:

```ts
async function pumpSession(sessionId: string, workerId: string) {
  const backoff = newBackoff();

  while (await renewSessionIngestLease(sessionId, workerId)) {
    try {
      let last = await loadLastIngestedIdx(sessionId);

      for await (const ev of controlPlane.listSessionEvents({ sessionId, afterIdx: last })) {
        await ingestAndWake(sessionId, ev);
        last = Number(ev.idx);
      }

      for await (const ev of controlPlane.streamSessionEvents({ sessionId, sinceIdx: last + 1 })) {
        await ingestAndWake(sessionId, ev);
        await updateLastIngestedIdx(sessionId, Number(ev.idx));
        await heartbeatSessionIngestLease(sessionId, workerId);
      }
    } catch (err) {
      await recordSessionIngestError(sessionId, err);
      await sleep(backoff.next());
    }
  }
}
```

Event handling:

```ts
async function ingestAndWake(sessionId: string, ev: SessionEvent) {
  const inserted = await insertSessionEventInbox(ev);
  if (!inserted) return;

  await updateLastIngestedIdx(sessionId, Number(ev.idx));

  const subscribers = await activeSubscriptions(sessionId);
  for (const sub of subscribers) {
    await DBOS.send(
      sub.workflowRunId,
      { sessionId, idx: Number(ev.idx) },
      "session-events",
      { idempotencyKey: `${sub.workflowRunId}:${sessionId}:${ev.idx}` },
    );
  }
}
```

The pump should stop ingesting when no active subscription remains, unless the
session is marked for audit, live UI viewing, or another projection. Historical
events remain in `session_event_inbox` according to retention policy.

### Control-plane APIs needed

For workflows, it is also useful to add a unary control-plane API:

```proto
rpc ListSessionEvents(ListSessionEventsRequest) returns (ListSessionEventsResponse);
```

Streaming is good for low-latency wakeups. A paginated list API is better for
catch-up, backfill, and workflow tests.

The streaming API should support:

```proto
rpc StreamSessionEvents(StreamSessionEventsRequest) returns (stream SessionEvent);
```

where the request includes `session_id` and `since_idx`. The list API should
return ordered events plus a cursor or `next_after_idx` so the pump can backfill
in bounded batches.

### Why this is not the product workflow

The bad shape is:

```text
Slack workflow opens StreamSessionEvents and waits forever.
```

That couples one business workflow to one live network stream. It makes fanout,
replay, backfill, debugging, and reconnect behavior harder.

The better shape is:

```text
shared pump owns stream ingestion
many workflows consume durable inbox facts
each workflow has its own cursor
```

If we later choose to make the pump DBOS-managed, it should be a separate
per-session ingest workflow that processes bounded chunks or bounded streaming
epochs. It should still write `session_event_inbox` and wake product workflows;
it should not become the Slack/task workflow itself.

## Full example: Slack-triggered implementation workflow

### Product flow

Slack user:

```text
@engram investigate why checkout fails in staging and fix it
```

Engram should:

1. Acknowledge the thread.
2. Spin up a triage session.
3. Let the triage agent ask follow-up questions in Slack.
4. Produce an implementation plan.
5. Ask for approval.
6. Implement the change, optionally using branch fanout.
7. Capture screenshots or video proof.
8. Post proof back to Slack.
9. Iterate on Slack feedback.
10. Open a PR or publish a patch when approved.

### Step 0: Slack webhook

HTTP handler, expressed as pseudocode:

```ts
async function slackWebhook(req: Request) {
  const event = verifySlackSignatureAndParse(req);

  await db.transaction(async (tx) => {
    await tx.insert(slackEventInbox).values({
      slackEventId: event.event_id,
      teamId: event.team_id,
      channelId: event.channel,
      threadTs: event.thread_ts ?? event.ts,
      userId: event.user,
      eventType: event.type,
      text: event.text,
      payloadJson: event,
    }).onConflictDoNothing();
  });

  const taskId = stableTaskIdForSlackThread(event.team_id, event.channel, event.thread_ts ?? event.ts);
  const workflowId = `task:${taskId}`;

  await startWorkflowOnce({
    workflowId,
    name: "slackIssueWorkflow",
    input: { taskId, initialSlackEventId: event.event_id },
  });

  await sendWorkflowMessage(workflowId, "slack-events", {
    slackEventId: event.event_id,
  }, {
    idempotencyKey: `slack:${event.event_id}:to:${workflowId}`,
  });

  return new Response("", { status: 200 });
}
```

The handler verifies, dedupes, starts the workflow idempotently, sends a
durable workflow wakeup, and returns quickly.

### Step 1: Workflow creates the task and triage session

```ts
class SlackIssueWorkflows {
  @DBOS.workflow()
  static async slackIssueWorkflow(input: {
    taskId: string;
    initialSlackEventId: string;
  }) {
    const task = await SlackIssueWorkflows.ensureTask(input);

    const triage = await SlackIssueWorkflows.createTriageSession(task.id);
    await SlackIssueWorkflows.subscribeWorkflowToSession({
      workflowRunId: currentWorkflowId(),
      taskId: task.id,
      sessionId: triage.sessionId,
      consumerName: "triage",
      startIdx: 0,
    });

    await SlackIssueWorkflows.postSlackAck(task.id);

    await SlackIssueWorkflows.triageLoop(task.id, triage.sessionId);
    await SlackIssueWorkflows.planApprovalLoop(task.id, triage.sessionId);
    const selectedSessionId = await SlackIssueWorkflows.implementationLoop(task.id, triage.sessionId);
    await SlackIssueWorkflows.publishLoop(task.id, selectedSessionId);
  }
}
```

`ensureTask`, `createTriageSession`, `subscribeWorkflowToSession`, and
`postSlackAck` are steps or DBOS transactions because they touch databases or
external systems.

Task source:

```json
{
  "kind": "slack_thread",
  "team_id": "T123",
  "channel_id": "C123",
  "thread_ts": "1710000000.000000"
}
```

Triage prompt:

```text
You are triaging a Slack-reported engineering issue.

You may ask the user follow-up questions by calling ask_user.
When you have enough information, call submit_plan with:
- summary
- suspected root cause
- implementation plan
- risk
- required validation

Do not post to Slack directly.
```

### Step 2: Agent asks the Slack user a question

The triage session emits:

```json
{
  "kind": "orchestrator_request",
  "request": "ask_user",
  "request_id": "req_ask_env",
  "payload": {
    "question": "Which staging checkout flow is failing?",
    "choices": ["guest checkout", "logged-in checkout", "both"]
  }
}
```

The control plane appends this to the session event log. The event pump inserts:

```text
session_event_inbox(session_id=triage, idx=42, kind=orchestrator_request, ...)
```

Then it wakes the workflow:

```text
DBOS.send(workflowID, {sessionId: triage, idx: 42}, "session-events")
```

The workflow drains events:

```ts
static async triageLoop(taskId: string, sessionId: string) {
  while (true) {
    await DBOS.recv("session-events", 300);
    const outcome = await SlackIssueWorkflows.processSessionEvents({
      taskId,
      sessionId,
      consumerName: "triage",
    });

    if (outcome.planSubmitted) return;
    if (outcome.waitingForUser) {
      await SlackIssueWorkflows.waitForSlackReplyAndForward(taskId, sessionId);
    }
  }
}
```

`processSessionEvents` transaction:

```ts
static async processSessionEvents(input: {
  taskId: string;
  sessionId: string;
  consumerName: string;
}) {
  return appDb.runTransaction(async (tx) => {
    const cursor = await loadCursor(tx, currentWorkflowId(), input.sessionId, input.consumerName);
    const events = await loadInboxEventsAfter(tx, input.sessionId, cursor.lastProcessedIdx, 100);

    let maxIdx = cursor.lastProcessedIdx;
    let waitingForUser = false;
    let planSubmitted = false;

    for (const ev of events) {
      if (ev.kind === "orchestrator_request" && ev.payload_json.request === "ask_user") {
        await insertTaskEvent(tx, input.taskId, "ask_user", ev.payload_json);
        await insertExternalEffect(tx, {
          idempotencyKey: `task:${input.taskId}:session:${input.sessionId}:event:${ev.idx}:slack-question`,
          provider: "slack",
          effectType: "post_message",
          requestJson: slackQuestionMessage(input.taskId, ev.payload_json),
        });
        waitingForUser = true;
      }

      if (ev.kind === "orchestrator_result" && ev.payload_json.request === "submit_plan") {
        await insertTaskArtifact(tx, input.taskId, "plan", ev.payload_json.payload);
        await insertExternalEffect(tx, {
          idempotencyKey: `task:${input.taskId}:session:${input.sessionId}:event:${ev.idx}:slack-plan`,
          provider: "slack",
          effectType: "post_message",
          requestJson: slackPlanMessage(input.taskId, ev.payload_json),
        });
        planSubmitted = true;
      }

      maxIdx = Number(ev.idx);
    }

    await updateCursor(tx, currentWorkflowId(), input.sessionId, input.consumerName, maxIdx);
    return { waitingForUser, planSubmitted };
  });
}
```

After the transaction records the effect, another step sends pending Slack
effects idempotently:

```ts
static async sendPendingSlackEffects(taskId: string) {
  const effects = await loadPendingEffects(taskId, "slack");
  for (const effect of effects) {
    await runDurableStep(
      () => sendSlackEffect(effect),
      { name: `sendSlack:${effect.idempotencyKey}`, retriesAllowed: true, maxAttempts: 8 },
    );
  }
}
```

### Step 3: User replies in Slack

Slack reply:

```text
logged-in checkout, after clicking Pay
```

Webhook behavior:

```text
verify signature
insert slack_event_inbox ON CONFLICT DO NOTHING
resolve team/channel/thread -> task.workflow_run_id
DBOS.send(workflowID, {slackEventId}, "slack-events")
```

Workflow receives and forwards:

```ts
static async waitForSlackReplyAndForward(taskId: string, sessionId: string) {
  while (true) {
    const msg = await DBOS.recv<{ slackEventId: string }>("slack-events", 3600);
    if (!msg) {
      await SlackIssueWorkflows.postTimeout(taskId);
      continue;
    }

    const reply = await SlackIssueWorkflows.loadSlackReply(msg.slackEventId);
    if (!reply) continue;

    await SlackIssueWorkflows.sendPromptToSession({
      sessionId,
      text: `The Slack user replied: ${reply.text}`,
    });
    return;
  }
}
```

This is the opposite direction:

```text
Slack -> inbox -> DBOS workflow -> SendPrompt(session)
```

### Step 4: Plan approval

The triage agent emits `submit_plan`. The workflow posts:

```text
Engram found a likely root cause.

Summary:
  Logged-in checkout fails after Pay because the session refresh cookie is
  stale after the payment redirect.

Plan:
  1. Reproduce in browser.
  2. Patch session refresh handling.
  3. Add regression test.
  4. Capture video proof.

[Approve] [Revise] [Cancel]
```

Button clicks are Slack webhooks. They enter `slack_event_inbox`, wake the
workflow, and the workflow either proceeds, sends a revision prompt, or cancels.

### Step 5: Implementation with branch fanout

Once approved:

```ts
static async implementationLoop(taskId: string, triageSessionId: string): Promise<string> {
  const checkpoint = await SlackIssueWorkflows.createCheckpoint(triageSessionId);

  const branches = await SlackIssueWorkflows.spawnBranches({
    taskId,
    sourceSessionId: triageSessionId,
    checkpointId: checkpoint.checkpointId,
    strategies: [
      { name: "minimal_fix", prompt: "Implement the smallest safe fix..." },
      { name: "test_first", prompt: "Write the regression test first..." },
      { name: "root_cause", prompt: "Investigate and fix the root cause..." },
    ],
  });

  for (const branch of branches) {
    await SlackIssueWorkflows.subscribeWorkflowToSession({
      workflowRunId: currentWorkflowId(),
      taskId,
      sessionId: branch.sessionId,
      consumerName: `branch:${branch.id}`,
      startIdx: 0,
    });
  }

  return await SlackIssueWorkflows.joinBranches(taskId, branches);
}
```

`spawnBranches` deterministic structure:

- workflow chooses strategy list in deterministic order;
- each branch creation is a named step;
- each inserted `task_branch` row references the child `session_id`;
- branch sessions are subscribed to the event pump.

Join policies:

```text
first_green:
  select first branch that reports required tests passed and acceptable diff size

best_score:
  wait for all branches or timeout, then run judge branch / scoring step

human_select:
  post branch cards to Slack and wait for a button click

fusion:
  select hunks from multiple branches, create a fusion session, validate
```

### Step 6: Screenshots and video proof

Implementation branch uses browser automation and emits artifacts:

```json
{
  "kind": "artifact_created",
  "payload": {
    "artifact_id": "art_video_123",
    "content_type": "video/mp4",
    "label": "checkout-flow-after-fix"
  }
}
```

Workflow event processing:

```text
insert task_artifact(kind=video, branch_id=selected)
insert external_effect(... slack post with artifact link ...)
advance cursor
send Slack effect idempotently
```

Slack message:

```text
Implemented candidate fix in branch minimal_fix.

Validation:
  - pnpm test checkout/session-refresh.test.ts: passed
  - pnpm test auth/session.test.ts: passed
  - browser checkout proof: video attached

[Looks good] [Request changes] [Open PR]
```

### Step 7: Iteration

If the user replies:

```text
Can you also test the mobile viewport?
```

The workflow routes the reply to the selected branch:

```text
SendPrompt(selected_session, "Slack feedback: Can you also test the mobile viewport?")
```

The selected session continues from its current state, produces new proof, and
the workflow posts the update.

If the requested change is risky, the workflow can fork again from the selected
branch checkpoint:

```text
selected branch -> checkpoint -> mobile_test branch
```

### Step 8: Publish

On approval:

```ts
static async publishLoop(taskId: string, selectedSessionId: string) {
  const patch = await SlackIssueWorkflows.exportPatch(selectedSessionId);
  const pr = await SlackIssueWorkflows.createOrUpdatePullRequest(taskId, patch);
  await SlackIssueWorkflows.postFinalSlackSummary(taskId, pr.url);
  await SlackIssueWorkflows.markTaskDone(taskId);
}
```

All publish operations are idempotent through `external_effect`.

## Branch mechanics in detail

### Parent checkpoint

Before fanout, create a named checkpoint:

```text
parent session: sess_parent
checkpoint: chk_reproduced_failure
```

The checkpoint should represent a useful boundary:

- repository checked out;
- dependencies installed;
- failing test reproduced;
- relevant logs/artifacts captured;
- agent context summarized.

### Fork sessions

For each strategy:

```text
ForkSession(
  source_session_id = sess_parent,
  checkpoint_id = chk_reproduced_failure,
  prompt = strategy prompt,
  harness_secret_id = user secret ref
)
```

Insert:

```text
task_branch(id=br_minimal, session_id=sess_child_1, ...)
task_session(role=branch, branch_id=br_minimal, session_id=sess_child_1, ...)
workflow_session_subscription(consumer_name=branch:br_minimal, ...)
workflow_session_cursor(last_processed_idx=0, ...)
```

### Branch result contract

Branches should finish with structured output:

```json
{
  "request": "submit_branch_result",
  "payload": {
    "status": "passed",
    "summary": "Fixed stale session refresh after payment redirect.",
    "tests": [
      {"command": "pnpm test checkout/session-refresh.test.ts", "status": "passed"}
    ],
    "changed_files": ["src/auth/session.ts", "tests/checkout/session-refresh.test.ts"],
    "risk": "low",
    "ready_for_review": true
  }
}
```

The orchestrator stores this in `task_branch.result_json` and updates branch
status. It should not infer all result state from freeform assistant text.

### Join outputs

Joining branches means selecting or synthesizing a code artifact, not merging
VM memory:

- selected branch session id;
- exported patch;
- test report;
- proof artifacts;
- optional PR branch.

Patch fusion creates a new branch:

```text
winner branch + selected hunks from sibling branches
  -> fusion session
  -> validate
  -> selected final patch
```

## Work queue and task detail UX

### Work queue

Primary surface after ADR 0039 should be a work queue:

```text
Title                              Source      Status          Branches      Updated
Fix checkout staging failure        Slack       waiting user    1 triage      2m ago
Repair PR #1842 CI                  GitHub      implementing    4 running     6m ago
Patch CVE in auth service           Dependabot  28/300 repos    swarm         1h ago
Investigate latency spike           Slack       reviewing       workroom      3h ago
```

### Task detail

Task detail should show:

```text
Intake -> Triage -> Plan -> Implement -> Verify -> Review -> Publish

Timeline
  Slack trigger
  Triage session created
  Agent asked follow-up
  User replied
  Plan submitted
  User approved
  Branches spawned
  Candidate selected
  Video proof posted

Branches
  branch board

Artifacts
  plan, patch, screenshots, video, logs

Conversation
  Slack thread mirror + agent messages
```

### Branch compare

Compare view:

```text
minimal_fix vs test_first

Files changed
  src/auth/session.ts                  both
  tests/checkout/session-refresh.ts    test_first only

Validation
  minimal_fix: checkout test passed, no regression test
  test_first: checkout test passed, regression test added

Risk
  minimal_fix: lower diff size
  test_first: better future protection

[Select minimal] [Select test_first] [Fuse: minimal code + test_first tests]
```

## Implementation roadmap

### Phase A: Task v2 and event inboxes

- Add task status/type expansion.
- Add `task_branch`, `task_event`, `task_artifact`.
- Add `slack_event_inbox`.
- Add `session_event_inbox`, `workflow_session_subscription`,
  `workflow_session_cursor`, and `session_event_ingest`.
- Add task detail timeline UI for existing chat tasks.

### Phase B: DBOS baseline

- Add DBOS to orchestrator package.
- Install DBOS schema and datasource migrations.
- Add workflow ID convention: `task:{task_id}`.
- Add idempotent workflow starts from webhooks.
- Add `external_effect` table and sender steps.
- Add workflow management UI/status fields.

### Phase C: Session event pump

- Implement active subscription registry.
- Implement `session_event_ingest` leases and worker ownership.
- Tail `StreamEvents` per active leased session.
- Insert inbox events idempotently.
- Wake workflows with `DBOS.send`.
- Add catch-up path with `ListSessionEvents`.
- Add reconnect/backoff and lease takeover after worker death.
- Add tests for crash/restart, duplicate events, duplicate wakeups, backfill,
  and cursor advancement.

### Phase D: Slack issue workflow

- Slack app install and connection storage.
- Slack signature verification.
- Slack thread to task mapping.
- Triage session creation.
- `ask_user` and `submit_plan` structured events.
- Slack approval buttons.
- SendPrompt replies back into the session.

### Phase E: Fork/checkpoint primitives

- Control-plane `CreateCheckpoint`.
- Control-plane `ForkSession`.
- Orchestrator branch spawning service.
- Branch board UI.
- Branch result structured event.
- First join policy: `human_select`.

### Phase F: CI repair swarm

- GitHub app/check webhook.
- Reproducer session.
- Checkpoint after reproduction.
- Strategy branches.
- First-green join policy.
- Export patch and create/update PR.

### Phase G: Proof artifacts and iteration

- Browser proof tools emit `artifact_created` events.
- Artifact posting to Slack/GitHub.
- "Request changes" routes feedback to selected session.
- Optional fusion branch.

## Open design questions

1. Should `session_event_inbox` store all events forever, or only events for
   sessions with active workflow subscriptions?
2. Should the event pump be a normal orchestrator process, DBOS queue worker, or
   its own deploy unit?
3. Do we need a control-plane unary `ListSessionEvents` API before shipping
   workflow consumption, or can we begin with `StreamEvents` plus replay?
4. What is the first branch join policy: human select, first green, or judge?
5. How do we expose branch cost budgets in the UI without making the product
   feel like infra?
6. What exact structured event schema should harnesses support, and how strict
   should validation be?

## References

- Ona public positioning around background agents, automations, connected
  environments, and guardrails:
  https://ona.com/
  https://ona.com/cases/background-agent
  https://ona.com/cases/automations
  https://ona.com/cases/ona-environments
  https://ona.com/cases/ona-guardrails
- Factory public docs and product positioning around Droids, headless execution,
  enterprise controls, sandboxes, and incident response:
  https://docs.factory.ai/
  https://docs.factory.ai/cli/droid-exec/overview
  https://docs.factory.ai/enterprise
  https://docs.factory.ai/cli/configuration/sandbox
  https://docs.factory.ai/cli/features/incident-response
- Fabro public docs around graph workflows, human gates, checkpoints, retries,
  observability, and execution environments:
  https://fabro.sh/
  https://docs.fabro.sh/core-concepts/how-fabro-works
  https://docs.fabro.sh/execution/environments
- DBOS workflows resume from the last completed step and support background
  workflow starts, workflow IDs, durable sleeps, and workflow guarantees:
  https://docs.dbos.dev/typescript/tutorials/workflow-tutorial
- DBOS steps are the boundary for nondeterministic work and external APIs:
  https://docs.dbos.dev/typescript/tutorials/step-tutorial
- DBOS workflow messages/events/streams support communicating with workflows:
  https://docs.dbos.dev/typescript/tutorials/workflow-communication
- DBOS datasource transactions can atomically commit app changes and a DBOS
  checkpoint:
  https://docs.dbos.dev/typescript/tutorials/transaction-tutorial
