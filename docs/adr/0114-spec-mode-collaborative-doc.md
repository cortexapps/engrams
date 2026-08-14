# ADR 0114: Spec mode — the collaborative document substrate

Status: Accepted (2026-08-10)

Spec mode v2 revision: Proposed (2026-08-13). The document substrate,
projection, write fence, and SDK turn-context seam remain Accepted. The v2
decision set replaces the section states, spec lifecycle, template stages,
agent tool surface, digest content, publish gate, and owner-only conversation
model.

Date: 2026-08-08

Implementation record: PR #1134 (`1dc41974`) recorded the decision. PRs #1139
(`b8f8cdda`), #1141 (`04eee868`), #1140 (`39249797`), #1143 (`2603b160`),
and #1144 (`557f877d`) implemented D1-D12 in dependency order. Before
acceptance, PR #1143 changed D8-D9 to use a static, harness-neutral system
instruction. The server remains the source of truth and repairs direct edits,
but turn-start freshness now depends on agent compliance with the instruction.

**Related:** ADR 0113 (the `WriteFile`/`ReadFile` substrate this projection is
built on, and the `coordination_operation` idempotency ledger it copies) · ADR
0089 (the generic tool protocol that carries every agent edit) · ADR 0103
(durable exec, the publish leg) · ADR 0051 (the transport map that keeps the
event feed on SSE and byte relays on WebSocket) · ADR 0034 (intent in Postgres,
a scanner drives it — never a pipeline inside a request future) · ADR 0026 (the
artifact store that serves a published spec) · ADR 0007 and ADR 0110 (the disk
durability the projection does *not* depend on).

Terms used in this document:

- **Spec** — one tech-spec document, owned by one spec session, listed on the
  Tech Specs page.
- **Doc** — the live collaborative document: a Yjs CRDT held by the
  orchestrator and persisted in its Postgres.
- **Projection** — a read-only markdown render of the doc, published to the
  guest file system so the agent can read the spec with ordinary file tools.
- **Rev** — a monotonic semantic document revision per spec. Agent tools read
  and compare this value. Cache-only Yjs updates do not change it.
- **Doc seq** — the dense transport order for all durable Yjs updates,
  including cache-only updates. Projections keep this cursor for replay and
  digest work.
- **Section** — a template-defined part of the spec. A section is a document
  node with a stable identity, not a heading that matches by text.
- **Digest** — the bounded turn context for the spec. It includes recent human
  edits, the phase, section states and attribution, presence, and open-question
  counts.

## Summary, in plain English

A spec is one live document that several people and one agent write together.
The document lives in the orchestrator, not in the microVM, so a person keeps
typing while the sandbox is evicted, and nothing that reaches the server is
lost when a pod restarts.

The spec also has one shared conversation. Each human turn has an author. The
agent receives that attribution and can reconcile claims from different
people.

People edit through a WYSIWYG canvas over a WebSocket. The agent edits through
tools, not through the file system: it calls `spec_update_section` and the
server applies the change to the document. The agent still *reads* the spec as
an ordinary markdown file, because reading a file is how an agent naturally
works — but that file is a read-only render, and the server republishes it as
the document changes.

This split is the whole design. It gives every agent edit a structure the UI
can render, it removes the need to merge markdown back into a CRDT, and it
keeps the coordinator out of the feature: this ADR adds no coordinator code, no
wire version, and no guest-agent change.

## Context

### What the product needs

Spec mode is a session mode for writing tech specs. The requirements that shape
this ADR:

- The spec is a collaborative document, durable independent of any VM (R8, N2).
- Several people and the agent edit at the same time, with presence rather than
  locks (R11, R57, R58).
- Human edits reach the agent as per-section, per-author events at its next
  turn; the agent never overwrites them (R12).
- Sections, open questions and diagram blocks keep their anchors under
  concurrent editing (R15).
- Checkpoints carry human-readable labels; restore is a forward edit; publish
  pins one immutable version (R13, R14, R36).
