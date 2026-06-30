# Dev-vm — local iteration loop

The dev VM (`engram-dev` in `us-west2-a`, KVM-capable Linux) is the
daily-driver target for engrams development. It runs the full
prod-shape stack — coord (`mode=coordinator`) + host-agent over
gRPC, fake-gcs-server for the blob backend, NBD-served chunked
rootfs, local OCI registry, and the web SPA — bound to localhost
on the VM, with auth disabled.

It's the same `just dev` you run on a laptop: one orchestrator (Tilt),
backend auto-detected per host (ADR 0024). On the dev-vm the probe finds
`/dev/kvm` and selects Firecracker + the prod-shape split topology; no
flag, no per-arch recipe. Tilt comes from the nix devShell.

The recipes assume the [`dev-vm` Claude skill](../.claude/skills/dev-vm/SKILL.md)
is installed (Mutagen sync, `gcloud` SSH config). All commands
below run **on the dev VM** unless noted otherwise.

## TL;DR

```bash
# laptop side
bash .claude/skills/dev-vm/scripts/start.sh         # boot the VM
bash .claude/skills/dev-vm/scripts/sync-start.sh    # start Mutagen

# dev-vm side — `just dev` (tilt up) is long-running, so launch it in
# tmux: SSH-spawned foreground jobs die on disconnect.
bash .claude/skills/dev-vm/scripts/ssh.sh \
  "tmux new-session -d -s engram 'cd ~/engrams && nix develop --command just dev'"

# bake the Claude demo image + create a session (once the stack is up)
bash .claude/skills/dev-vm/scripts/run.sh just bake-demo
bash .claude/skills/dev-vm/scripts/run.sh just integration-session

# laptop side — port-forward + open
bash .claude/skills/dev-vm/scripts/portforward.sh   # web 5173, coord 8090, registry 5001, tilt 10350
open http://localhost:5173        # SPA      |  http://localhost:10350  # Tilt UI
```

## What gets brought up

`just dev` (`tilt up`, see the `Tiltfile`) brings:

| Service | Port  | Backed by |
|---|---|---|
| Postgres | 5435 → 5432 | `docker compose -f deploy/docker-compose.dev.yml` |
| fake-gcs-server | 4443 | docker compose (+ the Linux host-net override) |
| local OCI registry | 5001 | docker compose (anonymous push/pull) |
| jaeger | 16686 (UI), 4317 (OTLP) | docker compose (ADR 0019 tracing) |
| coordinator | 8090 | `engram-coordinator`, `mode=coordinator`, GCS blob |
| host-agent | 9101 (gRPC), 9100 (metrics) | `engram-host-agent`, FC backend, under `sudo` |
| web SPA | 5173 | `pnpm dev` (vite); set `ENGRAM_SKIP_WEB=1` to drop it |

NBD is auto-detected: any `/dev/nbd*` device the dev-vm kernel exposes is
used. The host-agent runs under `sudo -n` for `CAP_NET_ADMIN` (TAP
creation) — passwordless sudo is required on the dev-vm (the Tiltfile's
serve_cmd is `cargo build … && exec sudo -n --preserve-env=… engram-host-agent`).

**Two-host (evac) mode:** `ENGRAM_INTEG_TWO_HOSTS=1 just dev` adds a
second host-agent (`host-agent-b`, gRPC 9102 / metrics 9110, disjoint NBD
half) so coord sees a 2-host cluster — what `just integration-evac-test`
needs.

## Daily workflow

1. **First time on a fresh VM**: see the [dev-vm skill's "Fresh VM"
   workflow](../.claude/skills/dev-vm/SKILL.md). Bootstraps Mutagen,
   installs Firecracker + nix, caches the FC test artifacts.

2. **Start of day**:
   ```bash
   bash .claude/skills/dev-vm/scripts/start.sh
   bash .claude/skills/dev-vm/scripts/sync-start.sh
   bash .claude/skills/dev-vm/scripts/ssh.sh \
     "tmux new-session -d -s engram 'cd ~/engrams && nix develop --command just dev'"
   bash .claude/skills/dev-vm/scripts/portforward.sh   # laptop, foreground
   ```
   Watch progress at http://localhost:10350 (Tilt UI) once port-forwarded.

