# ADR 0054: Harness special messages — rich file-change rendering and interactive `AskUserQuestion`

Status: 2026-06-20 — **Accepted**. Phase 2 (interactive `AskUserQuestion`)
landed backend-first: `0541be9c` (proto wire types) · `39127d87`
(harness hook-bridge + socket + `ResumeForAnswer` + continuation turn) ·
`e68639ee` (host-agent `answer_question` + at-least-once replay) · `30b67f45`
(coordinator answer ingress + question event passthrough) · `89144922`
(orchestrator passthrough + regenerated stubs). Deferred to follow-on
branches: Phase 1 (`FileChanged`/`DiffToolPart`) and the web interactive
question component.

## Context

`engram-harness-claude` is a pure **observer** of Claude's `--output-format
stream-json` stream (ADR 0052): every `tool_use`/`tool_result` is flattened into
generic `HarnessEvent::ToolCallStarted`/`ToolCallCompleted` carrying a
human-readable `args_summary` clipped to **1 KB** (`MAX_ARGS_SUMMARY_BYTES`,
adapter-local enforcement of the wire doc), and the web renders them with a
single generic `ToolFallback` — the lone exception being `engram.shell`, which
the web *synthesizes* from native exec events and renders with a dedicated part.

Two product gaps follow from that uniformity:

1. **No diffs.** `Write`/`Edit`/`MultiEdit` render as opaque key/value blobs. The
   1 KB cap also *destroys* the data a diff needs (a file's content or a hunk
   blows past 1 KB), so it cannot be recovered downstream — any fix must touch
   the adapter.
2. **`AskUserQuestion` is dead.** Measured against `claude` 2.1.181 in our exact
   invocation (`--print --input-format stream-json --output-format stream-json
   --verbose --dangerously-skip-permissions`, stdin held open): when Claude calls
   `AskUserQuestion`, the CLI **does not wait** for an answer. ~3 s after emitting
   the `tool_use` it synthesizes its own `is_error` `tool_result`
   (`"Answer questions?"`), records the tool under `permission_denials`, and
   completes the turn. So today, if Claude asks a clarifying question inside an
   engram session, **the user never sees it and Claude silently gives up.**

We want a family of **special harness messages**: first-class, semantic,
harness-agnostic events the UI renders or *interacts with* richly — keeping the
"no per-agent decoder" property (consumers read structured fields, not agent
bytes). Two flavors emerged:

- **Render-rich (one-way):** the harness recognizes a tool and emits a typed
  event the UI renders specially. `FileChanged` is the first instance.
- **Interactive (round-trip):** the harness recognizes a tool that needs a human
  answer, surfaces it, and feeds the answer back into the agent.
  `AskUserQuestion` is the first instance.

## Decision

Introduce special harness messages with both flavors. Crucially — and contrary
to our first design pass — the interactive flavor is implemented with a
**`PreToolUse` hook**, *not* a reverse-engineered control protocol. The hook's
`updatedInput` is the same answer-delivery channel the SDK's `canUseTool` uses;
a hook reaches it with **zero** non-public protocol.

### Empirical findings (claude 2.1.181) this rests on

| # | Probe | Result |
|---|-------|--------|
| 1 | Held-open stdin, no responder | AUQ auto-denied (`is_error` "Answer questions?", `permission_denials:[AUQ]`); turn completes. The bare CLI never waits. |
| 2 | `PreToolUse` hook returns `allow` only | AUQ **still** denied. Approving permission ≠ answering. |
| 3 | `PreToolUse` hook returns `allow` + `updatedInput:{questions, answers}` | **AUQ answered.** `permission_denials:[]`; CLI emits a real `tool_result` ("Your questions have been answered…"); model continues ("You picked Blue"). |
| 4 | Blocking hook (`sleep 70`, hook `timeout: 600`) | Block honored (70.0 s); answer still accepted. |
| 5 | `PreToolUse` returns `permissionDecision:"defer"` | Turn ends `terminal_reason:"tool_deferred"` (pending, not denied). On `--resume`, the AUQ **re-fires** the hook and is answerable. |
| 6 | `multiSelect:true` question, answered with a JSON **array** of labels in `updatedInput.answers` | **Accepted** (`permission_denials:[]`); the CLI comma-joins the array into the `tool_result` (`…="Python,TypeScript"`) and the model reads both back. So an answer value is a label **string** (single-select) or a **`string[]`** (multi-select), keyed by the question text. |
| 7 | Env var set on the `claude` parent, read in the hook | **Inherited.** A hook logged `ENGRAM_HOOK_SOCK` = the value set on `claude`'s environment — so the per-session socket path reaches the hook with a static `--settings`/hook artifact. |
| 8 | One AUQ tool_use carrying **two** questions (one multiSelect, one single-select) | **One round-trip.** A single `PreToolUse` fire, one `tool_use_id`; `tool_input.questions` is the whole 2-element array. The CLI returns **one** answer object keyed by question text, **mixing value types** in the same map (`{"…languages…":["Python","Rust"], "…editor…":"VS Code"}`). So *N questions per call = one hook round-trip and one answer map* — never N separate invocations. Only the model firing several *distinct* AUQ tool_uses in a turn yields multiple `tool_use_id`s. |
| 9 | Defer an AUQ, then `--resume` the session and answer it | **The re-fire is `tool_use_id`-stable.** The deferred and re-fired hooks logged the *identical* id (`toolu_014Lf…` both times), and the resume's assistant stream emitted **no new `tool_use`** — `--resume` replays the *persisted pending* tool call, it does not re-infer. Run `completed`, answer accepted, `permission_denials:[]`. So one stable token correlates the whole defer → evict → answer → resume round trip, and re-emitted-question dedup is trivially by that id. |
| 10 | Re-fire with an **empty** prompt: `claude -p "" --resume <sid>` | **Re-fires cleanly, no engram-visible pollution.** Empty `-p` is accepted (no "prompt required"); the deferred hook re-fires (same id, third time). claude inserts a synthetic `"Continue from where you left off."` user turn, but it appears **only in claude's private transcript** — `grep` of the `stream-json` for it = 0; the sole `user` event on the stream is the deferred `tool_result` (the answer). So the harness (a stream-json observer) never sees it, and it never becomes a `session_event`. |

`PreToolUse` fires for `AskUserQuestion` (findings 2–6), the payload carries
`tool_name`, `tool_input.questions` (each `{question, header, multiSelect,
options:[{label, description}]}`), and `tool_use_id`, and `permissionDecision`
accepts `allow|deny|ask|defer`. Render-rich (`FileChanged`) needs none of this —
it is pure stream observation.

### Follow-up verification (claude 2.1.183 + first-party Agent SDK 0.3.183)

A second probe (claude 2.1.183) plus an audit of Anthropic's own
`@anthropic-ai/claude-agent-sdk` 0.3.183 type surface pin down the **resume** half
of the round trip — the part findings #9/#10 only exercised through discrete `-p`
invocations, not the harness's *persistent streaming* process. This is the seam
B1 (below) lives on, so it is now load-bearing.

| # | Probe | Result |
|---|-------|--------|
| 11 | Defer an AUQ, then write a new `user` message to the **live persistent** streaming process (no `--resume`) | **Re-inference, NOT replay.** The model proposes a *fresh* AUQ with a **new** `tool_use_id` (`toolu_01XQ…` → `toolu_01Am…`); the original deferred call is orphaned. The live process therefore **cannot** answer a deferred tool — the answer is keyed to the original id, which a new turn never reproduces. (A normal `Prompt` arriving while a question is pending does exactly this: it spawns a duplicate question, never an answer.) |
| 12 | Defer an AUQ, then **streaming-mode** `--resume <sid>` (the harness's *actual* respawn shape — **not** `-p ""`) | **Re-fires the deferred hook on process startup, `tool_use_id`-stable, with no stdin written.** The persisted pending call is re-presented to the hook before any user turn; finding #9 reproduces in streaming mode (so no `-p ""` trick is needed). Caveat: if the hook keeps replying `defer`, claude re-presents in a tight loop (~5×/s) — so a resume is only safe **with the answer already in hand** (always true on the `AnswerQuestion` path). |
| 13 | Audit the first-party Agent SDK (`@anthropic-ai/claude-agent-sdk` 0.3.183) type surface | Confirms the whole contract on supported surface: `HookPermissionDecision = allow\|deny\|ask\|defer` is **public-typed**; `PreToolUseHookSpecificOutput = {permissionDecision, updatedInput, …}` (our hook-bridge verbatim); `multiSelect` is **per-question**; a deferred tool surfaces on the result message as `deferred_tool_use:{id,name,input}` + `terminal_reason:"tool_deferred"`. **Decisive for the run model:** the SDK's live control protocol has **no verb to continue a deferred tool** (the only running-turn verbs are `interrupt`/`stop_task`), and `canUseTool`'s `PermissionResult` is `allow\|deny` only — it **cannot `defer`**. So the first-party SDK *also* continues a deferred tool only via a fresh `query({resume})` spawn. Our kill→respawn-with-`--resume` is the canonical path, not an engram limitation. |

Net for the cycle below: a deferred AUQ is answerable **only** through a `--resume`
re-spawn (#12), the live process must **not** be fed (#11), and that is exactly
what Anthropic's own SDK does (#13).

### Flavor A — render-rich: `FileChanged`

The adapter recognizes `Write`/`Edit`/`MultiEdit` in the normal stream and, on
the **successful** `tool_result` (truthful — never a phantom diff for a failed
edit), emits a `FileChanged` event correlated by `tool_use_id`. Normalized,
harness-agnostic schema (in `engram-harness-proto`):

```rust
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FileChange {
    Write { path: String, content: String },                 // → all-green block
    Edit  { path: String, hunks: Vec<EditHunk> },             // → red/green hunks
}
pub struct EditHunk { pub old: String, pub new: String }
```

Claude mapping: `Write → write{content}`; `Edit → edit{[{old_string,new_string}]}`;
`MultiEdit → edit{edits[…]}`. A dedicated byte budget (`MAX_FILE_CHANGE_BYTES`,
~64 KB) is truncated **per inner string before serializing** (never the JSON
blob). The 1 KB `args_summary` cap is untouched and stays for every other tool.

### Flavor B — interactive: `AskUserQuestion` via a `PreToolUse` hook

Replace `--dangerously-skip-permissions` (today hard-coded in `build_claude_argv`,
`main.rs:1201`) with a `PreToolUse` command hook, shipped in the guest and wired
via `--settings`. The hook is a **dumb bridge** — it makes no decisions; it
relays the harness's verdict:

- **ordinary tool** (`Bash`/`Write`/…) → `{ permissionDecision: "allow" }`. This
  *is* the bypass-replacement; the VM is still the safety boundary, but the
  decision is now explicit and auditable.
- **`AskUserQuestion`** → round-trip with the harness over a unix socket
  (below), then map the harness's verdict to its stdout.

The harness **always defers** `AskUserQuestion` unless it already holds the answer
(below) — it never blocks the hook. Deferring ends the turn cleanly so the VM can
idle-evict; the answer comes back later and re-drives the agent through the resume
path the system already has. engram is a density service and a question is
human-paced, so pinning a hot VM to wait on a click is exactly the wrong trade.

#### Hook ↔ harness socket contract

**Why a socket.** The runtime process tree is three deep —
`engram-harness-claude` → `claude` (spawned once, `main.rs:740`) → a *transient*
hook process spawned per `PreToolUse`. The hook's stdin (the tool payload) and
stdout (the permission decision) are both claimed by the CLI, so the harness —
the hook's grandparent — has no stdio path to it. A unix socket is the
out-of-band channel: the harness is a long-lived **server**, each hook
invocation a transient **client**.

**Bind-before-spawn + env injection.** The harness binds a `UnixListener` at a
per-session path in a tmpfs runtime dir (e.g. `/run/engram/hook.sock`) **before**
spawning claude, then adds one line to the existing `.env(...)` chain at
`main.rs:754`:

```rust
.env("ENGRAM_HOOK_SOCK", &sock_path)
```

claude inherits it; the hook inherits it from claude (**verified** against 2.1.181
— a `PreToolUse` hook logged `ENGRAM_HOOK_SOCK` set on the `claude` parent).
The hook reads `ENGRAM_HOOK_SOCK` to find the socket — so the path is dynamic
per-session while the **hook artifact stays baked into the image** (the harness
binary itself) and the **`--settings` file is generated at startup** from that
binary's own path (see Hook artifact). Binding before spawn means the socket
always exists before any hook can fire (no startup race).

**Accept loop, no parking.** The listener accepts concurrently and replies to each
hook in **one line**, after which the hook exits — connections are never held
open. One AUQ call can carry several questions (one connection, N questions —
finding #8), and the model can fire parallel AUQ tool_uses (several independent
fires, distinct `tool_use_id`s); each is handled on its own, keyed by its
`tool_use_id`.

**Wire = newline-delimited JSON, one line each way** (questions/answers are tiny;
no length-prefix framing):

```jsonc
// hook → harness (request)
{ "tool_use_id": "toolu_…",
  "questions": [ { "question": "…", "header": "…", "multiSelect": false,
                   "options": [ { "label": "…", "description": "…" } ] } ] }