- Propagation is sub-second in both directions when the session is live (N1,
  N9); document state is consistent after resume, eviction and host roll (N3);
  the agent's view is never staler than the start of its turn (N10).

### What the platform already gives us

- **Tools.** ADR 0089 defines the tool seam: a registry in the orchestrator, a
  manifest injected as `ENGRAM_TOOLS`, an in-guest MCP bridge, and a durable
  round trip that survives idle eviction. ADR 0113 added seven session
  coordination tools on that seam, so the pattern is proven at this size.
- **Files.** ADR 0113 replaced the old unary `WriteFiles` with a streaming
  `WriteFile`, added `ReadFile`, and made both authenticated and bounded in
  memory. Two of its properties decide this design:
  - `WriteFile` is **create-only**. "A retry with the same path, length, and
    digest succeeds without changing the file. Different content at an existing
    path fails." It is an upload primitive, not a replace primitive.
  - `ReadFile` puts `{size_bytes, sha256}` in the **first frame**, before any
    bytes. A caller that reads one frame and cancels learns the digest of a
    guest file without transferring it.
- **Durable exec.** ADR 0103 gives `runExec(command, {execId})` with a stable
  ticket, so a replayed step attaches instead of running twice.
- **Postgres patterns.** Enqueue and let a scanner drive it (ADR 0034);
  `NOTIFY` is a wake and the log is the truth (the outbox); an idempotency
  ledger keyed by caller, operation and key (ADR 0113).

### What the platform does not have

- No CRDT, no presence, no shared-document code anywhere.
- No editor library in the web tier, and no mermaid renderer.
- No file watcher in the guest. `HarnessEvent::FileChanged` fires only for the
  agent's `Write`, `Edit` and `MultiEdit` tool results, so it cannot see a
  change written by `sed`, `python` or a shell redirect.
- No org-shared live surface. Every session surface today is owner-only.

## Decision

### D1. The engine is Yjs, and it runs in the orchestrator

The document is a Yjs CRDT. The orchestrator hosts it, because the orchestrator
already owns product state (tasks, reviews, artifacts, the ADR 0113 task tree)
and is the tier where the renderer, the digest and the checkpoint compaction
must run.

Yjs also has the only production-grade ProseMirror binding, which D2 and the
editor choice depend on.

### D2. The document is a section list, not free markdown

The document is a ProseMirror document whose top level is `section+`. A section
node carries a stable `id` attribute, assigned when the template is
instantiated.

```
doc      → section+
section  → sectionHeading block+     // id: string, template_section_key: string
```

Anchoring is therefore node identity, not heading text. A person can retitle a
heading and every anchor survives (R15). The section heading renders as an H2
in markdown, but in the schema it is a boundary: the node is `isolating`, and a
`filterTransaction` plugin refuses transactions that delete, split or merge
sections. The template owns the section list; the canvas cannot change it (R9).

Open questions and diagram blocks are nodes with their own stable ids, so they
keep position under concurrent edits.

Requirement and non-requirement identifiers are also stable. All accepted
browser and tool mutations of the Requirements section run the shared
`validateRequirementEdit` check before the server stores a Yjs update. The
check rejects removed identifiers, revived tombstones and identifiers that do
not use the next value for their prefix.

Each section has one state: `open`, `proposed`, `settled`, or `n/a`. The agent
can propose content, but only a person's explicit word can settle it.
The agent does not infer settlement from silence or from nearby edits. The
**Drop** action moves a `proposed` section back to `open`. The UI calls the
`n/a` action **Not this spec**.

### D3. All spec state lives in the orchestrator's Postgres

New tables, in the orchestrator schema:

