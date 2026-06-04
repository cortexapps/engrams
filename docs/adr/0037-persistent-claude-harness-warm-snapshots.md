# ADR 0037: Persistent Claude harness + warm-captured base snapshots

Status: 2026-06-04 — **Proposed.** Branches off `main`. Builds on
[ADR 0022](0022-runtime-memory-sharing-and-forking.md) (File-backend base-memory restore,
shared across same-template siblings) and on the [ADR 0034](0034-idle-eviction-control-plane-and-detection.md)
snapshot-durability + idle-evict work merged to `main` as **#82** — which deliberately
**reverted** its harness crash-diagnostics ("separate harness rework owns it"); this ADR is that
rework, so the crash-detection is authored here against the new persistent (EOF = crash)
semantics. The three load-bearing risks were retired by spikes on the
`worktree-adr-0022-warm-harness-spike` branch before proposing (numbers below).

## Context / problem

The built-in Claude harness (`engram-harness-claude`) is **child-per-prompt**: each
`HarnessCommand::Prompt` spawns a fresh `claude --print …`, drains its stdout to EOF, and the
child **exits**. Two costs fall out of that:

1. **Per-turn cold start.** Every prompt re-pays the `claude` (Bun) process boot + V8-heap
   build — the dominant slice of agent latency, not the API round-trip.
2. **Nothing warm to snapshot.** Between turns there is *no* `claude` child at all, only the
   lightweight Rust adapter. So a memory snapshot taken at idle captures nothing heavy.

[ADR 0022](0022-runtime-memory-sharing-and-forking.md) made each template's base guest memory
**File-restorable** from a page-cache-warm, NVMe-resident memfile shared across siblings —
turning restore into an in-kernel page-in with zero UFFD round-trips. But the base snapshot is
captured **pre-harness** (`PooledBackend::build_base_snapshot` runs `create → wait_agent_ready
→ snapshot`, agentd-only — `lib.rs` "without spawning a session harness"). So 0022 can restore
warm memory fast, but there is **no warm agent in the base to reap**. The big lever 0022
deferred ("if a change touches session identity, stop") is exactly this: capture a warm agent
into the base so File-restore yields a *ready* one.

## Decision

Make the harness **persistent** (hold one `claude` child across turns via
`--input-format stream-json`), then **warm-capture a generic, prompt-less, secret-free Claude
into the per-template base snapshot**, and **late-bind** per-session identity to the
already-running warm harness on restore. Reap it through 0022's File-backend: first-prompt
Bun-boot collapses and the warm Bun code pages share across siblings.

Three sub-decisions:

