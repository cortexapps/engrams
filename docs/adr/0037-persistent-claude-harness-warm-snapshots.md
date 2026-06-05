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
  `warm_harness: bool` on `SnapshotMetadata` (migration 0056; `serde(default)=false` ⇒ old
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
- P4a — `warm_harness` flag on `SnapshotMetadata` + `SnapshotRecord` + migration 0056
  (`snapshots.warm_harness`, `serde(default)=false`); persisted via `record_snapshot`, read by
  `snapshot_from_row`, wired into the base-capture record from the host's metadata. Inert (nothing sets
  it true until P4c). dev-vm `--workspace --all-targets` check + clippy green.
- P4b — no-respawn bind + file-delivered vsock tokens (`SESSION_ENV_FILE` + `read_session_var`; forge/
  share helpers file-first/env-fallback). dev-vm clippy + tests green.
- P4c — warm capture: `build_base_snapshot` gains `Option<AgentSpec>` (threaded trait→proto→coord);
  `build_warm_capture_agent_spec` (constant OAuth placeholder + sentinel id); `PooledBackend` gets the
  hub injected + `maybe_warm_capture` (sentinel-bind → start_agent → `wait_harness_warm` → snapshot
  `warm_harness=true`), gated `ENGRAM_WARM_HARNESS_CAPTURE`, fallback-cold. dev-vm check + clippy + 3
  `wait_harness_warm` unit tests green.
- P4d — restore-fork: `LateBindHarness` RPC + `HostClient::late_bind_harness`; coord create-flow
  `warm_bind = ENGRAM_WARM_HARNESS_BIND && snapshot.warm_harness` ⇒ `apply_egress_policy` +
  `late_bind_harness` (constant-placeholder policy entry) instead of `start_agent`. dev-vm check +
  clippy green. **All of P4 is gated OFF by default (both kill-switches) ⇒ inert in prod.**
- P5a — sandbox-keyed `HarnessSink` (`Fn(SandboxId, stream)`): FC/VZ route the harness re-attach by the
  connection's sandbox, not the harness's self-reported session id, so a warm-restored harness (which
  re-dials under the sentinel session id baked at capture) routes correctly without a colliding
  per-session sentinel registration. **Validated on real FC**: `e2e_warm_capture_via_pooled_backend`
  passes (warm claude boots + attaches + Idle → snapshot `warm_harness=true`, ~73s).
- **P5b RESOLVED — the load-bearing risk the spikes missed (the agentd reconnect nudge).**
  The full warm loop — `e2e_warm_restore_bind_via_pooled_backend` — got through capture + restore but the
  restored warm harness never re-attached: after a full snapshot→destroy→restore (a NEW VM / vsock
  backend, *unlike* the pause/resume Phase A/B validated) the vsock the harness was attached to at
  capture is **black-holed with no RST/EOF** — so the harness sits on a hung idle read and its
  read-error-driven reconnect loop never fires (pause/resume gets an RST and recovers on its own).
  Diagnosed conclusively on the dev-vm: post-restore the harness + claude processes are *alive* (agentd
  pid 1, harness, claude in `D`), just stuck on the dead read. **Fix = a deliberate reconnect nudge over
  the existing vsock** (commit `88b65e7`): agentd `SIGUSR1`s the resident harness child (its tokio
  `Child` handle survives restore, `.id()` == resumed PID) via a new `ReconnectHarness` RPC the host
  fires inside `late_bind_harness` before the hub's attach-wait; the harness's idle `select!` wakes on
  the signal, drops the dead connection, and re-dials **immediately** (no backoff — a nudge is a
  deliberate reconnect, not a failure, which preserves the latency win). A passive keepalive/timeout was
  rejected: it would re-introduce the multi-second wait warm-capture exists to remove, and risks tripping
  the idle-evict soft TTL. **Root cause of the first red run:** `PooledBackend` didn't forward
  `reconnect_harness` to its inner FC backend, so the trait default (`Ok(false)`) silently no-op'd the
  nudge — the same missing-forward class as the `start_shell` prod incident; the forwarder is the fix.
  **Validated on real FC** (`ENGRAM_WARM_RESTORE_E2E=1`): warm base → restore → nudge → re-attach →
  late-bind → first prompt round-trips through the egress proxy in **~3.5s** (bogus token ⇒ a real 401
  `result` frame). Cold + pause/resume harness e2e unaffected; clippy clean. Test gated off CI (needs a
  real `claude` binary + proxy + root + Anthropic reachability); runs on the dev-vm.