| Table | Holds |
| --- | --- |
| `spec` | one row per spec: title, template id, owner, phase (`ideation`/`drafting`/`published`), published checkpoint id, current transport sequence, current semantic revision |
| `spec_template` | sections (title, guidance, done criteria, required, n/a allowed) |
| `spec_update_log` | append-only Yjs updates, keyed by spec id and dense transport sequence; each row also records the resulting semantic revision |
| `spec_snapshot` | compacted document state plus the transport sequence and semantic revision it covers |
| `spec_checkpoint` | pinned history: state, state vector, rendered markdown, label, author |
| `spec_section_state` | per-section state, the reason for `n/a`, `settled_by`, and the state-change time |
| `spec_transcript_action` | durable, idempotent transcript actions with `actor_user_id` attribution |
| `spec_open_question` | id, section id, text, author, resolution, and `resolved_by` attribution |
| `spec_chat_message` | clean human conversation turns, keyed by prompt id, with an author snapshot and creation time |
| `spec_participant` | Yjs client id to user identity, per connection epoch |
| `spec_projection` | the push ledger: rev, doc seq, sha256, staging path, state |
| `spec_ticket_draft` | proposed tickets, backlinks, sync state |

These tables and columns use orchestrator migrations. `spec_template` has no
`stage_flags` column. The phase rename, stage removal, conversation store, and
durable attribution do not change the coordinator schema.

**This feature adds no coordinator schema or product code.** It consumes
exactly three existing coordinator verbs — `WriteFile`, `ReadFile` and durable
exec — and adds no `SessionEvent` variant, no `HarnessEvent` variant, no agentd
request and no `WIRE_VERSION` bump. Treat that as an invariant: a change to
this feature that needs a coordinator change is a signal that the boundary
moved, and it needs an amendment here.

### D4. Sync is a dedicated WebSocket route

`/api/v1/specs/:id/sync` carries the Yjs sync protocol and awareness. It mounts
on the `UpgradeHook` seam in `orchestrator/src/server.ts`, the same way the IDE
and preview routes do, and it keeps the rejection convention of closing with
`4000 + status`.

The route is dedicated and authenticated. It does not go on the generic Connect
pass-through surface, for the reason ADR 0113 kept `WriteFile` and `ReadFile`
off it: these are not product RPCs with uniform authz, they are byte channels
with their own guard.

A spec session has two independent streams. The document uses this socket.
The shared conversation uses the member-gated spec event stream in D14.

### D5. Postgres is the update bus between replicas

The orchestrator runs two replicas. A spec room is **not** owned by one pod.

Each `spec` row owns `current_doc_seq`, a dense per-spec committed transport
sequence. It also owns `current_semantic_doc_seq`, which changes only when the
source document changes. A render-cache update stays durable in the Yjs log but
does not invalidate an agent revision or create a new projection. On each
document update from a client:

1. In one transaction, conditionally increment `current_doc_seq` from the
   service's last applied transport sequence. Increment
   `current_semantic_doc_seq` only for a semantic change, and insert the update
   with both resulting values.
2. If the comparison fails, apply the missing log tail and retry.
3. Broadcast to local sockets.
4. Send a typed update envelope through `pg_notify('spec_update', ...)`.

A peer pod wakes on the notification and applies every row with
`seq > last_applied`. The notification is a wake; the log is the truth. A missed
notification costs latency, not correctness.

The spec-row update serializes sequence assignment and log insertion. A
transaction rollback does not consume a sequence, and sequence N cannot commit
before sequence N-1. Thus `last_applied` is a committed watermark, not a raw
Postgres sequence high-water mark. Compaction locks the same spec row, confirms
that the candidate covers both current values, then writes the snapshot and
deletes its covered tail in one transaction. It cannot delete an update that
the snapshot did not apply.

Persist before acknowledge is what makes N2 true: a pod that dies after
acknowledging has already made the keystroke durable, and a pod that dies before
acknowledging leaves the client's own unacknowledged update to be resent by the
Yjs sync protocol on reconnect.

Yjs updates are commutative, associative and idempotent, so two pods serving one
room converge. A single-owner design would be worse, not better: lease
expiry and takeover races would each become a split brain in which the losing
pod holds updates the winner never saw.

Leases still exist, for side-effectful singleton work only — publishing a
projection, computing a digest, compacting the log. That work is session-scoped,
so it rides the **existing** per-session listener lease rather than a second
lease of its own.

### D6. The agent writes through tools, and only through tools