1. **Stay Rust-native.** We evaluated the [Claude Agent SDK](https://docs.claude.com/en/api/agent-sdk)
   (TypeScript/Python). It bundles the *same* `claude` CLI and would give full functionality
   parity, and its "Hybrid sessions" pattern matches our idle→resume model. But running it
   means a **per-session Node/V8 host process inside every guest** — a second heap on top of
   `claude`'s, directly opposing 0022's density goal. So we keep our Rust supervisor and
   **cherry-pick** the SDK's good ideas: the `--input-format stream-json` persistent transport
   (which is exactly how the SDK drives the CLI), and — as a later option — a SessionStore-style
   transcript externalization. Zero functionality is lost: it's the same binary either way.

2. **Warm-capture into the BASE template snapshot** (not merely per-session resume snapshots),
   with a **late-bind** command delivering `(session_env, first prompt)` to the running warm
   harness. This is the maximum-payoff target: every `session.create` reaps it.

3. **Move the Claude OAuth token to Broker secret mode** (in scope here). Today it is **Literal**
   — verified: `inject_user_claude_token` does `session_env.insert("CLAUDE_CODE_OAUTH_TOKEN",
   plain)` (`sessions.rs`), bypassing `secret_mode` entirely, and per-request `secrets` are
   Literal-only. The shared **base** snapshot is secret-safe regardless (no owner/token exists at
   enable-time capture), but the *running* persistent Claude and any *per-session* idle-evict
   snapshot would otherwise hold the real token in guest RAM. Broker mode (random placeholder in
   `session_env`; the per-session egress proxy substitutes the real value only on the wire to
   `api.anthropic.com`, already allow-listed for `demo-claude`) makes them secret-free — and is
   exactly the shape the Phase B spike already ran against.

**Boundary.** Forking remains a later ADR. This lands the late-bind / session-re-identity
machinery a fork also needs, but no duplicate-side-effect or per-child-network-identity
semantics.

## Evidence (spiked before proposing)

- **Socket survival across idle→resume (network primitive).** A held TCP/TLS connection reused
  after an FC pause→resume fails with an **immediate RST** (`dt ≈ 0.00 s`), not a black-hole; a
  fresh connection works immediately. The feared "long idle breaks sockets" is **trivially
  recoverable** (retry / drop the idle pool). Test:
  `engram-sandbox-firecracker/tests/socket_resume.rs` (manual `#[ignore]`).
- **Real persistent `claude` survives idle→resume (end-to-end).** The real binary, in
  `--input-format stream-json` mode, through the real egress proxy to real `api.anthropic.com`,
  **survives FC pause/resume**: across one pre-freeze and two post-resume turns it stays ALIVE
  and every turn round-trips; post-resume turns complete in ~116–139 ms (vs ~306 ms cold). Test:
  `engram-host-agent/tests/e2e_harness.rs::e2e_persistent_claude_socket_across_resume` (manual).
  `IS_SANDBOX=1` is required (claude refuses `--dangerously-skip-permissions` as root); the
  stream-json input line is `{"type":"user","message":{"role":"user","content":"…"}}`.
- **Warm-harness footprint (density honesty).** `claude` is a ~243 MiB Bun ELF; a run is ~66 MiB
  file-backed code (shares across siblings as `Shared_Clean`) + ~137 MiB V8 heap (anonymous,
  COW-diverges per session). So warm-capture's win is **broad first-prompt latency**, with only
  **modest cross-sibling density** (code shares; heap does not). See
  `docs/adr/0022-warm-harness-spike-notes.md`.

The decisive synergy: with the persistent rewrite, the warm `claude` child is alive at idle, so
it lands in the base snapshot; and *before its first turn* it holds **no live socket** (claude
dials only on the first user message), so the base capture is connection-clean.

## Architecture (phased — see the implementation plan for file/line detail)

- **P1 Persistent harness.** Hold one `claude` child (lives across vsock reconnects in `entry()`,
  so it survives FC pause/resume — the warm-snapshot prerequisite); feed each `Prompt` as a
  stream-json user line into its kept-open stdin; per-turn boundary = the `{"type":"result"}`
  frame (not EOF). EOF now means **the child crashed** → author crash-detection here
  (`describe_exit` + a System transcript message), then respawn + `--resume` from the persisted
  session id. `Interrupt` becomes SIGINT-the-turn-but-keep-the-process-alive. A mid-turn vsock
  drop drains the turn to its boundary (so claude never wedges on a full stdout pipe) before
  reconnecting.
- **P2 harness credential → Broker.** A generic `broker_secret(env, session, name, real,
  allow_hosts)` primitive injects a deterministic placeholder into the guest env and returns an
  `EgressSecretEntry`; `inject_harness_broker_secret` dispatches per harness (today: the built-in
  Claude harness's per-user OAuth token → `CLAUDE_CODE_OAUTH_TOKEN`, allow `api.anthropic.com`).
  The entry is folded into the create + resume `SessionEgressPolicy`. **No `secret_mode` flip
  needed** — the proxy's `decide()` MITMs/substitutes purely on a matching `SecretEntry`, ignoring
  `secret_mode` — so this is independent of the image's declared-secret mode and brokers
  unconditionally for builtin-claude sessions. (The substitution chain was already wired via
  ADR 0006; the prior "broker not wired" comments were stale.)
- **P3 Late-bind.** New `HarnessCommand::Bind { session_id, session_env, first_prompt }` (kept
  harness-agnostic — `session_id` is the engram identity already in `HarnessAttach`, no
  `claude_session_id`: a prompt-less warm capture never ran a turn, so `claude` auto-assigns + persists
  its own id on the first real turn). The harness adopts the bound `session_id` for subsequent
  `HarnessAttach`es, stores `session_env` and layers it onto each `claude` spawn (the warm child booted
  with placeholder/template env only — `.envs(bound_env)` before the fixed invariants), and runs
  `first_prompt` as the first turn (else stays idle). `HarnessHub::bind()` sends it over the existing
  vsock channel, mirroring `send_prompt`'s attach-wait. **Inert until P4** wires warm-capture to call
  it; the cold path keeps `SpawnHarness` (which already seeds the full per-session env into the harness
  process) + `Prompt`. NOTE for P5: a respawn-on-bind rebuilds the V8 heap — if measurement shows that
  erases the warm-heap win, deliver the env without a full respawn.
- **P4 Warm-capture + restore fork.** In `build_base_snapshot`, gated on
  `ENGRAM_WARM_HARNESS_CAPTURE`, spawn a generic harness + a new `wait_harness_warm` readiness
  gate before `snapshot`; on any warming failure, fall back to a cold (today's) capture. Persist
  `warm_harness: bool` on `SnapshotMetadata` (migration 0055; `serde(default)=false` ⇒ old
  snapshots are cold → automatic fallback). On restore, if `warm_harness` → deliver `Bind`
  instead of `SpawnHarness` (the restored agentd already references the running warm child, so it
  must not respawn); else the unchanged cold path (always a correct fallback).
- **P5 File-backend synergy + measurement.** Confirm the warm pages restore under
  `ENGRAM_FC_BASE_RESTORE_MODE=file`; measure first-prompt latency (warm vs cold) + Σpss/Σrss.

## Consequences

- **First-prompt latency** drops for every `session.create` (no Bun boot / heap build on the
  first turn); **follow-up latency** drops too (no per-turn re-spawn). These, not density, are
  the headline win.
- **Density** improves modestly: the ~66 MiB Bun code shares across siblings; the ~137 MiB V8
  heap is per-session (COW-diverges). Honest, and still net-positive at rest.
- **Secret posture improves**: with Broker, the running Claude and per-session snapshots hold
  only a placeholder; the real token never enters guest RAM (defense-in-depth vs prompt-injection
  exfiltration, the original Broker rationale).
- **Every prod-facing change is kill-switched** (`ENGRAM_WARM_HARNESS_CAPTURE`,
  `ENGRAM_WARM_HARNESS_BIND`) and falls back to today's cold spawn; old base snapshots read as
  cold automatically. Content-keyed reuse means enabling warm-capture requires a fresh capture.
- **Follow-up (separate ADR):** host-side egress-proxy **upstream connection pooling** would kill
  the network-handshake half of resume/turn latency, harness-agnostic. Out of scope here.
- **Follow-up (separate ADR): manifest-declared harness broker credentials.** P2's
  `inject_harness_broker_secret` dispatches in code (`match harness.name { "claude" => … }`) with the
  env-var name + allow-hosts hardcoded per harness. The clean end state is to declare the broker
  credential as **data in the harness manifest** — `(env var, allow_hosts, source)`, where `source`
  is e.g. "the session owner's saved OAuth token of kind K" — and have the coord resolve + broker it
  generically with no per-harness code. That needs a harness-manifest schema addition + a per-user-
  token secret *source* in the SecretStore resolver (which must learn the session owner/principal).
  Deferred to keep this ADR scoped; the `broker_secret` primitive is already harness-agnostic, so
  this is purely about moving the credential *spec* from code to manifest.

## Risks

1. **Generic-at-capture vs real-at-bind session-id mismatch** (the crux): the warm harness
   re-attaches post-restore under a sentinel session id; the host reconnect lookup keys on it, so
   restore must register sentinel→sandbox until `Bind` delivers the real id, and tolerate the id
   changing post-bind.
2. **Interrupt-survival of persistent claude** is unverified (does SIGINT end the turn with a
   terminal `result`, or kill the process?); the rewrite handles both.
3. **EOF semantics invert** (turn-done → child-crashed) — the highest-likelihood correctness bug.
4. **agentd `/exec` + ttyd session_env staleness**: they must get the per-session real env on
   restore (via `merge_session_env`), not the placeholder captured in the base.
5. **A warm-bound session's *own* idle-evict snapshot must take the cold path** — `warm_harness`
   is stamped only on enable-time base captures, never on session eviction captures.

## Commit chain

(ADR-bookend — filled as phases land; flip Status → Accepted at the end with measured numbers.
SHAs are current-as-of-rebase onto `main` #82.)

- P0a — carry over the Phase A/B spike tests as reference/regression
- P0b — this ADR (Proposed)
- P1 — persistent harness rewrite (`engram-harness-claude`): compiles + 5 unit tests + clippy green
- P2 — broker the harness credential (`broker_secret` + `inject_harness_broker_secret`); coord units +
  `intercept_e2e.rs` substitution tests (commit 1a34093)
- P3 — late-bind protocol: `HarnessCommand::Bind` + harness handler (adopt id, layer env, run first
  prompt) + `HarnessHub::bind()`; proto round-trip + 2 hub unit tests; dev-vm clippy + 5 adapter
  unit tests green. Inert until P4.
- _P4…P6_ pending