// harness → hook (verdict) — one of:
{ "verdict": "answer",
  "answers": { "<question text>": ["<label>", …] } }   // canonical Vec<String>, always an array
{ "verdict": "defer" }
```

The socket carries the **canonical** `Answers` shape (always an array of labels,
matching the `BTreeMap<String, Vec<String>>` the rest of the system uses). The
**hook** does the claude-specific denormalization into `updatedInput.answers` — a
bare label string for single-select, an array for multiSelect (finding #6) — using
the `multiSelect` flag on the questions it already holds. The claude quirk stays
isolated in the (claude-specific) `hook-bridge`.

**The verdict: answer-in-hand, else defer.** The harness keeps one small in-memory
map, `answers_in_hand: HashMap<tool_use_id, Answers>`, populated only by an
`AnswerQuestion` command moments before it triggers a re-fire (below). On every
hook fire it decides in one line and the hook exits immediately:

- `tool_use_id` **in** `answers_in_hand` → `{verdict:"answer", answers}` + emit
  `HarnessEvent::QuestionAnswered{run_id, tool_call_id, answers}`.
- otherwise → `{verdict:"defer"}` + emit
  `HarnessEvent::UserQuestion{run_id, tool_call_id, questions}` (the "agent is
  asking" signal — the UI renders the card and marks the session awaiting-input).

**The full cycle.**

1. Agent calls `AskUserQuestion`. The hook reads its payload,
   `connect($ENGRAM_HOOK_SOCK)`, writes `{tool_use_id, questions}`, reads one line.
   No answer in hand → harness emits `UserQuestion{T}` and replies
   `{verdict:"defer"}`; the hook emits `permissionDecision:"defer"`; the turn ends
   `tool_deferred` (finding #5) and the VM becomes evictable. `UserQuestion{T}` is
   durable in the session event log, so the UI shows the question no matter what
   happens to the VM next.
2. The user answers in the web. `HarnessCommand::AnswerQuestion{tool_call_id: T,
   answers}` routes web → orchestrator → coordinator → host → harness over the
   **existing command channel**, delivered by the **same path as a prompt**
   (`send_prompt_core` → `ensure_active`, `api/prompt.rs:21`): if the session idled
   or evicted, `ensure_active` resumes it from the FC snapshot and the HOLD loop
   waits until the harness is ready, then forwards. Answering an idle session *is*
   prompting an idle session — **no new coordinator state, no buffering**.
3. The harness's `AnswerQuestion` arm — a new arm beside `Prompt` in the `cmd_rx`
   `select!` (`main.rs:931`) — stashes the answer, then triggers an **intentional
   re-spawn** of the persisted session. It must **not** write to the live process:
   feeding the live streaming process a turn re-*infers* a new `tool_use_id`
   (finding #11) and never answers the deferred call. There is no in-process
   continuation — not for engram, and not for the first-party SDK (finding #13) —
   so the only path is to end *this* `claude` and re-enter through the existing
   death→respawn-with-`--resume` loop (`run_engine`, `main.rs:484`):

   ```rust
   Some(HarnessCommand::AnswerQuestion { tool_call_id, answers }) => {
       // answers_in_hand is owned by `run_engine` (it outlives this process — see
       // below) and borrowed into run_claude_session like `pending`/`seen_prompt_ids`.
       answers_in_hand.insert(tool_call_id, answers);
       sigint_child(&child);                   // claude flushes its transcript per-message,
                                               // so the session stays cleanly --resume-able
       return SessionOutcome::ResumeForAnswer; // NEW outcome: respawn with --resume, but
                                               // do NOT trip fast-crash backoff and do NOT
                                               // emit an abnormal-exit System message
   }
   ```

   `ResumeForAnswer` is a sibling of `Respawn` so an intentional answer-resume is
   not counted as a crash (`main.rs:485`) and carries the "I am resuming to answer"
   intent into the next `run_claude_session`.

4. `run_engine` respawns `claude` in its normal **streaming** shape with `--resume`
   (`build_claude_argv`, unchanged — **not** a special `-p ""` invocation; finding
   #12 proves streaming-mode `--resume` re-fires on startup). Because a
   resume-for-answer is pending, `run_claude_session`'s startup does **not** go
   `Idle` and does **not** pop a queued prompt — it establishes a **continuation
   `TurnState`** (fresh `run_id`, `RunStarted` with no `prompt_id`/no user-echo) so
   the re-fired output is captured rather than dropped by the "line outside any
   turn" / "result with no in-flight turn" branches (`main.rs:907`, `:892`). claude
   re-presents the deferred AUQ to the hook on startup, **id-stable** (finding
   #9/#12). The harness finds `answers_in_hand[T]` → replies `{verdict:"answer",
   answers}` + emits `QuestionAnswered{T}`; the hook returns
   `{ "hookSpecificOutput": { "permissionDecision": "allow",
   "updatedInput": { questions, answers } } }`; claude synthesizes the `tool_result`
   and the continuation turn streams normally — the finding-#3/#6 path — closing on
   a `RunCompleted`. Because the answer is in hand on the **first** re-fire, the
   defer-loop spin (finding #12) never arms.

The harness emits a user-echo only on the `Prompt` path, never for
`AnswerQuestion`, so no synthetic user turn ever becomes an engram `session_event`.
(With streaming `--resume` the stream's sole `user` event is the deferred
`tool_result` itself; the `-p ""` `"Continue from where you left off."` transcript
quirk of finding #10 does not arise because the harness never uses `-p`.)

**No persistence, no parked state.** The durable "a question is outstanding" record
is the persisted claude session (it carries the pending tool call) *plus* the
`UserQuestion` event already in the log — both pre-exist. The coordinator buffers
nothing; `ensure_active` is the whole resume mechanism, already hardened for
prompts. The harness's only added state is the ephemeral `answers_in_hand` entry, owned by
`run_engine` (which outlives any single `claude` process). It is **set in the live
process's `AnswerQuestion` arm and consumed by the hook of the *next*, resumed
process**, so it must survive the respawn — findings #11/#12: the answer can only
land on the resumed re-fire, never on the live one. The stable `tool_use_id` is the
one correlation token across the whole round trip and makes re-emitted-question
dedup trivial (same id → the UI already has the card). (One unhappy-path
consequence: if the harness *process* itself dies between the stash and the
re-fire — host roll, OOM — the in-memory answer is lost; the durable
`UserQuestion` event keeps the card on screen and the user re-answers. The answer
is not yet on the at-least-once command-replay path; see Consequences.)

**Hook artifact.** Rather than ship a loose script, **re-invoke the harness
binary as its own hook** — the `--settings` `command` is just
`engram-harness-claude hook-bridge`. The same binary that runs the socket
*server* (normal mode) runs the transient hook *client* under this subcommand,
selected by `argv` (the `git`/`busybox` pattern). Three reasons over a loose
script: (1) **one artifact** — the harness is already baked into the guest, so
there is nothing extra to bake/version/sync; (2) **no extra guest runtime** — a
`.js` hook needs node, a `.sh` hook needs a shell + `jq`, the Rust binary needs
nothing; (3) **no protocol drift** — client and server share the same serde
types (`Question`/`Answers`/NDJSON framing) compiled once, so they can never be
a version apart. Socket path arrives via `ENGRAM_HOOK_SOCK` (finding #7). (The
research spike used `node …/auq_hook.js`; the contract above is
language-agnostic.)

**The `--settings` file is *written at startup*, not baked** (a refinement of
the original "baked + static" plan, landed in implementation). The hook
`command` must be the **absolute path of this binary** + `hook-bridge`, and
that path is *not* fixed — `resolve_claude_bin` shows the harness can live at a
pack sibling, `$ENGRAM_CLAUDE_BIN`, or a bare `$PATH` name across the FC / VZ /
Process backends. So `run_engine` generates the `--settings` JSON at startup
from `std::env::current_exe()` — one source of truth (the running binary's own
path), correct on every backend, with nothing extra to bake/version/sync. The
binary *is* the one baked artifact; deriving the settings from it is strictly
simpler than baking a second file that would hardcode a path wrong in dev.

### Wire surface (all trailing; `wire_golden` regen, graceful degrade)

`HarnessEvent` (up): `FileChanged { run_id, tool_call_id, path, change }`,
`UserQuestion { run_id, tool_call_id, questions: Vec<Question> }`,
`QuestionAnswered { run_id, tool_call_id, answers: Answers }`.
`HarnessCommand` (down): `AnswerQuestion { tool_call_id, answers: Answers }`.

These are **append-only**: bincode keys enum variants by source position, so the
events append after `AgentMessageChunk` and the command after `DequeueQueued` —
never interleaved. **Indices follow landing order** (Phase 2 shipped before
Phase 1): `UserQuestion` = event 11, `QuestionAnswered` = event 12,
`AnswerQuestion` = command 7. `FileChanged` takes event 13 whenever Phase 1
lands (a clean trailing append — no placeholder variant reserved). Each also
needs a `kind()`
string (e.g. `"file_changed"`, `"user_question"`, `"question_answered"`) and the
`UserQuestion`/`QuestionAnswered`/`FileChanged` arms wired into `tool_call_id()`.
`engram-harness-proto/tests/wire_golden.rs` pins both the bytes and the `u32`
variant index for every variant; the new ones get `assert_golden` +
`assert_variant_index` rows, regenerated via `--ignored regen_golden`.

The interactive types mirror the AUQ tool schema (finding #6):

```rust
pub struct Question {
    pub question: String,
    pub header: String,                       // short chip/tag label (≤~12 chars), presentational
    pub multi_select: bool,                   // single- vs multi-select lives HERE, not in the answer
    pub options: Vec<QuestionOption>,         // { label, description }
}
pub struct QuestionOption { pub label: String, pub description: String }