The agent mutates the document with `spec_*` tools registered on the ADR 0089
seam, following the shapes in `orchestrator/src/tools/coordination.ts`:

| Tool | Purpose |
| --- | --- |
| `spec_read` | read the whole doc or one section, live |
| `spec_update_section` | replace a section's content |
| `spec_set_section_state` | set `open`, `proposed`, `settled`, or `n/a`; `n/a` also takes a reason |
| `spec_add_open_question`, `spec_resolve_open_question` | the question ledger |
| `spec_update_block` | a diagram block's source spec |
| `spec_gap_check` | inspect the spec for failure cases and missing decisions |
| `spec_propose_tickets` | the ticket tree, after publish |

Alternatives are prose in the **Alternatives considered** section. Exploratory
notes stay in the shared conversation. They do not have separate tools or
panels.

`spec_gap_check` is an internal instrument. The agent turns each finding into
a `spec_add_open_question` call in the applicable section. The UI calls this
work **look for what breaks**. It does not show a separate results panel.

Every input schema is a flat `type: "object"`. This is not a preference: the
claude CLI rejects the whole `tools/list` if any one tool emits `anyOf`, which
removes every injected tool at once. Per-action requirements go in a zod
refinement, exactly as the `Artifact` tool does.

Every mutating tool takes an optional semantic `expected_rev` and returns
`{applied, new_rev, concurrent_editors}`. That return value is the agent's
feedback channel: it learns that its edit landed, and that other people are in
the document. A cache-only Yjs write does not cause a conflict. A file write
could never tell it either thing.

A section-state command also takes a stable action id. The row binds that id to
a canonical command fingerprint, so reuse for a different command fails. Its
state comparison and full transcript-chip payload commit in one transaction. A
`runOnce` drainer publishes pending `spec_transcript_action` rows with that
action id as the delivery key, then marks each row delivered. A failed publish
leaves the row pending. A command retry reads its original action before it
validates the new section state, so it returns the original result without a
second state change. The transcript publisher must deduplicate by action id
because a process can stop after publish and before the delivered mark.

The central document validator identifies each changed section and enforces the
Requirements ledger before persistence. While the document store holds the
spec-row lock, it resolves a browser client through `spec_participant`. It then
commits the Yjs update, each required proposed state and each pending transcript
action in one transaction. Human-edit actions use the stable id
`human-edit:<spec-id>:<document-revision>:<section-id>`. A later section-state
command locks the same spec row, so it cannot pass an earlier document edit.

`spec_read` is the agent's freshness escape hatch. Its file copy is a snapshot;
this tool is live (N10).

Tools are `execution: "sync"`. The MCP bridge parks the call, the request rides
up as `tool_call_requested`, the owning pod executes it, and the result returns
over the durable outbox — so a tool call that spans an eviction completes after
resume, with no extra work from us.

### D7. The projection is a read-only render, published by staging and rename

The server renders the document to markdown and publishes it to
`/workspace/spec.md` with mode `0444`. The file opens with a comment that names
the rev and states that direct edits are discarded.

`WriteFile` cannot overwrite, so publishing is two steps:

1. `WriteFile` the render to `/workspace/.engrams/spec/incoming-<rev>.md`. The
   path is new every time, so the create-only rule never conflicts, and a retry
   with the same bytes succeeds by ADR 0113's own contract.
2. One durable exec, with the stable ticket `spec-publish-<spec_id>-<rev>`, that
   renames the staged file onto the stable path under a lock and sweeps older
   staged files:

```sh
flock /workspace/.engrams/spec/lock sh -c '
  rm -f /workspace/.engrams/spec/incoming-*.md.old
  mv /workspace/.engrams/spec/incoming-<rev>.md /workspace/spec.md'
```

`mv` inside one file system is atomic, so a reader never sees a partial file —
the property ADR 0113 bought for uploads is preserved here. The durable ticket
makes the step safe to replay.

The projection is refreshed after each agent tool mutation (so the agent's next
read shows its own change), at the turn boundaries `run_completed`, `idle` and
`parked`, at prompt delivery for a session that is not running, and at resume.

