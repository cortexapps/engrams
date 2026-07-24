# ADR 0103: Durable exec — a guest journal and attach-or-start semantics

- Status: Proposed
- Date: 2026-07-22
- Issues: prod incident 2026-07-22 (this ADR's Context; sessions `03f99dd2`,
  `adf045f6`, PRs #864/#869); ADR 0074 (parking ladder; the un-pause vsock
  black-hole post-mortem documents the `TRANSPORT_RESET` mechanics), ADR 0028
  (checkpoint durability), ADR 0045 (vmstate-only capture), ADR 0099 H5
  (crash-state testing discipline), ADR 0100 (PR review — the first workflow
  caller), ADR 0102 (automations — the next one)
- Prior work: commit `022491cb` (stage 1, branch `fix/exec-sever-on-checkpoint`)

## Context: the day exec results started silently vanishing

An exec today is a phone call held open across four hops: the orchestrator
holds a stream to the coordinator, the coordinator to the host-agent, the
host-agent a vsock connection into the guest, and agentd streams
stdout/stderr chunks back ending with exactly one `Exit`. Nothing about the
exec exists anywhere except in those open sockets. If any hop drops, the
result is unreachable forever — even when the command itself kept running
and succeeded.

One of those hops drops **on a timer**. Every FC snapshot capture — the
periodic checkpoint (~10 min cadence), the seed-at-create checkpoint, the
eviction capture, and the ADR 0045 vmstate-only capture alike — runs
`vmm.save_state()`, whose vsock `prepare_save` queues a `TRANSPORT_RESET`
for the guest (see ADR 0074's post-mortem). When the resumed guest acks it,
its vsock driver silently forgets every open connection. No FIN or RST
crosses the muxer, so **the host end of each connection never EOFs** — a
reader parked in a blocking read waits forever.

### What actually happened (2026-07-22, prod)

1. **22:18:04** — PrReviewWorkflow for PR #869 starts. It creates finder
   session `adf045f6` and execs the bootstrap:
   `rm -rf /workspace/engrams && git clone … && git checkout …`.
2. **22:18:07** — the guest starts cloning; one stderr chunk
   (`Cloning into '/workspace/engrams'...`) relays to the coordinator and
   lands in `session_events`.
3. **22:18:09–11** — the seed-at-create checkpoint fires: pause → diff
   capture → resume. The capture queues the `TRANSPORT_RESET`; the resumed
   guest drops the exec's vsock connection. Silently.
4. **22:18:10** — the egress proxy logs the clone's GitHub transfer
   completing: ~26 MB down, byte-identical to a healthy clone minutes
   earlier. **The command succeeded.** The guest then writes the checkout
   to disk (visible as NBD flushes through 22:19:40).
5. **22:18:10 → forever** — agentd's exit report has a dead connection to
   write into; the host-agent's reader blocks in `read_msg` on a socket
   that will never EOF; no `exec_completed` is ever emitted; the
   workflow's `runExec` never returns. The review spun for over an hour
   until a human killed it. A second session (`03f99dd2`, PR #864) had
   died the same way at 20:08.

Two structural facts make this worse than a one-off:

- **The cadence math.** P(a given exec gets severed) ≈ duration / checkpoint
  cadence. A 30-second exec is safe ~95% of the time; a 5-minute build
  fails about half the time; **any exec longer than the cadence is
  guaranteed lost.** The platform as built cannot reliably run a long exec.
- **The caller can't defend itself.** Even with a timeout (the current
  `runExec` sets none), a caller that gives up has lost a result that may
  have been a success — and re-running is only safe for idempotent
  commands. Worse, the whole stream-drain lives inside one DBOS step, so
  an orchestrator restart mid-exec re-runs the step and **re-executes the
  command**: double execution for non-idempotent commands.

Everything else riding host↔guest vsock already honors the "captures reset
connections" contract: the harness re-dials on a dropped link, the
shell/browser relays are supervised and rebind (issue #567). Exec is the
one one-shot stream with no recovery — and it fails in the worst shape
(silent, infinite) rather than the acceptable one (loud, terminal).

## Decision

Make an exec's **result** durable in the guest and make its **identity**
durable in the caller, so that every link in the chain — vsock, host-agent,
coordinator, orchestrator — can die and reconnect without losing the result
or double-running the command. Three nested layers; each outer layer
degrades to the one beneath it.

1. **Stage 1 — severance becomes termination, never a hang** (shipped,
   commit `022491cb`). Each `LiveSandbox` carries a `vsock_epoch` watch;
   both snapshot-create paths bump it (on failure too — a failed create may
   still have quiesced the device); the exec reader races its blocking read
   against the epoch and synthesizes `Exit(None)` on a bump. Downstream
   already treats a null exit status as failure. This is the permanent
   floor: every skew/fallback case below lands here.
2. **Stage 2 — the exec journal.** The guest filesystem is the durable,
   snapshot-consistent medium (it rides every checkpoint and restore), so
   the result lives there, written by the command's own wrapper — not by
   agentd, whose process lifetime is shorter than a long exec.
3. **Stage 3 — one exec RPC with attach-or-start semantics.** `Exec` takes
   an optional caller-supplied `exec_id` and resume offsets. If the
   journal exists, attach and tail from the offsets; if not, create it and
   spawn. Reconnecting is re-calling. There is no separate start/await
   API, no legacy surface, and no exec state in Postgres — **the journal
   is the state.**

## How it works

### The journal (guest, written by the wrapper)

agentd spawns each exec wrapped, morally:

```
mkdir /var/lib/engram/execs/<exec_id>        # ← the spawn dedupe marker
write request.json                            # command, timestamps
sh -c '<command>' > stdout 2> stderr
echo '{"exit":<code>,...}' > exit.json.tmp && mv exit.json.tmp exit.json
```

- `mkdir` happens **before** fork and is the atomic "this exec has been
  spawned" marker. Attach-or-start branches on it: dir exists → attach
  only, never spawn; dir absent → create + spawn. This is what makes
  delivery retries safe (at-least-once delivery, exactly-once spawn).
- `exit.json` is written **by the child's shell, atomically, last**. It is
  the completeness marker (ADR 0099 H5 style): present ⇒ the recorded exit
  code is exact; absent ⇒ still running, or died — never a fabricated
  result. Because the wrapper owns it, the record completes even if agentd
  re-execs mid-command (which happens at every session create via
  `RefreshAgent`) or crashes.
- `request.json` records the command so a re-attach with the same
  `exec_id` but a different command is rejected loudly (first-writer-wins)
  instead of silently returning the wrong result.
- **Bounds:** stdout/stderr each capped (64 MiB default), truncate-with-
  marker beyond the cap — the exit code stays exact; output beyond the cap
  is only complete if a listener was attached while it streamed. Per-
  session cap on concurrent journals. If a journal write fails (disk
  full), the exec itself must not fail: that exec degrades to stage-1
  semantics and the record is marked degraded.
- **GC:** journals are deleted after a TTL past `exit.json`, and the whole
  tree lives outside any base-image capture path.

### The wire (host ↔ guest, coordinator ↔ host, app-gRPC)

One request shape everywhere, additive over today's:

```
Exec {
  session/sandbox, command,
  exec_id?,          # caller ticket; omitted → minted server-side
  stdout_offset?, stderr_offset?,   # resume points, 0 on first call
  wake?,             # unpark the session if parked (SendPrompt-style)
}
→ stream: ExecStarted{exec_id}, Stdout…, Stderr…, Exit{status}
```

First frame always announces the `exec_id` so even a casual caller *could*
reattach. On attach, agentd replays the files from the offsets, then
live-tails until `exit.json` exists, then sends `Exit`. `CancelExec
{exec_id}` is the one other verb — killing is a different act from
reading, and without it a caller that gives up leaves a zombie burning
guest CPU.

The host-side reader keeps the stage-1 epoch race — but a bump now
triggers **re-dial and re-attach from the current offsets** instead of
giving up. When re-attach is impossible the reader distinguishes two very
different situations: transport-shaped loss while the ticket may still be
valid (reconnect failed, stream ended without an Exit) ends the stream
**without any Exit frame** so the caller re-attaches; only conditions
where another attach is *unsafe or meaningless* (agentd predates the
upgrade, degraded journal) fall back to the loud stage-1 `Exit(None)`
floor. Refusals (identity mismatch, GC'd ticket) are their own terminal
frame — see the taxonomy below — never an exit.

### The caller (orchestrator)

The workflow body does not change — the shell stays hash-stable (ADR
0100). All the durability lives inside the existing control-plane method:

```ts
async bootstrapFinderSession(sessionId, { reviewId, repo, headSha }) {
  // sessionId came out of a checkpointed step: deterministic per attempt.
  const execId = `exec:${sessionId}:bootstrap-clone`;
  const { exitStatus, stderr } = await runExec(sessionId, command, {
    execId, deadlineMs: 5 * 60_000,
  });
  if (exitStatus !== 0) throw new ReviewSetupError(…);
}
```

`runExec` becomes the one retry loop: *attach → drain → on disconnect,
re-attach from offsets → until Exit or deadline*. Because `execId` is
deterministic and attach-or-start never double-spawns, the composite DBOS
step can safely re-run as a unit after any orchestrator restart — no
start/await step split needed, and non-idempotent commands are safe from
double execution. A deadline is finally safe to have, because expiring
can no longer lose a result that was merely delayed.

## Failure matrix — before and after

| # | Scenario (all observed or directly constructible) | Today | With this ADR |
|---|---|---|---|
| 1 | Checkpoint fires mid-exec (22:18 incident) | Reader hangs forever; result lost though command succeeded | Epoch bump → re-attach → journal replays the gap; caller never notices |
| 2 | Orchestrator deploys mid-exec (3 deploys on 2026-07-22 alone) | DBOS re-runs the step → command executes twice | Step re-runs → same `exec_id` → attach finds the journal → no re-spawn, result delivered |
| 3 | Coordinator or host-agent restarts mid-exec | Stream dies, result lost | Caller's retry loop re-dials; journal replays from offsets |
| 4 | Exec finishes while nobody is connected | Result evaporates | `exit.json` waits in the journal until asked or TTL |
| 5 | Session evicted mid-exec, resumed later (other host) | Result lost; child frozen forever from caller's view | Child resumes with the VM, wrapper writes `exit.json`; next attach (with `wake`) delivers it |
| 6 | Exec never exits (daemon, hang) | Caller hangs forever (no timeout today) | Deadline bounds the wait; `CancelExec` kills by ticket |
| 7 | agentd re-execs mid-command (`RefreshAgent`, every create) | (Latent) journal would lose its ending if agentd owned it | Wrapper owns `exit.json`; agentd's lifetime is irrelevant |
| 8 | Guest disk full during journal write | n/a | Exec proceeds; record marked degraded; stage-1 semantics for that exec |
| 9 | Snapshot rewind past a completed side effect (D5 class) | Same exposure — inherent to crash-restore | Unchanged and now **documented**: ticket = exactly-once spawn per timeline; external side effects = at-least-once under rewind (see Contract) |
| 10 | New coordinator, old agentd (rollout skew) | n/a | Capability-detected → stage-1 fallback, named in the error (the #567 skew pattern) |
| 11 | Checkpoint fires during the capability probe / CancelExec / Upload round-trip (before any exec stream exists) | n/a (new surface) | One-shot reads AND writes race the epoch watch → retryable `Unavailable`; all three round-trips are idempotent |
| 12 | Silent exec (non-tty `git clone` prints nothing) + transport flap burst mid-run | n/a (new surface) | Flap costs retries, never the result: the deadline is the only budget; an open attach waiting through silence consumes nothing |
| 13 | Attach with a ticket agentd must refuse (command mismatch, GC'd/missing journal with offsets) | n/a (new surface) | Distinct `Refused` terminal — deterministic, fails fast at the caller, never recorded as a completion |
| 14 | Wrapper killed (OOM) while its command lives on in its own process group | n/a (new surface) | The record counts live and is never GC'd while the *command* pid survives — the spawn-dedupe marker outlives the wrapper |
| 15 | Ticket re-used after guest journal TTL/GC with zero offsets | Documented residual | Attach-or-start semantics re-run the command; coordinator lifecycle rows stay deduped by exec_id, so run 2 leaves no new rows — callers mint fresh tickets per logical run |

## The contract, stated plainly

- **Exactly-once submission.** For a given `exec_id`, at most one spawn per
  surviving guest timeline — retries, replays, and redeliveries can never
  start a second copy. (The journal dir is the guard.) *Skew exception:*
  an old agentd (a session created before the fleet roll; `RefreshAgent`
  upgrades agentd only at session create) has no journal and therefore no
  dedupe — if the caller's retry loop re-calls `Exec` against one (only
  possible when the coordinator↔caller stream itself died without any exit
  frame), the command spawns again. That is stage-1 exposure, not a
  regression: it closes with the roll, and review sessions never hit it
  (they are created fresh, after the roll, with new agentd).
- **One canonical result.** The caller gets whatever the surviving timeline
  recorded. Exit codes are always exact; output is exact up to the cap.
- **At-least-once external side effects under rewind.** A snapshot taken
  mid-exec that is later restored resumes the command's tail; effects on
  external systems (a `git push`) may fire twice. No crash-restore system
  can do better without the external system's cooperation. This is the
  platform's existing contract for *all* guest execution (the agent pushes
  from inside the guest today); this ADR inherits it, it does not create
  it. **Design guideline:** external side effects belong in orchestrator
  workflow steps (durable, dedup-able domain); guest execs should stay
  workspace-local (the rewindable domain, where effects rewind consistently
  with the disk).

## Test plan

- **Stage 1 (exists):** `exec_reader_synthesizes_exit_when_snapshot_severs_vsock`
  — one stderr chunk, then a silent never-EOF connection, then the epoch
  bump; asserts synthetic `Exit(None)` and stream end.
- **Crash-state suite (ADR 0099 H5, all externally constructible):** build
  journals by hand — torn `stdout` at arbitrary offsets, `exit.json.tmp`
  present but unrenamed, missing `exit.json` with a dead pid, garbage
  sibling files, `request.json` command mismatch — and assert attach
  reports running/died/mismatch correctly and never fabricates an exit.
- **Attach-or-start property tests:** same `exec_id` twice concurrently →
  exactly one spawn; offsets replay without gaps or duplicates
  (proptest over chunk boundaries).
- **FC integration (sized to the property, ADR-0099-style minimal):** one
  VM, start an exec that sleeps then echoes, take a diff snapshot
  mid-sleep, assert the same `Exec` call re-attached and delivered the
  exit — the end-to-end proof that the severance path heals. Wire it into
  `ci.yml`'s FC lane explicitly.
- **Skew:** new host code against a stubbed old-protocol agentd → stage-1
  fallback with the named error.
- **Deferred (recorded, not forgotten):** the e2e-stack severance smoke
  (kill the coordinator-side stream once mid-exec through the full
  orchestrator→coordinator→host stack and assert the retry completes) is
  not yet implemented — the cosim covers the composition with the real
  coordinator core and real agentd; the e2e lane currently drains
  happy-path execs only.
- **Caller layer (orchestrator) — the review flow is the reference
  consumer and gets pinned explicitly:**
  - *Stage-1 regression (needed now, independent of stages 2+3):* a fake
    sessions client whose exec stream ends **without an exit frame** (and a
    variant with `exit_status: null`) → `bootstrapFinderSession` /
    `bootstrapVerifierSession` must throw `ReviewSetupError` and the
    workflow must land in `failReview` with a durable failed record —
    never report success, never hang. This pins the "downstream already
    handles a synthetic `Exit(None)`" claim stage 1 relies on; today that
    behavior exists (`exitStatus !== 0` where `undefined !== 0`) but no
    test asserts it.
  - *`runExec` attach loop:* against a fake exec server — disconnect
    mid-stream → re-attach with offsets → assembled stdout/stderr have no
    gaps and no duplicated bytes; deadline expiry → error and
    `CancelExec` issued; server counts spawns per `exec_id` and the test
    asserts exactly one across arbitrarily many disconnect/re-attach
    cycles.
  - *Step-replay safety:* re-run the whole control-plane method (as a
    DBOS step re-run would) with the same deterministic `exec_id` → the
    fake server sees attach, not a second spawn; the caller still gets
    the original result. This is the double-execution guarantee, tested
    at the layer that owns it.
  - *e2e-stack smoke (one, minimal):* start a real exec through the full
    orchestrator → coordinator → host path, kill the coordinator-side
    stream once, assert the caller's retry loop completes with the right
    exit — the only lane that proves the hops compose (per the e2e-lane
    rule in AGENTS.md).

## Rollout

1. Stage 1 is host-agent-only: host fleet roll, no rebake. Ship first —
   it stops the observed bleeding and is the floor everything else falls
   back to.
2. Stages 2+3 ship together: agentd (bundle republish + host roll; new
   sessions pick it up via `RefreshAgent` at create), host-agent reader,
   coordinator exec path **rewritten** onto attach semantics (the old
   buffered-stream implementation is deleted, not wrapped), regenerated TS
   clients, `runExec` retry loop. Skew between the pieces degrades to
   stage 1 by construction.
3. The review workflow body is untouched; only `review-control-plane.ts`
   changes. (Note: any *workflow body* edit still re-versions DBOS
   workflows until the version-pinning/adopt-on-boot work lands — land
   that first or together.)

### Stage 2 implementation notes (2026-07-22)

- The initial journal policy is a 24-hour post-exit TTL, at most 32 active
  journals per guest, and the specified 64 MiB cap per output stream.
- Host epoch recovery carries an internal `attach_only` bit on the
  host↔guest bincode request. It is deliberately absent from the public
  API: a reconnect after GC must fall back loudly to Stage 1 instead of
  interpreting the missing directory as permission to spawn again.
- Rollout capability detection uses the reserved `CancelExec` ticket
  `__engram_durable_exec_capability__`. A new agentd recognizes it without
  touching the journal; an old agentd returns its typed unknown-request
  error, after which the host submits the original five-field bincode shape
  and retains Stage 1 behavior.
- Rust protobuf bindings regenerate as part of the Cargo build. The
  TypeScript/client `buf generate` output stays for the caller-side Stage 3
  change so this stage does not partially modify the orchestrator surface.
- The co-simulator now pins the 2026-07-23 `6d403c4b` recurrence:
  `checkpoint_severs_exec_mid_stream` composes the real coordinator exec
  core, real Firecracker host reader, and real agentd journal handler across
  a silent checkpoint severance. A FIFO orders the real subprocess around
  the capture; the test uses real Tokio time so paused-time auto-advance
  cannot outrun subprocess I/O through reconnect backoffs or journal grace
  windows.

### Review hardening (2026-07-23)

### Terminal taxonomy (normative)

Every way an exec stream can end falls into exactly one of four classes.
Every hop — agentd, backend reader, host pump, coordinator, caller — must
preserve the class; converting between them is the bug this section exists
to prevent.

| Class | Wire shape | Coordinator | Caller (`runExec`) | Lifecycle rows |
|---|---|---|---|---|
| **Genuine exit** — the command completed (incl. a real died-without-exit `Exit(None)` after diagnostics, and cancel/timeout kills) | `Exit{status?}` | persists `exec_completed` (deduped by exec_id), `wall_ms` from the logged start | returns the status | started + completed |
| **Transport loss** — severance, EOF, reconnect failure, pump channel close, while the ticket may still be valid | stream ends **without** `Exit` → `Unavailable` | retryable error, **no** completion row | re-attach from offsets, capped backoff, until the deadline (the only budget) | none new |
| **Refusal** — deterministic rejection of THIS attach: command/identity mismatch (first-writer-wins), GC'd/missing journal with a used ticket, non-zero offsets against a missing record | `Refused{reason}` — its own frame, never an `Exit` | terminal non-retryable error, **no** lifecycle rows | fails fast, no retry | none |
| **Stage-1 floor** — another attach is unsafe or meaningless: old-agentd skew (double-spawn risk), degraded journal | `Exit(None)` after the named `Degraded`/skew diagnostic | persists the null completion | returns `exitStatus: null` (callers treat non-zero as failure) | started + completed(None) |

Caller policy, stated once: **the deadline is the only budget.** Retry
every transport loss with capped exponential backoff (reset the backoff on
byte progress so healing is fast; a silent healthy exec holds one open
attach and consumes nothing). Refusals, protocol violations (duplicate or
mismatched `ExecStarted`), NotFound, and no-live-sandbox are terminal
immediately — deterministic conditions are not retried at all. There is no
attempt counter: every previous attempt-counting scheme was either dead
code (any-frame reset — the coordinator prepends `Started` on every
answered attach) or a hair trigger (byte-progress reset — a ~10s transport
flap burned 9 attempts against a silent `git clone` with minutes of
deadline left).

- Backend parity follows the same fabricate-only-where-unsafe rule: Process now
  has sandbox-lifetime attach-or-start records instead of echoing a false
  durability signal, and durable VZ transport EOF ends without a fabricated
  exit while its legacy path retains the stage-1 `Exit(None)` floor.
- Lifecycle accounting must not conflate "no completion marker" with "still
  running", and retention must be bounded: the journal's active cap now counts
  only records whose wrapper/command is provably live (a died-without-marker
  wrapper frees its slot instead of monotonically shrinking the 32-exec budget
  toward permanent `DegradedStart`), TTL GC also reclaims provably-dead
  incomplete records (they stay diagnosable for a full TTL; live recordings
  are never collected), and the Process backend evicts completed exec records
  oldest-first beyond the same 32-record budget.
- A full producer/pump/consumer sweep closed the missed half of the
  coordinator↔host adapter: the host-agent gRPC pump now turns backend
  end-without-Exit into `Unavailable` instead of re-fabricating `Exit(None)`.
  The artifact-upload bytestream rejects the same condition instead of storing
  a silently truncated file, the generic buffered exec drains fail honestly,
  and zero-byte re-attaches deduplicate `exec_started` by durable
  `(session_id, exec_id)` event-log lookup rather than by byte offsets.
- Guard the resource, not the code path: the severance epoch must be raced by
  *every* read that can park on an established guest vsock connection, not
  just the steady-state exec reader loop. The audit that follows from that
  rule found three more unguarded one-shot response reads — the durable
  capability probe (which runs on every exec, before the guarded loop
  exists), `CancelExec`, and `write_files`' Upload — each a re-introduction
  of the infinite wedge on its own hop. All three now race the epoch watch
  via a shared helper and fail as retryable `Unavailable` (all three
  round-trips are idempotent). The same rule applies to WRITES: a large
  Upload payload (or exec stdin) written into a severed connection parks
  once the socket buffer fills, so severable writes race the epoch too.
  The remaining vsock reads are either timeout-bounded (GuestIp 2s,
  RefreshAgent 10s, shell/IDE/VNC 15–30s, SpawnHarness 60s/attempt) or
  pre-application handshakes served by the always-running muxer with
  EOF-shaped retry (`connect_fc_vsock`).
- Journal liveness is about the COMMAND, not the wrapper: an OOM-killed
  wrapper whose command survives in its own process group must keep its
  cap slot and its spawn-dedupe marker (GC'ing the dir would let a retry
  double-spawn a still-running command), and dead incomplete records age
  from their last observed activity (newest file mtime), not from their
  start time — a 24h exec that dies uncleanly still gets a full diagnosis
  window.
- Completion accounting mirrors the start-side dedup: `ExecCompleted` is
  deduplicated by exec_id against the durable log (a journal replay of an
  already-complete exec reaches a real Exit on every attach), and `wall_ms`
  spans from the recorded `exec_started` to the Exit rather than the attach
  segment that happened to deliver it. The store method generalized to
  `session_exec_event_at` (kind-typed, `MIN((payload->>'at')::timestamptz)` —
  the event's OWN stamp, not the row's `created_at`; see the self-review
  section for why); its D4 conformance scenario pins every predicate
  clause — exec_id match, session scoping, kind filter (stdout/completed
  payloads also carry exec_id), first-occurrence-wins over duplicates, and
  payload-stamp-not-row-time via a skewed row.
- **Output recording is observation-independent (fixed in this PR).** The
  coordinator persists exec output rows as a side effect of serving an
  attach — the writer placement is forced (it is the only PG-capable
  component that sees the bytes, and only while serving), so the fix is
  idempotence, not relocation: every chunk row is stamped with its absolute
  RAW byte range (`bytes_start`/`bytes_end`), and an attach skips
  persisting at or below the ticket's recorded per-stream high-water mark
  (`session_exec_output_high_water`, D4-conformance-covered), trimming a
  straddling chunk at the raw byte boundary before the lossy decode.
  Attaching N times now records the same rows as attaching once; replayed
  bytes still stream to the caller. Documented residuals: two simultaneous
  from-zero attaches can still double-record (same accepted race as the
  lifecycle dedup); a persistence gap below the mark is no longer healed by
  replay (best-effort recording, unchanged posture); rows written before
  stamping existed are invisible to the mark (their execs may duplicate
  once more, then never again); and output no caller ever pulls through
  the coordinator is never recorded at all (closing that needs a
  completion-triggered journal drain — a second 64 MiB transfer per exec,
  unjustified for a debugging surface).
- **Cancel and dedup safety pass (review round 8/9).** Four findings, each
  a case of the same principle — *the guard must cover every entry to the
  hazard, and the safe direction must be consistent across sites*:
  - The exec path validates the raw `exec_id` before building a journal
    path; CancelExec did not, so a `../`-shaped ticket escaped the journal
    root and could SIGKILL an arbitrary in-guest process group. Fixed by
    validating the raw string at the wire entry (`existing_entry`) plus a
    `..`-component reject in `from_dir` as defense in depth.
  - `appears_live` treats PID reuse conservatively (over-count), but
    `cancel` killed by the recorded pid unconditionally — the *unsafe*
    direction against the same hazard. Fixed: a terminal `exit.json`
    short-circuits cancel to a no-op (nothing to kill; the pid is reusable).
  - The lifecycle/high-water dedup queries counted rewound (tombstoned)
    rows, so after an ADR-0028 recovery rewind — which also rewinds the
    guest journal, forcing a genuine re-run — both stores suppressed the
    re-run's lifecycle rows and held its output high-water up. Fixed with
    `AND rewound_at IS NULL` (both stores, D4-conformance-covered): the
    dedup sees the live timeline only.
  - `ExecStarted` was persisted before the stream was polled, so a refused
    attach still recorded a start (and poisoned the ticket's start-dedup for
    a later real run). Fixed by deferring the emission to the first
    non-`Refused` frame — the taxonomy's "refusal records no lifecycle rows"
    now holds at the coordinator.
  - The reconnect-hop attach write was the one severable write the earlier
    write-sweep missed; it now races the epoch like the initial write,
    CancelExec, and Upload. And `runExec` reaps its own spawned exec on the
    protocol-violation give-up (but never on a refusal — the ticket's real
    first-writer is running there, and cancelling would kill it).

### Self-review hardening (2026-07-24)

An adversarial self-review of the round-8/9 changes (three independent
reviewers per hazard area, findings hand-verified) surfaced six defects —
several of them *introduced or made load-bearing by* the earlier fixes:

- **`isTerminalExecError` treated every FailedPrecondition as terminal.**
  The two message guards ("exec refused", "no live sandbox") were dead code
  — FailedPrecondition wasn't in the retryable fallthrough list, so the
  function answered "terminal" before ever consulting them. But the
  coordinator maps `Gone`, `HostLost`, AND every `Conflict` — including the
  documented-retryable Evacuating/Queued/Pending states and the transient
  no-live-sandbox eviction flip — onto FailedPrecondition. An operator
  drain mid-exec therefore failed the workflow instantly with hours of
  deadline left, violating "the deadline is the ONLY budget"; and because
  terminal errors deliberately never reap, it also leaked the running exec.
  Fixed: FailedPrecondition is retryable BY DEFAULT; terminal only for the
  `engram-error-slug: snapshot_invalidated` metadata (`ApiError::Gone` —
  dead/failed/completed session), "exec refused", and the identity-changed
  invariant guard. If the slug is stripped in transit the failure mode is
  retry-until-deadline (bounded), never a premature give-up.
- **The deferred `ExecStarted` collapsed `wall_ms` to ~0 for silent
  commands** — the exact non-tty `git clone` shape this ADR exists for.
  Deferring emission to the first frame also deferred the *measurement*:
  a silent command's first coordinator-level frame is its Exit. Fixed
  twice over: the emission stamps the attach-time clock reading (only the
  emission is deferred, not the measurement), and the store lookup reads
  the event's own `at` (renamed `session_exec_event_at`) instead of the
  row's `created_at` — the row lands at first-frame time even with the
  stamp fixed.
- **A losing attach could stream the winner's output before its refusal.**
  The pre-replay identity check is skipped when the winner hasn't flushed
  `request.json` yet, and the drain loop read chunks before checking state.
  Since `request.json` is written once, atomically, and never mutated,
  identity is decided permanently the moment it exists: one Mismatch check
  after the publish-wait window, before any drain, closes every ordering.
- **The terminal-marker cancel gate covered only the CLEAN half of the
  PID-reuse hazard.** `exit.json` lands after the wrapper's pipe drain, but
  the command's pid becomes reusable at reap — a daemonizing child that
  inherits stdout stalls the drain forever, leaving a reaped pid with no
  marker. The wrapper now records the command's `(pid, starttime)` identity
  at spawn (read pre-wait, while even a fast-exiting command is a readable
  zombie), and cancel verifies it before `killpg`: a mismatch proves the
  pid was recycled — and a pid is only recyclable once the whole group is
  empty — so there is provably nothing left to kill. `kill_process_group`
  also swallows ESRCH (the group being gone is every caller's goal state);
  previously the `?` made cancelling a provably-dead exec an error and
  defeated the finished-during-cancel retry loop.
- **The probe and CancelExec writes didn't race the epoch** while this ADR
  claimed all one-shot I/O did. Fixed (the record and the code now agree).
- **The web UI read `rusage.duration_ms`, the wire carries `wall_ms`** —
  exec durations had never rendered. Pre-existing, fixed alongside.

Documented residuals (accepted, not fixed): a reap against an idle-evicted
session resurrects the VM just to kill one process (`cancel_exec_core`
calls `ensure_active`; correct direction — the command would resume-and-
burn otherwise — but a heavy discard); a failed best-effort `exec_started`
emission leaves output rows with no start row, which the web transcript
drops until a later attach self-heals the start; the concurrent
duplicate-`Started` window widened from one await to first-frame (same
MIN-absorbed race as before); and a refusal on a GC'd ticket after a
genuine earlier run leaves the earlier run's rows in place — "refusal
records no lifecycle rows" is about the refused attach, not about
retroactively erasing history.

## Alternatives considered

- **In-memory ring buffer in agentd + replay verb.** Rejected: bounded by
  guest RAM (128 MiB guests vs. multi-hundred-MB build logs), lost on
  agentd re-exec (which happens per create), and duplicates what the disk
  already does durably.
- **Separate Start/Await RPCs.** Rejected for surface area: attach-or-start
  gives the same durability with one verb, and one-shot callers keep
  single-call ergonomics. The split survives only as vocabulary inside
  `runExec`'s loop.
- **Coordinator-side healing (make durable exec "a coordinator protocol the
  orchestrator just consumes").** The coordinator could internally re-attach
  to the host on coord↔host stream loss, hiding host-agent rolls from the
  caller within one RPC. Rejected because it removes NOTHING from the
  caller: the two failures this feature exists for are the coordinator's
  own death and the caller's own replay, and both make the ticket and the
  cursor irreducibly caller-side — "delivered" means delivered to the
  caller (only the receiver knows its own high-water mark across the
  server's death, the same reason Kafka consumers own their offsets and
  SSE clients send Last-Event-ID), and only the caller can name an exec
  deterministically across its own DBOS step replays. Server-side healing
  would add a third retry loop and a second offset-tracking state between
  the FC driver's redial and the caller's loop, while the caller keeps
  every obligation it has today. One dumb idempotent verb + one
  caller-held cursor is the smallest total system.
- **SSE (or similar resumable-stream transport) instead of the gRPC
  stream.** SSE's `Last-Event-ID` is exactly our byte cursor — the design
  borrows that idea — but adopting the transport removes nothing (the
  cursor is renamed, the exactly-once ticket has no SSE analog, deadline/
  cancel stay caller-side) and costs real things: SSE has no typed terminal
  frames (every ending is an untyped connection drop, erasing the terminal
  taxonomy this ADR is built on), output is UTF-8-only (base64 for binary,
  +33% on build logs), and it would resurrect the coordinator HTTP surface
  ADR 0051 retired, leaving two exec protocols to keep in conformance.
  EventSource's auto-reconnect — the one labor-saver — is a browser
  affordance; our server-side caller hand-writes the loop either way.
  Note the current design is already "polling with a push optimization":
  each attach is a poll of the journal from the cursor that keeps pushing
  while connected, and under repeated severance it degrades gracefully
  into exactly that.
- **Exec rows in Postgres.** Rejected: the journal already is the state,
  and a PG mirror adds a second source of truth, MetadataStore surface
  (and its ADR 0098 D4 conformance burden) for no additional durability.
  Revisit only if fleet-wide exec observability is wanted later.
- **Defer checkpoints while an exec is in flight.** Rejected as a
  correctness mechanism: a legitimate exec can run longer than the
  checkpoint cadence, and starving checkpoints trades away crash
  durability (the D5 lesson, inverted). Optionally worth doing as a
  *latency* optimization for the seed-at-create race; never required.
- **Caller-side retry of idempotent commands.** Rejected as the fix (fine
  as a stopgap): doesn't generalize past idempotent commands and papers
  over the real defect.
