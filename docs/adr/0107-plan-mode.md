# ADR 0107: Plan mode — a harness-agnostic read-only design pass with an approval gate

Status: 2026-07-31 — **Accepted.** Implemented end to end on PR #927 (one
commit per phase; see §Implementation record). Renumbered from 0106 after
PR #902 concurrently claimed that number (and `CreateSessionRequest`
field 12 — `harness_mode` moved to 13).

Builds on ADR 0089 (the generic tool protocol: registry, native bindings,
deferred session tools, two-phase completion), ADR 0063 (harness descriptors),
ADR 0034/0074/0091/0101 (idle eviction, parking, and honest lifecycle — a parked
plan approval must survive eviction), ADR 0073 (durable outbox delivery), and
ADR 0102 (automations — the headless policy below). Issue #905 is the driving
work item; it is the first target of the local-parity initiative (#905–#918).

## Context

Local Claude Code sessions use plan mode daily: 5.7% of mode-carrying transcript
records over six weeks, 21 saved plan documents (median ~13 KB), 39
`ExitPlanMode` calls. engrams has no plan mode: no permission mode on the
harness, no plan surface in the UI, and no way to hold an agent to a read-only
design pass before it edits. Users who want that discipline stay on the local
CLI.

The design constraint that shapes everything below: **plan mode must be
harness-agnostic.** The harness survey:

| Harness | Native plan support | Plan-completion signal | Mode-change cost |
| --- | --- | --- | --- |
| Claude Code | `--permission-mode plan` | `ExitPlanMode` tool call `{plan, planFilePath}` | argv change ⇒ process respawn with `--resume` |
| Codex | plan "collaboration mode"; per-turn `sandboxPolicy` override | none — the plan is the turn's final message | none (per-turn params) |
| OpenCode (survey) | plan agent with `edit/bash: deny` | agent switch by the user | agent switch |
| pi (survey) | deliberately none | none | n/a |

Two harnesses we ship have no common native mechanism, and custom harnesses may
have none at all. So engrams owns the mode, and each harness adapter maps it to
the best native mechanism it has.

### Spike findings (claude 2.1.185 — the bundle pin; codex-cli 0.144.3)

Run against the pinned CLI versions before this ADR was written:

1. `--permission-mode plan` works under `--print --input-format stream-json`.
   `ExitPlanMode` fires as a normal PreToolUse-hookable tool with input
   `{plan, planFilePath}`. The hook payload carries `permission_mode: "plan"`.
2. In plan mode the CLI writes the plan document to `~/.claude/plans/<slug>.md`
   (Write/Edit fires that plan mode itself permits). A hook backstop must keep
   these writes legal.
3. Hook verdict `defer` on ExitPlanMode ends the turn with
   `terminal_reason: "tool_deferred"` — identical to the AskUserQuestion park
   shape. The existing `Parked` emission and idle-eviction classification apply
   unchanged.
4. Hook verdict `deny` + reason on ExitPlanMode is surfaced to the model and
   keeps it planning in the same turn: it revises the plan and calls
   ExitPlanMode again. This is the rejection mechanism.
5. Resume WITHOUT the plan flag after a deferred ExitPlanMode does NOT re-fire
   the dangling call. The CLI self-initiates a continuation turn in mode `auto`
   and implements the plan with no user message at all. Consequences: the
   approve path must not depend on a re-fire, and an accidental plan-flag loss
   silently becomes an implementing session — the stamp file (below) is the
   single source of truth for argv, and the hook backstop covers the mismatch
   window.
6. Hook `allow` does NOT approve ExitPlanMode (the call still lands in
   `permission_denials`; upstream anthropics/claude-code#15755 agrees). Nothing
   can accidentally approve a plan through the hook.
7. Resume WITH the plan flag re-fires the dangling ExitPlanMode with the SAME
   `tool_use_id` — identical to the AskUserQuestion re-fire semantics. Answering
   the re-fire with `deny` + feedback keeps the model planning: it revises the
   plan and calls ExitPlanMode again under a fresh call id (which then defers
   and parks again). The reject path is exactly the proven AUQ shape.
   Resume WITHOUT the flag plus an injected "Your plan was approved — implement
   it." user message produces one clean implementation turn — no re-fire, no
   stray self-continuation. The approve path is confirmed end to end.
8. Codex (schema-verified via `codex app-server generate-json-schema`):
   `TurnStartParams.sandboxPolicy` accepts
   `{"type": "readOnly", "networkAccess": <bool>}` as a per-turn override. The
   override is sticky ("this turn and subsequent turns"), so the build turn
   after approval must explicitly restore the normal policy.
   `approvalPolicy` has the same per-turn override shape. Open item: codex
   `readOnly` engages codex's own OS sandbox (Landlock/seccomp on Linux); the
   vendored FC guest kernel (ADR 0025) must enable it, else codex plan turns
   fall back to the prompt + exit-tool contract only.

## Decision

Plan mode is two orthogonal mechanisms, deliberately separated:

1. **Mode transport** — which mode a session is in, and how a harness learns it.
2. **The plan gate** — how a finished plan reaches the user and how the
   decision comes back.

### 1. Mode is engrams-owned per-session state that rides prompts

There is no new RPC and no env mechanism. A single optional field,
`harness_mode`, is added to `SendPromptRequest`, `CreateSessionRequest`,
`CreateTaskRequest`, and `CreateTaskAutomationAction`. Create-time plan mode is
just mode on the first prompt.

> Naming: the field cannot be called `mode`. `CreateSessionRequest.mode = 2`
> and `Session.mode = 6` are already the agent/dev_vm `SessionMode`. Issue #905
> says "mode"; this ADR diverges deliberately.

The coordinator validates the value against the harness descriptor's declared
modes (unknown → BadRequest), appends a new `harness_mode_changed` session
event (no migration — `session_events.kind` has no CHECK constraint), and
carries the mode in the outbox Prompt payload. The wire command becomes:

```
HarnessCommand::Prompt { text, prompt_id, mode: Option<String> }
```

This is a positional-bincode wire break. It follows the sanctioned ADR 0089
P5d shape: regenerate the pinned goldens, bump nothing else, and deploy the
coordinator, host-agent, and both builtin harness bundles together. A stale
bundle fails attach loudly.

**Each harness latches the mode to `/workspace/.engrams/mode`** (shared helper
in `engram-harness-sdk`). Process env is immutable and reverts to create-time
values after idle-evict → resume, so a stamp file on the workspace disk — the
thing snapshots preserve — is the only durable latch. This is the same
mechanism as `claude-session-id`.

Harness descriptors declare which modes exist. `harness.toml` gains:

```toml
[[modes]]
id = "default"
default = true

[[modes]]
id = "plan"
label = "Plan"
```

`HarnessMode` is `{id, label, default}` — pure declaration, no env map. It
drives the create-surface Select and the composer chip visibility ("does this
harness support plan mode"), nothing else. A harness that declares no modes
never shows the affordance.

### 2. The plan gate is one deferred session tool

```
exit_plan_mode
  input:  { plan: string }                                   // markdown
  output: { decision: "approve" | "reject", feedback?: string }
  handling: "session", execution: "deferred"
  nativeBindings: { claude: "ExitPlanMode" }                  // no codex binding — deliberate
```

The absence of a codex binding is what makes the design uniform: the claude
harness only injects manifest tools without a claude native binding, and codex
routes every manifest tool without a codex binding through its generic
dynamic-tool park path. So codex and every future custom harness receive
`exit_plan_mode` as an injected tool with near-zero adapter code, while claude
intercepts its native `ExitPlanMode` — whose training the model already has.

Everything downstream is the unmodified ADR 0089 machinery: the
`pending_tool_calls` ledger, `CompleteToolCall` + the session-handling guard +
zod output validation, the durable `OutboxKind::ToolResult` row, and the
park → evict → wake path. A proposed plan parks the session; the session
idle-evicts on the normal soft TTL while waiting; the decision wakes it. Zero
new durability machinery.

### 3. Per-harness enforcement and delivery

| | Plan turns | Enforcement backstop | Approve | Reject |
| --- | --- | --- | --- | --- |
| claude | respawn `--resume` with `--permission-mode plan` when the stamp and the running process disagree (the clean ResumeForDeferred respawn path) | hook bridge: deny Write/Edit/MultiEdit/NotebookEdit while the fire says plan mode, EXCEPT plan-file paths; Bash gets no opinion instead of today's unconditional allow (an allow would override the CLI's own plan gating) | flip stamp → default, respawn `--resume`, inject the user turn "Your plan was approved — implement it.", emit an explicit `ToolCallCompleted` (the existing stashed-result drain machinery). No re-fire dependency — see spike finding 5. | keep stamp = plan, stash → respawn (still plan) → the re-fired ExitPlanMode is answered `deny` + reason = the feedback text — the CLI's native keep-planning affordance (spike finding 4) |
| codex | per-turn: stamp = plan ⇒ `start_turn` sends `sandboxPolicy: {type: "readOnly"}` and prefixes a harness-owned plan preamble ("read-only design pass; call exit_plan_mode with the complete markdown plan"). No respawn ever. | the turn sandbox itself; `exit_plan_mode` arriving while the stamp says otherwise gets an immediate protocol-error result, no park | respond to the parked JSON-RPC call with the decision, flip the stamp, end the plan turn, start a fresh full-access turn "Plan approved — implement it." (restoring the sticky sandbox override) | respond `{decision: "reject", feedback}`; the thread stays in plan mode |
| custom | descriptor declaration + injected tool + prompt contract | none — enforcement is advisory for harnesses that cannot restrict themselves; this ADR states that honestly | generic ToolResult delivery | generic ToolResult delivery |

### 4. Headless policy: auto-approve when ownerless

A deferred session tool in an ownerless session (automations — ADR 0102 creates
tasks with `createdByUserId = null`) parks forever today; only a 24-hour
observational watchdog notices. For plan mode the policy is: **the
tool-consumer auto-approves a pending `exit_plan_mode` when the session's task
has no owner**, by calling the internal completer with
`{decision: "approve"}`. The plan document still lands in `session_events` as a
durable, reviewable record — automations get a plan-then-implement pattern, not
a stall. Attended sessions always park and get the attention affordance below.
The watchdog's `onStale` wiring stays future work.

### 5. The user surface

Summarized here; the full UX spec lives with the web-tier phase.

- **PlanCard** in the thread (sibling of `UserQuestionCard`): the markdown plan
  in an internally scrolled region, decision bar always visible — "Approve &
  build" / "Request changes" with an inline feedback textarea. Resolved cards
  collapse to one-line receipts, so reject → revise cycles stay calm. A
  read-only WorkPane "Plan" tab renders the full document with a revision
  switcher.
- **Composer**: a `⊹ Plan` chip; mode rides the next SendPrompt. Faint thread
  markers narrate transitions ("planning — read-only", "plan approved —
  building").
- **Attention**: the contracted-but-never-set task status `awaiting_review`
  (task.proto, orchestrator schema) is set by the tool-consumer when a
  session-handled call parks an owned task, cleared on completion. The rail,
  session list, vitals, and tab title all key off it. This generalizes to
  `ask_user_question` by construction.
- **Slack**: notification-with-link only. There is no generic Slack presenter
  dispatcher today; interactive Slack approval is out of scope.

### 6. New vocabulary (complete list)

- `HarnessCommand::Prompt.mode: Option<String>` — wire break, flag-day.
- `SessionEvent::HarnessModeChanged` / kind `harness_mode_changed`.
- Proto: `harness_mode` on SendPromptRequest, CreateSessionRequest,
  CreateTaskRequest, CreateTaskAutomationAction; `HarnessDescriptor.modes`
  (`HarnessMode {id, label, default}`).
- Registry tool `exit_plan_mode`; web presenter `PlanCard`; stamp file
  `/workspace/.engrams/mode`; sdk helpers `read_mode_stamp`/`write_mode_stamp`.
- New harness-claude `HookVerdict::Deny { reason }` (bridge-internal NDJSON,
  not the platform wire).

## Rollout

Ten phases, one PR each, shippable in order: spike (done, findings above) →
this ADR → registry tool (inert) → descriptor modes → prompt-mode plumbing +
wire break (flag-day deploy) → claude behavior → codex + noop behavior → web →
attention → automations/CLI → stack e2e + ADR flip. The registry tool is inert
until a harness emits it; the interim window where codex could call an
uninjected-but-manifested tool is mitigated by the tool description and by
keeping the phases in one release train.

## Implementation record (2026-07-31, PR #927)

One commit per phase. As-built divergences from the proposal:

- **ADR renumber**: 0106 → 0107 (PR #902 collision; `harness_mode` is
  `CreateSessionRequest` field 13, not 12).
- **Claude approve delivery is engine-owned, not drain-owned**: the approve
  result is never stashed. `run_engine` keeps `pending_plan_approvals`
  across the respawn and the spawn bootstrap injects the build turn +
  explicit `ToolCallCompleted` — deterministic, and the CLI's spontaneous
  self-continuation (spike finding 5) never races a stale stash.
- **Bridge round-trip set is manifest-derived**: the hardcoded
  AskUserQuestion filter became "every `nativeBindings.claude` name in
  ENGRAM_TOOLS ∪ `mcp__engrams__*`", with the AUQ literal as the
  corrupt-env fallback. New bindings need no bridge change.
- **Codex build turn rides the queue**: approve answers the parked JSON-RPC
  call, emits the explicit completion, interrupts the read-only turn, and
  QUEUES the build prompt — `turn/completed` consumes it and `start_turn`
  re-reads the flipped stamp (the sticky `sandboxPolicy` override is
  re-sent every turn by design).
- **Codex REJECT also rides the queue** (found in live use, sessions
  fe3cd981 + 98111e00): the tool-result channel is not a steer for codex.
  Answering the parked call `success: true` with the raw decision JSON, and
  then answering it `success: false` with the reviewer's words in
  `contentItems`, BOTH made the model narrate "Plan submitted for review."
  and end the turn. A reject is now symmetric with an approve — answer the
  call, interrupt, and queue the feedback as a fresh user turn — with the
  stamp left at `plan`, so the revision turn is still read-only. Claude
  needs none of this: `deny` + reason is its native keep-planning verdict.
- **Attention is derived, not written**: task rows never store
  `awaiting_review`. The task list derives it at read time from the
  `pending_tool_calls` ledger (requested-but-unsubmitted session-handled
  call on a live primary session), so it can never go stale and it covers
  `ask_user_question` for free.
- **Headless policy simplified per review**: auto-approve keys on "the
  task exists and has no human creator" — no per-automation policy knob.
- **Coverage**: claude fake-engine tests (approve/reject argv + stamp
  trails, deterministic argv-branching fake), codex scripted-app-server
  tests (read-only plan turn params, outside-plan rejection), a noop
  duplex test of the full park → reject → revise → approve → build
  choreography (the harness-independent e2e), coordinator live-PG mode
  tests, sim/PG rewind conformance, web buildMessages/PlanCard/contract
  tests. A real-CLI stack e2e needs provider keys the CI lane does not
  have; the noop choreography + the Phase 0 spike record stand in.
- **Mode is a chip, not a keybinding**: Shift+Tab was proposed for CLI
  parity but it is the browser's reverse-focus key, so hijacking it cost
  keyboard navigation and did not read as a mode switch anyway. Mode now
  lives only in `ModeChip` — a lit toggle for one alternate mode, a menu
  for several — shared by the start screen and the session composer. The
  capability selectors (harness/model/effort) became quiet text buttons
  showing the value the launch will actually use ("Claude Opus 5", not
  "Default model"), which is the shape Codex and the Claude desktop app
  both converged on.

## Open questions

- Landlock availability in the FC guest kernel for codex `readOnly` turns
  (verify on the dev VM before leaning on the sandbox as the sole
  enforcement for codex).
- Whether the pinned claude 2.1.185 can move forward once the ADR 0054 AUQ
  regression is re-validated; plan mode does not depend on it either way.
- Interactive Slack plan approval (needs a generic Slack presenter
  dispatcher; notification-with-link is the current scope).