Human edits are **not** pushed into the guest in the middle of a turn. They
reach every browser at once, and they reach the agent at its next turn through
the digest, or immediately through `spec_read` if it asks. This one rule removes
the whole class of races in which the server rewrites a file the agent is
reading.

### D8. Direct writes are discarded, and the instruction is harness-neutral

The base system prompt tells every harness that the projection is read-only and
names the `spec_*` tools as the write path. Mode `0444` stops accidental writes.
The canonical document never imports the projection, so a direct guest edit
cannot overwrite a human edit.

A determined shell command can still change the file. The server therefore
samples the file's digest with a metadata-only `ReadFile` at the end of each
turn: one round trip, no body transfer. If the digest differs from the published
rev, the server republishes the canonical render and tells the agent, in the
next digest, that its direct edit was discarded.

The alternative — parse the guest markdown and merge it back into the CRDT —
needs a three-way merge, a conflict record and a rule for what happens when a
person edited the same paragraph. That machinery is the largest single piece of
this feature, and the tool path (D6) already gives the agent a better channel.
We do not build it. The cost is stated plainly to the agent instead of hidden.

### D9. Human edits reach the agent through the live tool and digest

The digest is written to `/workspace/.engrams/spec/digest.md` with the
projection. The harness-neutral base system prompt tells the agent to call
`spec_read` at the start of every turn and to read the digest when it needs the
per-author change summary. `spec_read` reads the orchestrator document, so a
queued prompt does not depend on the age of the disk projection.

The digest includes the current phase. For each section, it includes the state
and the state-change time. A settled section also includes the settling person.
The digest includes the people who are present and open-question counts. These
fields join the existing per-section and per-author edit summary.

The SDK has an 8 KB turn-context cap. The digest truncates its own content to
stay below that cap. `SPEC_DIGEST_PATH` remains
`/workspace/.engrams/spec/digest.md`. Moving it would require a base image
re-bake.

The instruction stays outside the user prompt text. The web therefore does not
render injected words as if the person wrote them. This first implementation
depends on agent instruction compliance. If product evidence shows that this is
not strong enough for N10, a later ADR can add one generic turn-context
capability for all harnesses. Spec mode does not add a vendor hook, wire event,
command, or harness bundle change.

**Amendment (2026-08-12).** Two changes replace compliance-dependent freshness
with enforced freshness, and reverse this section's no-vendor-hook call:

1. **The write fence.** `expected_rev` is now required on `spec_update_section`,
   `spec_set_section_state`, and `spec_update_block`, and it is section-scoped:
   each `spec_update_log` row records the section ids it changed
   (`changed_section_ids`, migration 0065), and a mutation is refused only when
   its TARGET section changed past `expected_rev` by a writer outside the
   agent's own session. A document-global check was rejected because live
   co-editing advances the global revision continuously — every agent write
   would bounce, and the model would learn to resubmit with the returned
   revision unread, which is no fence. The fence is harness-neutral: it holds
   even for a harness that ignores every instruction.
2. **The digest push.** `engram-harness-sdk` gains one turn-context seam
   (`turn_context::with_turn_context`): at the consumption boundary — the
   moment an adapter writes a prompt to its vendor agent — it prepends the
   digest file to the vendor-facing text, delimited as
   `<engrams-turn-context>`. Every adapter calls it (Claude and Codex today);
   a new harness inherits the behavior from the SDK instead of porting a
   vendor hook. This is harness-agnostic by construction: each adapter
   already owns its prompt queue and writes to the vendor only at turn start,
   so the digest is read after any queue wait and a queued prompt never
   carries a stale digest (N10). The transcript objection that rejected
   prompt-prepending does not apply at this layer: the coordinator's
   `role:user` message is the single source of the person's bubble, and the
   wrapped text reaches only the vendor process. Injection is gated on the
   session's tool manifest carrying `spec_read` — an orchestrator-controlled
   signal — never on the digest file existing, so a repository checked out
   into the workspace cannot plant a digest into a non-spec session's
   prompts. The read-every-turn prompt instruction is replaced with
   digest-guided section reads; the fence stays the correctness backstop for
   a harness that ignores every instruction.

