# Authoring an image's `[warm]` hook

An image's `engram.toml` can declare a `[warm]` block (see
[`WarmConfig`](../crates/engram-core/src/types/image.rs)): a command run
inside the capture VM, once `agentd` is ready, just before the base
snapshot is frozen. The command must **start its long-lived process
detached and then exit** — e.g. `gradle --daemon help` spawns the Gradle
daemon as a separate process and returns; that daemon stays alive and is
captured live into the base snapshot, so every restored session inherits
a warm, cache-hot daemon with no cold start.

This doc is the contract for writing one. Issue #539 hardened the
capture-time enforcement around it (stall detection, per-stage deadlines,
a carried output tail) — this is the companion "what a hook may assume,
and how to report progress" reference.

## What a `[warm]` hook may assume

- **A ready guest.** `agentd` has already reached its readiness dial;
  the image's rootfs/manifest `[env]` (`JAVA_HOME`, `PATH`, …) is merged
  with the resolved `capture_env` and injected into the hook's exec
  environment.
- **Egress per `[warm.network]`.** Absent (the default) → the capture VM
  is **egress-less**: no policy is registered, so the proxy denies all
  traffic. An image whose warm boot needs the network (eager OIDC
  discovery, an `op inject`) opts in via `[warm.network] default =
  "allow"` (dev posture — no agent runs at capture) or a scoped `"deny"`
  + `allow_hosts`/`allow_host_patterns` allowlist.
- **No per-session secrets.** The capture VM's `agentd` holds no durable
  session env and never gets a session bind. `capture_env` (an
  admin-attached, capture-time-only env — literals or secret refs
  resolved against the same `SecretStore` a session uses) is the only
  secret-bearing input a `[warm]` hook gets; it is distinct from a
  session's profile-injected runtime secrets and is captured (frozen)
  into the base snapshot along with everything else the hook does, so
  treat it as secret-bearing storage (ADR 0007).
- **No TTY.** The hook runs as a plain exec, not an interactive shell.
- **A fresh VM per attempt.** Every capture attempt (including a scanner
  retry) boots a brand-new capture VM from a fresh image pull — the hook
  re-runs from scratch on retry. Any external side effect it has (a
  webhook call, a registration ping) must be **idempotent under
  replay**.
- **Must exit.** A hook that starts a daemon and returns is expected; a
  hook that itself runs forever is fail-loud (see below) — it will be
  killed and the capture aborted.

## Fail-loud, by design

A non-zero exit, a stall, a blown stage deadline, or the global timeout
all abort the capture and the enable — the platform never ships a "cold"
base snapshot that a `[warm]` hook claimed to warm. There is no partial
credit: fix the hook, retry the enable (`RetryEnableJob`).

## Deadline semantics (issue #539)

Three independent budgets apply, tightest-wins:

- **Stage deadline** (`deadline_secs` on a `start` line, per stage):
  fires on elapsed wall-clock time since the stage started, *regardless
  of output* — a chatty-but-stuck stage still dies on budget. Only set
  if the stage declares one.
- **Stall** (`ENGRAM_WARM_STALL_SECS`, host-configured, default 120s):
  fires when **no stdout/stderr bytes and no progress line** have
  arrived for the stall budget. Armed only once the hook has emitted its
  first valid progress line ("conforming") — a hook that never emits one
  is unaffected by stall detection; it gets the pre-#539 behavior
  (single global timeout only). **Any stage that can *wait*
  (a `kubectl wait`-style condition, a poll loop) must emit heartbeats,
  or the platform can't tell "slow but alive" from "wedged".**
- **Global timeout** (`WarmConfig.timeout_secs`, default 600s, set in
  `engram.toml`): the same backstop this always had — `agentd` SIGKILLs
  the child in-guest at this deadline regardless of the other two. Has
  supremacy: it fires even if a stage/stall budget would otherwise allow
  more time.

## The progress protocol

A hook that wants observability during a long capture (which is any
hook worth having a stage/stall budget) emits sentinel-prefixed lines on
**stdout**. Space-separated `key=value` tokens; `msg=` consumes to
end-of-line (so it must come last); unknown keys are ignored
(forward-compatible); a line that doesn't parse is treated as ordinary
output (tail-captured, resets the stall clock, but is not a stage
transition):

```text
::engram-warm:: event=start stage=<name> [deadline_secs=<u64>] [msg=<free text>]
::engram-warm:: event=heartbeat [stage=<name>] [msg=<free text>]
::engram-warm:: event=done stage=<name>
```

- `start` implicitly closes the previous stage (you don't need to emit
  `done` before the next `start`); `deadline_secs` is *that stage's*
  hard budget.
- `heartbeat` resets the **stall** clock (proves the hook is alive) but
  **not** the stage deadline (a stuck-but-chatty stage still dies on its
  own budget) — use it inside anything that waits/polls.
- `done` closes the current stage explicitly and cleanly.

This rides the hook's **plain stdout** through the existing `exec`
stream — no `engram-agentd` wire change, no image re-bake needed for the
protocol itself (baked `agentd`s in existing base snapshots keep
working). stderr is captured into the output tail and resets the stall
clock like any other output, but is not parsed for progress lines.

### Example

```sh
#!/bin/sh
set -e
echo "::engram-warm:: event=start stage=deps-up deadline_secs=120"
./install-deps.sh
echo "::engram-warm:: event=done stage=deps-up"

echo "::engram-warm:: event=start stage=migrations deadline_secs=60"
./run-migrations.sh
echo "::engram-warm:: event=done stage=migrations"

echo "::engram-warm:: event=start stage=wait-ready deadline_secs=300"
until curl -fsS localhost:8080/healthz >/dev/null 2>&1; do
    echo "::engram-warm:: event=heartbeat stage=wait-ready msg=still waiting on healthz"
    sleep 5
done
echo "::engram-warm:: event=done stage=wait-ready"

# Start the long-lived process detached, then exit — it's captured live.
nohup ./run-server.sh >/var/log/server.log 2>&1 &
disown
exit 0
```

## What you get for it

- A live capture shows `capture_phase`/`warm_stage` on the `enable_jobs`
  row (and the app-gRPC `EnableJob`), updated at least every 30s even
  during a healthy silent wait.
- A failure — including a stall kill, a stage-deadline kill, or the
  in-guest global-timeout SIGKILL — leaves the failing stage name and
  the hook's last 16 KiB of combined stdout+stderr on the row
  (`warm_stage`/`output_tail`). No host-log access required to diagnose
  it.

## Backend coverage

The watchdog lives above the `SandboxBackend` seam (in
`PooledBackend::run_warm_hook`) and uses `exec_stream`, which every
backend (Firecracker, VZ, Process) implements — stall detection, stage
deadlines, and tail capture apply identically regardless of which
backend is capturing. `build_base_snapshot` itself is implemented only
on `PooledBackend`; VZ and Process opt out of base-snapshot capture
entirely via the trait's default (a hard error), same as before this
issue.