3. **Iterate on a session**:
   ```bash
   bash .claude/skills/dev-vm/scripts/run.sh just bake-demo          # build + bake the Claude image
   bash .claude/skills/dev-vm/scripts/run.sh just integration-session # create (or reuse) a session
   curl localhost:8090/sessions
   open http://localhost:5173
   ```

4. **After code changes**: Mutagen syncs the source tree automatically.
   Rebuild + restart a single process from the Tilt UI (click its
   "rebuild") at http://localhost:10350, or restart the whole stack by
   killing + relaunching the tmux session.

5. **Reset to a clean slate** (drops DB volumes, fake-gcs bucket,
   sandbox state, baked images):
   ```bash
   bash .claude/skills/dev-vm/scripts/run.sh bash -c '
     tilt down || true
     docker compose -f deploy/docker-compose.dev.yml -f deploy/docker-compose.linux.yml down -v
     sudo rm -rf var/host-sandboxes var/host-sandboxes-b var/engram var/sandboxes'
   ```
   (`var/host-sandboxes*` are root-owned — the host-agent ran under sudo.)

6. **End of day**:
   ```bash
   bash .claude/skills/dev-vm/scripts/ssh.sh "tmux kill-session -t engram"
   bash .claude/skills/dev-vm/scripts/stop.sh
   ```

## Recipes at a glance

| just recipe           | what it does                                            |
|-----------------------|---------------------------------------------------------|
| `dev`                 | Full stack via Tilt (run in tmux on the VM). Backend auto-detected. |
| `dev-down`            | `tilt down` — stop the Tilt-managed processes + compose services. |
| `bake-demo`           | Bake `deploy/demo/` → local registry (harness-free; ADR 0062 — the `claude` harness rides the bundle stamp). |
| `pull-kernel`         | Fetch the kernel this host's backend needs. |
| `integration-test`    | Bake → enable → cascade → create session → delete. Run after `just dev`. |
| `integration-session` | Like integration-test but keeps the session alive. Idempotent at HEAD. |
| `integration-evac-test` | ADR 0018 M4 evac. Run after `ENGRAM_INTEG_TWO_HOSTS=1 just dev`. |

## Smoke-test the loop

```bash
bash .claude/skills/dev-vm/scripts/run.sh just integration-test
```

Times bake, enable+cascade, and session create. Expected on the
dev-vm with a warm fake-gcs bucket:

| Step | Wall  |
|---|---|
| bake demo image (musl agentd + canonical memory + push) | ~30 s |
| POST /api/enabled-images (full cascade + first warm slot) | ~60 s |
| POST /sessions (warm if cascade refilled; cold otherwise) | 1–60 s |

## Known gotchas

- **Cold session marked Active before agentd is reachable.**
  For `harness: none` sessions, coord emits `Pending → Active` ~14 ms
  after the FC microVM starts, but agentd inside the guest doesn't
  bind its vsock listener until kernel boot + init are done (~15 s
  later). Until then, `/exec` and `/shell` against the session
  return `read FC vsock CONNECT response: early eof`. This
  reproduces locally — fix lives in
  `crates/engram-coordinator/src/api/sessions.rs:839/852`
  (the `(None, _)` branches should run the agent_handshake before
  emitting Active).

- **TTY-less pnpm install** prompts about `confirmModulesPurge`
  in some scenarios. If you hit it, set `ENGRAM_SKIP_WEB=1` (drops the
  web resource) or run `CI=true pnpm install` once in `web/`.

- **Mutagen sync stalls** if you've edited a large untracked dir
  (e.g. `target/` getting excluded after the fact). Check with
  `.claude/skills/dev-vm/scripts/sync-status.sh`; force-reconcile
  with `sync-flush.sh`.

- **`./var/host-sandboxes*/` is root-owned** because the host-agent
  runs under `sudo`. The reset snippet above uses `sudo rm -rf`. Don't
  try to `rm -rf` it as your normal user.

- **NBD devices on the dev-vm: only 4 by default.** Bump
  `nbds_max` if you need more concurrent sandboxes. Production
  hosts run with `nbds_max=64`.