### D10. Checkpoints are the history model; publish is a light confirm

A checkpoint holds a compacted document state, its state vector, the rendered
markdown, the covered `seq`, and a label written by the same small model that
titles sessions (R13). Checkpoints are cut when a run completes, before a
restore, and at publish.

Restore is a forward, section-scoped transaction: the section's content is
replaced with the checkpoint's render of that section, as a new edit. History is
never rewound (R14), and because a restore checkpoints first, a restore is
itself undoable.

Publish shows a light confirmation. It states the open-question count and says
that publication is one way. The owner acknowledges open questions when the
count is nonzero. Open questions do not block publication. Publish does not
compute a blocker list or run `spec_gap_check` automatically.

Publishing stays owner-only and refuses while the spec is in `ideation`. It
pins a checkpoint, stamps the publisher, and changes the phase to `published`.
It also writes the rendered markdown as an ordinary artifact version, so
sharing, serving and authorization come from ADR 0026 for free.

`GET /api/v1/specs/:id/decisions` returns the durable decision record. It
combines section settlements with open-question resolutions. Each item includes
its actor. The endpoint is member-gated.

The artifact store is deliberately not the history model. Its
`MAX_ARTIFACT_VERSIONS = 100` bound is a runaway-loop stop, and a long drafting
session will exceed it.

### D11. Presence, including the agent's

Human presence is Yjs awareness, relayed between replicas over the same
`NOTIFY` channel as updates. Awareness is ephemeral and small, well inside the
notification payload limit, and it is never persisted.

The agent gets presence too, synthesized from its in-flight tool calls: the
section argument of a running `spec_*` call becomes "the agent is working in
§Failure modes". It is section-level, not a character cursor, because the agent
does not have one — a fake cursor would claim precision the system does not
have.

### D12. A spec is the first org-shared live surface

Every general session surface is owner-only. A spec is org-visible and
org-editable (R1, R57), so its guards resolve **org membership**, not session
ownership. The document socket, shared conversation, spec event stream, and
decisions read model are member-gated.

Publishing stays owner-only (R37), and `published_by` is stamped from the first
release so that reviewer workflows can be added later without a schema change.

The full session shell stays owner-only. A non-owner uses the spec routes and
does not receive the session id from the spec read route.

### D13. A spec starts in ideation and crosses once into drafting

A new spec starts in `ideation`. The agent can read the repository, inspect the
document, and investigate risks. Every mutating spec tool refuses in this
phase. The template section identities can exist, but no section content is
written until a person asks the agent to start drafting.

`POST /api/v1/specs/:id/start-drafting` is the one-way bridge to `drafting`.
The route is member-gated and idempotent. It sends a seeding prompt that names
the person who requested the change.

This bridge keeps investigation visible in the conversation. It also prevents
an initial template skeleton from reading as filler before a person is ready to
write.

### D14. The spec conversation is one attributed multiplayer thread

Any org member can send a turn through
`POST /api/v1/specs/:id/messages`. The server stores the person's clean text in
`spec_chat_message`. It sends the prompt to the shared agent session with a
`[speaker: <Name>]` header. This header is the attribution seam that lets the
agent address people by name and reconcile their claims.

`GET /api/v1/specs/:id/messages?after=` returns the attributed human turns. The
web joins these rows to `agent_message` events on `prompt_id`. Human bubbles
render the stored clean text. They never render raw event text, which contains
the speaker header.

`GET /api/v1/specs/:id/events` is the member-gated SSE feed for the shared
thread. It uses the same session-event envelope and cursor rules as the
owner-only session feed. The full session shell and its feed stay owner-only.

The owner's session page shows the raw speaker header. This is an accepted
consequence of the shared-session design. A person could also put a forged
speaker header in the message text. The agent prompt gives authority to the
first header only. If spoofing becomes a real problem, the prompt will use a
JSON wrapper instead.

## Alternatives considered