// An answer is always the list of selected labels. Single-select = a
// 1-element vec; the question's `multi_select` carries the arity, so the
// answer never needs to. Keyed by question text (finding #8).
pub type Answers = BTreeMap<String, Vec<String>>;
```

**Why `Vec<String>`, not an untagged `One|Many` enum.** `HarnessEvent`/
`HarnessCommand` cross the `engram-harness-proto` wire as **bincode 1.x**, which
is positional and *not* self-describing — it has no `deserialize_any`, so
`#[serde(untagged)]` (and `flatten`, and internally-tagged enums) **panic at
decode**. The whole workspace has zero untagged enums for this reason, and
`engram-harness-proto/tests/wire_golden.rs` pins it. A uniform `Vec<String>` is
bincode-safe, JSON-clean (`["VS Code"]`), and golden-testable; the single-vs-multi
distinction is recovered from the paired `Question.multi_select`. The `hook-bridge`
denormalizes back to the CLI's `updatedInput.answers` shape — a bare label string
for single-select, an array for multiSelect (finding #6) — using that flag.

**`tool_call_id` (= Claude's `tool_use_id`) is the single correlation token** for
both flavors — render-rich from the stream, interactive from the hook payload.
No new id space; no control `request_id` (the control protocol is not used).

### Layers

- **`engram-harness-proto`** — `FileChange`/`EditHunk` and `Question`/
  `QuestionOption`/`Answers` schema; the new (trailing) events/command + their
  `kind()`/`tool_call_id()`/`wire_golden` rows.
- **`engram-harness-claude`** — `FileChange::from_claude` + emit on successful
  result; the `UnixListener` + always-defer verdict (`answers_in_hand` bridge); the
  `AnswerQuestion` arm that stashes the answer and triggers a `claude --resume`
  re-fire; `UserQuestion`/`QuestionAnswered` emission; the `hook-bridge` subcommand;
  `ENGRAM_HOOK_SOCK` + `--settings` in `build_claude_argv`; drop
  `--dangerously-skip-permissions`.
- **coordinator** — passthrough `SessionEvent` variants (`#[serde(default)]`,
  JSONB payload → **no migration**); `AnswerQuestion` rides the existing command
  channel and the existing `ensure_active` resume-on-deliver path (`api/prompt.rs`)
  — no new state, no buffering.
- **web** — `DiffToolPart` (red/green for `edit`, all-green for `write`) and an
  interactive question component whose answer routes
  web → orchestrator → coordinator → host → harness.

## Alternatives considered

- **`canUseTool` over the CLI control protocol.** Was the original plan. Rejected
  for two reasons, the second decisive. (1) It requires reverse-engineering a
  non-public frame layout and an `initialize` handshake, when a `PreToolUse` hook
  reaches the identical `updatedInput` answer channel with public, documented
  surface (finding #3). (2) **The control protocol cannot do what we need anyway**
  (finding #13): in the first-party SDK, `canUseTool`'s `PermissionResult` is
  `allow|deny` only — it **cannot `defer`** (defer is a `PreToolUse`-hook-only
  decision) — and the live control connection exposes **no verb to continue a
  deferred tool** (only `interrupt`/`stop_task`). So even a full control-protocol
  client would still have to defer via the hook and resume via a re-spawn. The hook
  + `--resume` path is therefore not a lower-risk shortcut — it is the *same*
  architecture Anthropic's own Agent SDK uses, minus a protocol we don't need.
- **Smuggle a diff into `args_summary`** (no wire change) — viable for diffs but
  overloads the field, sniffs JSON shape in the web, and doesn't generalize to a
  semantic event family. Rejected in favor of concrete, named events that match
  how `HarnessEvent` already models facts.
- **`agent_payload: Option<Vec<u8>>`** (ADR 0002's escape hatch) — a generic
  opaque envelope. Rejected: opaque bytes serialize badly toward a browser
  (base64, un-decodable in JS), and a generic catch-all blurs "what can the
  harness send." Concrete named events fit the codebase grain better.
- **Answer AUQ via `PostToolUse` `updatedToolOutput`** — plausible hooks-only
  alternative, but a denied AUQ may not reach `PostToolUse`, and the answer is
  designed to ride the *input*. The `PreToolUse` + `updatedInput` path is proven
  (finding #3), so we take it.

## Consequences / risks

- **Invocation change:** the harness drops `--dangerously-skip-permissions` for a
  `--settings` `PreToolUse` auto-allow hook. The safety model is unchanged (VM is
  the boundary); the permission decision is now explicit and auditable.
- **Older baked images without the hook** simply don't surface questions — Claude
  auto-denies as it does today (graceful; no regression). New wire variants are
  trailing, so a new host decoding an old harness's stream is unaffected.
- **Answer delivery & idempotency:** *Idempotency on duplicate delivery is
  structural and free* — the deferred tool yields exactly **one** `tool_result`, so
  the first resume that finds the answer consumes the pending call and a duplicate
  `AnswerQuestion` triggers at worst a no-op resume (nothing pending to re-fire); no
  dedup set needed. *At-least-once delivery* is now also **provided** (the
  preferred option — reliability is non-negotiable): the host's command-replay
  buffer gained an `undelivered_answers` set alongside `undelivered_prompts`
  (`engram-host-agent`), recorded on `answer_question` and retired on
  `QuestionAnswered{tool_call_id}`, so a dropped `AnswerQuestion` is re-delivered
  on the next reattach. The ephemeral `answers_in_hand` still doesn't survive a
  *harness-process* crash between stash and re-fire, but the replay buffer
  re-sends the answer (which re-stashes it) and the durable `UserQuestion` card
  keeps the question on screen as the final backstop.
- **Echoed result / double-render:** the CLI writes the AUQ `tool_use`/
  `tool_result` into the normal stream; the web dedups it against the
  `UserQuestion`/`QuestionAnswered` it already rendered, keyed by `tool_call_id`.
- **Always defer:** the harness never blocks the hook — an unanswered AUQ defers
  immediately, so there's no block-window/hook-`timeout` tuning and no risk of a
  timed-out hook becoming a denial. The cost is that even a fast answer pays one
  resume cycle; acceptable because the wait is human-paced and `ensure_active` is
  already the hot path.
- **Truncation** of large diffs is surfaced in the UI (no silent caps); MultiEdit
  overflow clips trailing hunks and logs the count dropped.

## Implementation

The protocol spike is **complete** (pinned against `claude` 2.1.181, with a
follow-up on 2.1.183 + the first-party Agent SDK 0.3.183); its thirteen findings
*are* the [Decision](#empirical-findings-claude-21181-this-rests-on) above and the
socket contract under [Flavor B](#hook--harness-socket-contract), so there are no
protocol unknowns left to resolve. Two build phases remain — kept
separate because Phase 1 ships value with **no invocation change**, while Phase 2
changes the run model (always-defer/resume) *and* the safety posture (drops
`--dangerously-skip-permissions`). One logical change each.

- **Phase 1 — render-rich `FileChanged`.** `FileChange` schema + adapter mapping +
  `DiffToolPart`. No invocation change, no hook, leaves
  `--dangerously-skip-permissions` untouched. Self-contained; ships diffs.
- **Phase 2 — interactive `AskUserQuestion`.** The `PreToolUse` hook + out-of-band
  socket (`ENGRAM_HOOK_SOCK` injection at `main.rs:754`, bind-before-spawn);
  `UserQuestion`/`AnswerQuestion`/`QuestionAnswered`; the `AnswerQuestion` arm
  (stash answer into the `run_engine`-owned `answers_in_hand`, then `sigint_child`
  + return the new `SessionOutcome::ResumeForAnswer` — re-using the existing
  streaming `--resume` respawn, findings #11–13) beside `Prompt` at `main.rs:931`,
  plus the continuation-`TurnState` startup branch in `run_claude_session`; the
  host-agent `answer_question` + `undelivered_answers` replay; the coordinator
  answer ingress + `UserQuestion`/`QuestionAnswered` event passthrough; the
  orchestrator passthrough; drop `--dangerously-skip-permissions`. The `--settings`
  JSON is **generated at startup** from `current_exe()` (see Hook artifact), not
  baked. Landed backend-first; the **web question component is a follow-on branch**
  (so the echoed-AUQ dedup and the interactive card are not yet shipped).

### Tests (must run in CI)

This work is wire-protocol-shaped, so the tests are load-bearing, not optional —
several findings above (the bincode/untagged trap, append-only ordering, the
multi-question map) are exactly the kind of thing that round-trips green in one
build and desyncs a peer in the next.

- **`engram-harness-proto/tests/wire_golden.rs`** (extend) — `assert_golden` +
  `assert_variant_index` rows for `FileChanged`, `UserQuestion`,
  `QuestionAnswered` (events 11–13) and `AnswerQuestion` (command 7). Include a
  sample with a **mixed `Answers` map** (a single-element vec *and* a multi-element
  vec) so a regression to an untagged/`deserialize_any` encoding fails here loudly.
- **`engram-harness-claude` unit/integration** — `FileChange::from_claude` for
  Write / Edit / MultiEdit (incl. the >64 KB truncation path); a **socket
  round-trip** test that binds the listener and connects a fake hook client:
  with no answer in hand the verdict is `defer` and a `UserQuestion` is emitted;
  after an `AnswerQuestion` stashes the answer, a re-fired hook for the same
  `tool_use_id` gets `answer` + a `QuestionAnswered`; the **multi-question** case
  (N=2, one multiSelect + one single) → one request, one answers map; and
  idempotent re-delivery (duplicate `AnswerQuestion` → at-most-one `tool_result`).
- **e2e (`test-e2e-stack`), *if feasible in CI*** — the only lane that exercises
  web → orchestrator → coordinator → host → harness end to end. Target: a session
  whose agent (a) Writes/Edits a file → assert a `FileChanged` event surfaces and
  renders as a diff, and (b) calls `AskUserQuestion` → assert the `UserQuestion`
  event surfaces, an injected answer reaches the agent, and the run unblocks.
  **Caveat:** true e2e needs the baked `hook-bridge` + `--settings` in the guest
  image, so it lands with Phase 2's rebake; and AUQ depends on the *model* choosing
  to emit the tool (nondeterministic, costs tokens). Where that's too flaky/costly
  for CI, fall back to a **harness-level integration test** that feeds canned
  `--output-format stream-json` and runs a real hook subprocess against the live
  socket — deterministic, no model call — and keep the full-stack e2e as a
  manual/nightly smoke. Do not silently drop the e2e: if it can't run in CI, say so
  here and land the harness-level substitute in the same change.
- **web** — `buildMessages` + `DiffToolPart` (red/green vs all-green) and the
  interactive question component (single + multi) render tests.
