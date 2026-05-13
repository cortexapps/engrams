# Engram demo runbook

A 5-minute walkthrough that exercises every Phase 4 surface
end-to-end on macOS using `ProcessBackend` + the `engram-harness-noop`
adapter — no Firecracker, no real Claude binary, no GCP.

The point is to give you a runnable thing you can poke at while
finding bugs to fix. Real production runs the same code paths
against `engram-sandbox-firecracker` + a real harness adapter; the
TCP transport here stands in for vsock.

## Prereqs

- Docker (for Postgres) + `cargo` + `just`.
- Optional: `cargo install cargo-nextest --locked` for the test gate.

## 1. Start the stack

```bash
just dev
```

This brings up Postgres in Docker, builds `engram-harness-noop`, and
runs the coordinator with:

- `ENGRAM_MODE=all` — single-binary; the host-agent runs in the
  same process as the coordinator.
- `ENGRAM_SANDBOX_BACKEND=process` — sandboxes are subprocesses
  rooted in `./var/sandboxes/<sandbox-id>/`.
- `ENGRAM_DEV_AUTO_NOOP=1` — every new session auto-spawns the
  noop harness adapter, which dials the host-agent's harness TCP
  listener.
Sessions go through the chunked-OCI cold path on every create
(warm pools were retired with ADR 0008; chunk-cache hits keep the
hot path fast).

Wait until you see:

```
INFO engram_host_agent::harness: harness TCP listener bound addr=127.0.0.1:NNNNN
INFO engram_coordinator: coordinator listening addr=127.0.0.1:8090
```

## 2. Create a session

In another terminal:

```bash
SID=$(engram --endpoint http://localhost:8090 session create \
  --repo local://hello --branch main)
echo "session: $SID"
```

The coordinator will:
1. Persist a `sessions` row with `session_kind=local`.
2. Build a `SandboxSpec` with `agent: Some(AgentSpec { argv: [...,
   --connect HOST:PORT, --session-id $SID], env: {...} })`.
3. Have ProcessBackend spawn the `engram-harness-noop` binary as
   a child rooted in the sandbox cwd.
4. The noop adapter dials the harness listener over TCP, sends
   `HarnessAttach { session_id, harness_version }`, and starts
   emitting tool calls.

Within a couple of seconds you can watch traffic land:

```bash
engram session log $SID
```

You'll see `run_started` → `tool_call_started` →
`tool_call_completed` (×3 by default) → `harness_idle`.

## 3. Watch idle eviction

The noop harness emits 3 tool calls at 5s intervals, then falls
silent. With `ENGRAM_IDLE_TTL_SECS=60` (default), the idle
evictor will hot-suspend the session ~60s after the last harness
event:

```bash
sleep 75
engram session get $SID
# status should now be `idle`
```

Behind the scenes:
- `idle_evictor` polled `harness_hub.idle_sandboxes(60s)`,
- ran the cold checkpoint primitive (no-op for `local://`),
- snapshotted the sandbox to local NVMe (`ProcessBackend::snapshot`
  tarballs the cwd),
- destroyed the sandbox (which SIGKILLs the noop child),
- marked the session `idle`.

## 4. Auto-resume on next request

Run a command — auto-resume kicks in transparently:

```bash
engram session exec $SID "echo hello-from-\$ENGRAM_SESSION_ID"
```

The exec handler called `ensure_active`, which restored from the
local snapshot (sub-second), rebound the registry, and ran the
exec. `engram session get $SID` is back to `active`.

## 5. Conversation timeline

```bash
engram session log $SID --limit 20
```

Every harness event is a row in `session_events`. The conversation
is the source of truth — the workspace (for Git sessions) is the
parallel artifact pushed at checkpoint time.

For a Git session you'd also have:

```bash
engram session log $SID --kind workspace   # git log of the checkpoint branch
engram session diff $SID --vs main         # workspace diff vs base
engram session fork $SID --at 3            # branch the agent at event 3
```

## 6. Tear down

```bash
engram session delete $SID
just db-down                # if you want Postgres gone too
```

## Known issues surfaced by this runbook

These are the bugs the demo found. Track them as cleanup items
before the post-Phase-4 review.

1. **`hosts.hostname UNIQUE` + `id` not in `ON CONFLICT` made the
   in-process host id drift across restarts.** Worked around by
   pinning `--mode=all` to a stable HostId
   (`00000000-0000-4000-8000-000000000a11`). The trait-level fix
   is to either drop the UNIQUE on hostname or update `id` in the
   ON CONFLICT clause.
2. **`pending_reassign` wasn't a recognised wire string** in
   `parse_session_status` even though the enum had the variant.
   Fixed in this pass.
3. **`--mode=all` didn't heartbeat its in-process host**, so the
   dead-host detector reaped it after ~30s. Fixed by stamping
   `last_heartbeat_at` on a 5s tick.
4. **`ENGRAM_DEV_AUTO_NOOP=1` was rejected by clap's default bool
   parser** — it expected `true` / `false`. Fixed with
   `BoolishValueParser`.

## Where to look next

- `crates/engram-coordinator/src/api/sessions.rs` — `create_session`
  is where `AgentSpec` is built and the `harness_hub.bind_session`
  call happens.
- `crates/engram-host-agent/src/harness.rs` — the hub. The
  TCP listener, `bind_session`, and the `accept_via_session_lookup`
  path live here.
- `crates/engram-sandbox-process/src/lib.rs` — `create()` /
  `destroy()` honour `SandboxSpec::agent`. Logs go to
  `<cwd>/agent.log`.
- `crates/engram-coordinator/src/idle_evictor.rs` — polls
  `harness_hub.idle_sandboxes(ttl)` every 10s.
- `crates/engram-harness-noop/src/main.rs` — what the dev
  harness looks like end-to-end.