**Loro instead of Yjs.** Loro is faster and Rust-native, and its oplog model
maps well onto checkpoints. It would pay off if the document lived in the
coordinator — but the coordinator must stay product-agnostic (D3), so Loro would
mean either moving product state into Rust or running WASM inside Bun. Neither
buys anything for a 2 MB document. Automerge was rejected for the same reason
plus lower text performance: its git-like history duplicates D10.

**A single owner pod per room, elected by the existing lease store.** Rejected
in D5. It converts a lease race into split brain, and CRDT semantics make the
ownership unnecessary.

**Sticky routing at the ingress.** Solves nothing that D5 does not, and it adds
an infrastructure dependency to a correctness property.

**Keep alternatives, working notes, and failure review as UI-only stages.**
Rejected. A stage makes ordinary collaboration into a modal special case.
Alternatives belong in document prose. Working notes belong in the shared
conversation. Failure review is an agent proposal that adds open questions to
the applicable sections.

**Give each person a separate agent session for attribution.** Rejected. The
agent would receive separate histories and could not reconcile two people's
claims in one thread. One shared session keeps the conversation coherent. The
speaker header provides durable attribution at the prompt boundary.

**The agent edits `spec.md` directly, and the server merges it back.** This is
the most natural reading of "its normal file tools work", and it was the
original plan. It needs a markdown parser, section segmentation, a three-way
merge against the last published render, a conflict table and a policy for
overlapping edits. Rejected in D8: the tool path is better for the agent (it
gets structure, a revision check and a list of concurrent editors) and much
smaller for us. Recorded as the main deliberate limitation of this design.

**Delete then write, to work around create-only `WriteFile`.** There is a window
in which the file does not exist, and the delete races any guest reader. The
staged rename in D7 has neither problem.

**A new agentd `ReplaceStream` with `expected_sha256`.** A compare-and-swap
replace is a coherent extension of the upload primitive, and it would collapse
D7's exec leg into one call. But agentd is baked into base snapshots, so a new
request variant must tolerate a new host talking to an old agentd, and it needs
a fleet-wide image refresh. Not worth blocking a 2 MB file on. Recorded as a
follow-up amendment; the layers above D7 do not change when it lands.

**Prepend the digest to the coordinator's user prompt.** Rejected because it
would pollute the transcript and could become stale in a prompt queue. The SDK
turn-context seam in the D9 amendment wraps only the vendor-facing prompt at
the consumption boundary.

## Runtime coverage

| Event | What happens |
| --- | --- |
| Human types while the sandbox is evicted | The document is server-held. Editing works; the projection refreshes at resume. |
| Member sends a conversation turn while the sandbox is evicted | The clean turn stays in `spec_chat_message`; the durable prompt path delivers the attributed turn after resume. |
| Agent tries to mutate a spec during `ideation` | The tool refuses the mutation. Reads and investigation continue. |
| Owner tries to publish during `ideation` | The route refuses publication. The spec must cross into `drafting` first. |
| Agent tool call spans an eviction | The ADR 0089 outbox delivers the result after resume. No spec-specific work. |
| Resume | The projection and digest are republished before the first prompt is delivered. The guest keeps nothing across an eviction, so the publish is unconditional. |
| Orchestrator pod roll | The document rebuilds from `spec_snapshot` plus the log tail. Clients reconnect and resync from state vectors. In-flight publishes are durable-exec steps with stable tickets. |
| Host roll | No effect on the document. The projection republishes on the new sandbox. |
| `WriteFile` or exec failure | The projection ledger row stays unpublished and the scanner retries. Prompt delivery is never gated on it: a stale projection is safe, because the rev is stated in the file and `spec_read` is live. |
| Two pods serving one room | Converges by D5. |
| Document exceeds the size cap | Rejected at the document service, so the render never grows past the cap (N6). |

## Testing

- **Document service, headless.** Concurrent updates from several clients
  converge; persist-before-broadcast holds under a simulated pod death; the
  snapshot plus tail rebuild equals the live document; compaction preserves
  state.
