# Dev-vm — local iteration loop

The dev VM (`engram-dev` in `us-west2-a`, KVM-capable Linux) is the
daily-driver target for engrams development. It runs the full
prod-shape stack — coord (`mode=coordinator`) + host-agent over
gRPC, fake-gcs-server for the blob backend, NBD-served chunked
rootfs, local OCI registry, and the web SPA — bound to localhost
on the VM, with auth disabled.

It is the same `just dev` used elsewhere: one orchestrator (Tilt),
backend auto-detected per host (ADR 0024). On the dev-vm the probe finds
`/dev/kvm` and selects Firecracker + the prod-shape split topology; no
flag, no per-arch recipe. Tilt comes from the nix devShell.

Run these workflows from an Engrams session whose restricted profile grants the
dev VM Google Cloud connection. The session bundle supplies `gcloud`, OpenSSH,
and Mutagen. Google authentication uses brokered metadata ADC. Do not run
`gcloud auth login`, import a credential file, create a service-account key, or
depend on a laptop Google login.

## TL;DR

```bash
# Use the exact project and zone from the connection grant.
gcloud compute instances start engram-dev \
  --project <project-id> --zone <zone>

# Every SSH connection uses IAP. Long-running jobs belong in tmux.
gcloud compute ssh engram-dev --project <project-id> --zone <zone> \
  --tunnel-through-iap \
  --command "tmux new-session -d -s engram 'cd ~/engrams && nix develop --command just dev'"

# Run one command on the VM.
gcloud compute ssh engram-dev --project <project-id> --zone <zone> \
  --tunnel-through-iap --command 'cd ~/engrams && nix develop --command just integration-test'

# Forward the web and Tilt ports into this session when needed.
gcloud compute ssh engram-dev --project <project-id> --zone <zone> \
  --tunnel-through-iap -- -N -L 5173:localhost:5173 -L 10350:localhost:10350
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

1. **First time on a fresh VM**: use a separately reviewed bootstrap task. The
   runtime connection does not grant VM creation, IAM changes, or service-account
   key creation. Install Firecracker and Nix on the VM, then cache the FC test
   artifacts.

2. **Start of day**:
   ```bash
   gcloud compute instances start engram-dev --project <project-id> --zone <zone>
   gcloud compute ssh engram-dev --project <project-id> --zone <zone> \
     --tunnel-through-iap \
     --command "tmux new-session -d -s engram 'cd ~/engrams && nix develop --command just dev'"
   ```
   Watch progress at http://localhost:10350 (Tilt UI) once port-forwarded.

3. **Iterate on a session**:
   ```bash
   gcloud compute ssh engram-dev --project <project-id> --zone <zone> \
     --tunnel-through-iap \
     --command 'cd ~/engrams && nix develop --command just bake-demo'
   gcloud compute ssh engram-dev --project <project-id> --zone <zone> \
     --tunnel-through-iap \
     --command 'cd ~/engrams && nix develop --command just integration-session'
   ```

4. **After code changes**: run Mutagen through a session-local SSH configuration
   whose `ProxyCommand` is `gcloud compute start-iap-tunnel ...
   --listen-on-stdin`. Do not use a public address or `gcloud compute
   config-ssh`. Rebuild one process from Tilt or restart the tmux session.

5. **Reset to a clean slate** (drops DB volumes, fake-gcs bucket,
   sandbox state, baked images):
   ```bash
   gcloud compute ssh engram-dev --project <project-id> --zone <zone> \
     --tunnel-through-iap --command \
     'cd ~/engrams && tilt down; docker compose -f deploy/docker-compose.dev.yml -f deploy/docker-compose.linux.yml down -v'
   ```
   (`var/host-sandboxes*` are root-owned — the host-agent ran under sudo.)

6. **End of day**:
   ```bash
   gcloud compute ssh engram-dev --project <project-id> --zone <zone> \
     --tunnel-through-iap --command 'tmux kill-session -t engram'
   gcloud compute instances stop engram-dev --project <project-id> --zone <zone>
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
gcloud compute ssh engram-dev --project <project-id> --zone <zone> \
  --tunnel-through-iap \
  --command 'cd ~/engrams && nix develop --command just integration-test'
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

- **Mutagen sync stalls** if a large untracked directory was added before its
  ignore rule. Use `mutagen sync list` and `mutagen sync flush <name>` through
  the session-local IAP SSH configuration.

- **`./var/host-sandboxes*/` is root-owned** because the host-agent
  runs under `sudo`. The reset snippet above uses `sudo rm -rf`. Don't
  try to `rm -rf` it as your normal user.

- **NBD devices on the dev-vm: only 4 by default.** Bump
  `nbds_max` if you need more concurrent sandboxes. Production
  hosts run with `nbds_max=64`.
