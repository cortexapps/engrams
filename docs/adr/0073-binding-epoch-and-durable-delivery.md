# 0073 — Binding epoch, self-authenticating harness attach, and durable acked delivery

Status: Accepted (2026-07-06)

Issue: #542 (2026-07 core-ops overhaul, Tier 2). Related: #543 (op log), #545
(parking ladder), #535 (prompt-over-wire), #527 (prompt-receipt SLO anchor).

## Problem

The session→sandbox→harness binding lives in five places with no generation
number tying them together: the PG `sessions` row, the hub's in-memory maps,
the guest harness process, host-local `sandbox.json`, and the coordinator's
resolve-at-delivery view. Any host-agent roll, resume, or vsock bounce desyncs
them, and there is no protocol to re-derive the truth — so compensators
accreted (survivor rebind, retry-Rejected-forever, deliver-with-reattach polls,
a desync watchdog with a rehandshake ladder, in-memory replay buffers, a 30-min
PG backstop, and the orchestrator's "mention me again"). Prod evidence
(2026-07-01 pass): 49 `sandbox not found`/7d; a live VM "resumed" 8× in 12
minutes (fbd3794c); `SendPrompt` on an idle session blocks behind a 12.2s-p50 /
89s-p95 resume before acking.

Root causes, in order: **the binding is stored, not derived** (no single
authority, no epoch → every desync needs its own healer, and two healers can
fight); **delivery is fire-and-forget over a bouncy channel** (no hop has a
durable acked record, so every hop invented retries); **the hub is
five-mutexes-and-a-convention** (issue #217's ABBA wedge proved the documented
lock order is a promise, not a construction); **idle detection is duplicated**
because the in-memory copy is known-lossy.

## Decision

Four moves, one invariant set:

**(a) Binding epoch + self-authenticating attach.** PG owns
`sessions.binding_epoch BIGINT`, bumped atomically in the same statement as
every `sandbox_id` (re)assignment. Every bind pushes an attach token
`(session_id, sandbox_id, binding_epoch)` down: the bind host-RPC carries it;
the hub persists it as a host-durable record `<bindings_dir>/<session_id>.json`
(atomic write-temp-then-rename); the coordinator injects it into the harness
env (`ENGRAM_SANDBOX_ID`, `ENGRAM_BINDING_EPOCH` — pure `AgentSpec.env`
passthrough, no agentd proto change); the harness presents it in
`HarnessAttach` on every dial. The hub validates every attach against the
on-disk record, never an in-memory map:

- token matches disk → accept, immediately, even on a freshly restarted
  host-agent (the file survived the roll — the #447 gap closes by
  construction);
- presented epoch < on-disk epoch → reject `Superseded` (deterministic,
  fatal: the harness exits; the fbd3794c loop becomes unrepresentable);
- no record / other mismatch → reject `UnknownBinding` (transient: harness
  backs off, covering the sub-second create→bind window).

*Invariant: the binding is DERIVED from a handshake validated against durable
state; stale attaches are rejected deterministically, never healed
probabilistically.*

**(b) Coordinator-durable outbox, acked by `prompt_id`.** Commands down
(prompts, interactive answers) become PG rows in `session_outbox`; the host
queue is a non-durable relay. `SendPrompt` = user-echo emit + one INSERT +
NOTIFY + 202 in <50ms. A delivery driver resumes the session *behind* the
enqueue, forwards rows oldest-first per session, and marks `acked_at` when the
confirming event (`run_started{prompt_id}` / `prompt_queued{prompt_id}` /
`question_answered{tool_call_id}`) lands in `session_events`. Un-acked rows
redeliver on backoff; the harness's existing `seen_prompt_ids` dedup (ADR 0052)
makes redelivery a no-op. *Invariant: every command down to the guest is
durable until acked; per-session delivery order is row order.*

**(c) Hub actor-ization.** `HubInner`'s eight mutexed maps + the CANONICAL
LOCK ORDER (issue #217) + the `next_gen` counter (issue #218) collapse into
one actor task owning `HashMap<SandboxId, SandboxRuntime>`; public methods
become mpsc messages. ABBA inversions and guard-across-await become
unrepresentable by construction.

**(d) One idle detector.** The PG `session_events` scanner (today's
"backstop") becomes THE detector at host-tick cadence, with the exact
soft/hard-TTL semantics the hub scan implements in memory. The host
contributes only liveness (`harness_attached` in the heartbeat, consumed as a
disagreement alarm). Shell pins move to a PG column stamped by the
coordinator's WS bridge. The hub TTL scan, candidates POST, desync watchdog,
and rehandshake RPC are deleted. Eviction *policy* (pressure, TTLs, pipeline)
is untouched — that belongs to #545.

## Two epochs, deliberately distinct (coordination with #543)

This ADR's `sessions.binding_epoch` fences *which harness attach is current*:
bumped on every sandbox reassignment, carried in the attach token, validated
at the hub. Epic #543's `sessions.current_epoch` fences *which op executor may
act on a session's lifecycle*: bumped per op claim, stamped into host RPCs and
PG writes. They coexist and do not unify: a session can change sandboxes many
times under one executor claim, and an executor claim can change with no
sandbox reassignment. Once both land, `bind_session` carries both fields
(`binding_epoch` for the token, `fencing_epoch` for the RPC fence). An
implementer must not merge them.

## Consequences

- Deleted (no stubs): `rebind_survivor_sessions`, the `session_to_sandbox`
  map, `deliver_with_reattach`, the 60s HOLD loop, the replay buffers, the 10s
  attach wait, "mention me again", the lock-order block + 8 mutexes +
  generation counter, the hub TTL scan + host eviction tick, host shell-pin
  refcounts + sweeps + RPCs, the candidates POST path, `idle_detect_backstop`
  (subsumed), `desync_watchdog` + `rehandshake` RPC + metrics.
- Wire: `HarnessAttach`/`HarnessAttachAck` are a clean bincode break — a stale
  harness bundle cannot attach (correct failure; re-publish rides the PR).
  `bind_session` gains `binding_epoch`; WIRE_VERSION bumps current+1 at land
  (lockstep coord+host roll, no fallback ladder).
- Backends: uniform — the change lives at hub/coordinator/harness-proto,
  shared by FC/VZ/Process. Process sets the same env on its direct spawn. No
  silent-Ok: a bind path that cannot produce a token fails loudly.
- The outbox row is NOT the SLO receipt: `prompt_received` (#527) stays the
  user-visible receipt anchor; `session_outbox.created_at` times the durable
  enqueue only.
- SendPrompt semantics change from "delivered or error" to "queued (202)";
  the orchestrator drops its resume-failure apology arm.

## Phases

1. binding_epoch + attach token + disk-validated attach (migration 0083).
2. session_outbox + delivery driver + acks + 202 (migration 0084).
3. hub actor-ization (no schema/wire change).
4. one idle detector + PG shell pin (migration 0085) + watchdog/rehandshake
   deletion.

Single PR per the 2026-07 execution plan; one commit per phase; divergences
recorded here between commits; flips to Accepted with the commit chain.

## Divergence log

- **Phase 1 — epoch mint point moved coordinator-side, pre-spawn.** The issue
  folded the bump into the `assign_session_sandbox` writes, but the harness's
  token rides its spawn env, which is assembled BEFORE the sandbox exists (the
  backend mints the sandbox id). So the epoch is minted by a dedicated
  `MetadataStore::mint_binding_epoch` (one `UPDATE … RETURNING`) at the moment
  the coordinator commits to a (re)bind, rides `AgentSpec.binding_epoch`, and
  the backend stamps `ENGRAM_SANDBOX_ID` at spawn. Assignment writes are
  unchanged (no signature churn on the guarded CAS family).
- **Phase 1 — the epoch fences process lineage, not VM identity.** agentd
  REATTACHES a live harness child on `SpawnHarness` (ADR 0045 C1: teleport
  moves a running harness losslessly), and a surviving harness carries a
  frozen spawn env. Therefore: mint on create / drained idle-resume /
  respawn (any flow that expects a fresh process), do NOT mint on live moves
  (the record re-points to the new sandbox at the SAME epoch — allowed by the
  store; same-epoch writers are serialized by the session lease). The token's
  `sandbox_id` is informational; validation compares ONLY the epoch, and
  connection state keys on the transport-derived sandbox. A non-drained
  idle-resume's in-image survivor harness is deliberately fenced out
  (`Superseded`) and the fresh spawn replaces it — with the phase-2 outbox
  redelivering anything the survivor had in flight.
- **Hub actor-ization is MOOT-BY-DELETION.** The actor's entire
  justification was the eight-mutex web + the documented lock order + the
  issue #217 ABBA wedge + guard-across-await recurrences. After phases 2-3
  deleted seven of the eight maps (replay buffers → durable outbox; routing
  → on-disk binding records; TTL / shell-pin / eviction bookkeeping → the
  PG detector + shell-pin column) and this phase deleted the two write-only
  TTL maps outright, the hub holds ONE leaf mutex (`connections`), never
  nested, never held across an await. Wrapping one mutex in an actor task
  adds an mpsc, a task, and reply plumbing while deleting no failure mode —
  the repo's refactors-retire-code rule cuts against it. The invariant the
  actor was to provide ("ABBA unrepresentable") now holds by arithmetic:
  a lock order over one lock. Revisit only if the hub ever grows a second
  long-lived mutex.
- **Phases 3 and 4 are executed in reverse order.** Phase 4 (one idle
  detector + PG shell pin) deletes five of the eight hub maps outright
  (`last_event_at`/`last_idle_at` via the TTL scan, `shell_attached` +
  sweeps via the PG pin, `eviction_inflight` via the candidates-POST
  deletion); actor-izing them first would rebuild state the next commit
  removes. Purge, then actor-ize what survives (`connections` +
  per-connection checkpoint state + the generation counter) — same end
  state, half the refactor surface.

## Commit chain

```
ea934628 fix(rebase): reconcile ADR 0067 with merged tier-1 PRs (#561/#562/#563)
84cac7b5 refactor(hub): ADR 0067 phase 4 — hub reduced to one leaf mutex; actor-ization recorded moot
9fa4fe10 feat(idle): ADR 0067 phase 3 — one idle detector, PG shell pin, detection plane deleted
cf1adeea feat(outbox): ADR 0067 phase 2 — coordinator-durable acked delivery, SendPrompt 202
b1f6d3ee feat(binding): ADR 0067 phase 1 — binding epoch + self-authenticating harness attach
66158c55 docs(adr): 0067 binding epoch + self-auth attach + durable delivery (Proposed)
```