- **Cross-replica.** Two service instances against one live Postgres exchange
  updates through the log and the notification channel, including a dropped
  notification (gap fill by `seq`).
- **Projection.** Publish is idempotent under replay of the durable exec step;
  a staged file left by a failed publish is swept; a digest mismatch triggers
  exactly one republish; metadata-only sampling does not transfer the body.
- **Tools.** Every `spec_*` schema compiles to a flat `type: "object"` — assert
  this in a test, because the failure mode is the loss of every injected tool,
  not of one; `expected_rev` mismatch is reported, not applied. The section
  state matrix includes `proposed` to `open`. Every mutating tool refuses in
  `ideation`.
- **Editor.** A transaction that deletes or splits a section is refused;
  anchors survive a concurrent edit in another section.
- **Conversation and access.** Member routes accept org members and reject
  non-members. Stored human text has no speaker header. The agent prompt has
  exactly one authoritative header. The web joins human rows to agent events
  by `prompt_id` and never renders raw prompt text.
- **Phase and publish.** The start-drafting route is one-way and idempotent.
  Publish refuses in `ideation`. Open questions require a light acknowledgment
  but do not block publication. The decisions read model includes actors.
- **Product language.** Prompt and UI-copy tests refuse `frontier`, `drafted`,
  `confirmed`, `stage`, and `red-team`.
- Lanes: the orchestrator and web suites. This ADR adds no Rust, so no
  coordinator lane changes — if a change here needs one, see the invariant in
  D3.

## Implementation

The accepted document substrate is already in place. Each v2 phase lands as one
reviewable pull request.

- **S1 — Section states.** Rename the state values, add the Drop edge, add
  settlement credit, and remove the old section-order concepts from the rail.
- **S2 — Stage removal.** Delete the special alternatives, working-notes, and
  results panels. Keep failure inspection as an internal agent instrument.
- **S3 — Multiplayer conversation.** Add the message store, attributed prompt
  route, member-gated message reads, and member-gated spec event stream.
- **F1 — Frontend foundations.** Add the derived spec surface, thread
  projection, collaborator colors, section presence, scroll anchors, and
  spec-scoped message and event clients.
- **F2 — Spec shell.** Move the spec to its own route and add the four-column
  document shell.
- **S4 — Phase model.** Rename the phase field, add the start-drafting bridge,
  enforce the ideation write refusal, and rewrite the agent prompt.
- **F3 — Document surface.** Add the section list, section controls, open
  questions, and provenance treatment.
- **F4 — Conversation rail.** Add attributed turns, document activity, the
  persistent next proposal, and the shared composer.
- **S5 and F5 — Digest and presence.** Add the bounded digest fields and the
  matching presence surfaces.
- **F6 — Ideation.** Add the investigation thread and the explicit bridge into
  drafting.
- **S6 — Publish and decisions.** Add durable actor attribution, the light
  publish confirmation, and the decisions read model.
- **F7 — Creation and publish confirmation.** Replace the creation sheet and
  the blocking publish control.
- **F8 — Published and small-screen views.** Add the artifact read view, the
  decisions card, and the compact read-and-reply surface. Remove the remaining
  obsolete files and copy.

## Open questions

- **Compaction cadence.** The update log grows under live typing. Today, only
  the publish scanner and a checkpoint compact the log. A spec that people edit
  without an agent thus never compacts. A development spec reached 164,555 log
  rows with no snapshot, and the first cold load replayed the full log in one
  synchronous pass. That pass held the event loop for minutes and made every
  orchestrator route time out, not only the spec routes. A time or size trigger
  that does not need an agent action is therefore necessary, and the replay must
  not block the event loop. This work is not part of the v2 campaign.
- **Diagram-block rendering.** The renderer is new to the web tier and its
  choice belongs with the rich-node work, not here. Server-side rendering is
  out of scope: the projection carries a fenced code block, which the agent
  reads and edits well.
- **Agent presence granularity when a tool call is queued behind another.** The
  section argument is known at call time, but a queued call has not started.
  Show it as pending or not at all?