- **P5 measurement DONE on dev-vm** (`e2e_warm_latency_and_density_via_pooled_backend`, gated
  `ENGRAM_WARM_RESTORE_E2E`): build one warm base, then cold-create (`start_agent`) vs warm-create
  (nudge + late-bind), and restore N=3 fresh File-backend siblings to sample memory.
  - **Cross-sibling density (the decisive result): Σpss/Σrss ≈ 94%** — i.e. minimal sharing. The
    smaps_rollup breakdown is unambiguous: only **~26 MiB/sibling is Shared_Clean** (cross-sibling),
    **~300 MiB/sibling is Private_Dirty**. FC runs un-chrooted so all siblings `MAP_PRIVATE` the one
    shared `memory.bin` inode — sharing *is* active (the 26 MiB proves it) — but the live Bun/V8
    runtime dirties ~90% of its ~326 MiB resident set on resume, so almost nothing stays shareable.
    **This confirms the density-honesty prediction (§Warm-harness footprint): the V8 heap COW-diverges
    per sibling; density is NOT the warm-capture win.**
  - **First-prompt latency: inconclusive on the dev-vm (confounded), do not quote.** Restoring the
    *same* warm base for both arms means the cold arm's respawned claude reads its Bun ELF from the
    GUEST page cache the warm capture already populated (host `drop_caches` can't reach the guest
    cache), so its "cold boot" isn't truly cold; and `prompt→RunCompleted` is dominated by claude's
    multi-second per-turn processing, which swamps the ~1s boot delta (observed deltas: +952ms, +672ms,
    −608ms — within noise). A clean number needs a separate cold base (no warm capture) **or the prod
    canary** — so first-prompt latency becomes a **P6 prod-canary gate**, not a dev-vm-closed one.
- _P6 (Accepted)_ pending — connection-recovery gate met, density measured (modest, as designed). The
  remaining gate is the **prod canary**: first-prompt latency warm-vs-cold on a real template (the only
  setup that exercises a genuinely-cold base against the warm base), behind the existing kill-switches.
  **Known follow-up (pitfall #4):** on the warm path agentd's `/exec` + ttyd shell keep the capture-time
  (sentinel) session env — FC `merge_session_env` is a documented no-op for the guest env; the agent
  loop is correct (forge/upload via the P4b file; OAuth via the baked constant placeholder), but
  shell/exec attribution needs a warm-path agentd env-merge RPC.

### Per-session identity delivery to a warm (no-respawn) harness — the P4 mechanism

The warm-heap win requires that bind **not** respawn `claude` (P3's respawn-at-bind is the inert
fallback; P4 replaces it with no-respawn). Two classes of per-session value, by transport:

- **HTTP-egress secrets (Claude OAuth token, future API keys):** ride the egress proxy. Bake a
  **constant, session-independent placeholder** into the warm capture's env; the per-session proxy
  policy maps placeholder→real **by guest IP** on the wire. The warm `claude` keeps the placeholder in
  its frozen env untouched — no respawn. This reconciles P2 (whose `broker_secret` mints a *per-session*
  placeholder): the warm-claude OAuth placeholder must be constant, and the restore-side policy uses
  that constant.
- **vsock capability tokens (`ENGRAM_FORGE_TOKEN`, `ENGRAM_UPLOAD_TOKEN`):** ride a separate guest→host
  vsock bridge the proxy never sees; the coord validates the *literal* token against
  `git_broker_tokens[session]`. A placeholder can't be substituted there. So the **real** per-session
  value is delivered into the guest at bind — written to a **per-session file on the COW disk**; the
  `engram-agentd` forge/share helpers read file-first, env-fallback (cold path unchanged). The
  identity-blind global forge/upload sink (`Fn(stream)`, trusts the request's `(session_id, token)`)
  means host-side auth-by-sandbox-identity would need a cross-backend sink refactor — out of scope.
