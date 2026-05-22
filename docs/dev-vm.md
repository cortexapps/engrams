# Dev-vm — local iteration loop

The dev VM (`engram-dev` in `us-west2-a`, KVM-capable Linux) is the
daily-driver target for engrams development. It runs the full
prod-shape stack — coord (`mode=coordinator`) + host-agent over
gRPC, fake-gcs-server for the blob backend, NBD-served chunked
rootfs, local OCI registry, and the web SPA — bound to localhost
on the VM, with auth disabled.

The recipes assume the [`dev-vm` Claude skill](../.claude/skills/dev-vm/SKILL.md)
is installed (Mutagen sync, `gcloud` SSH config). All commands
below run **on the dev VM** unless noted otherwise.

## TL;DR

```bash
# laptop side
bash .claude/skills/dev-vm/scripts/start.sh         # boot the VM
bash .claude/skills/dev-vm/scripts/sync-start.sh    # start Mutagen

# dev-vm side (via run.sh or a direct SSH)
bash .claude/skills/dev-vm/scripts/run.sh just integration-up
bash .claude/skills/dev-vm/scripts/run.sh just integration-session

# laptop side — port-forward + open
bash .claude/skills/dev-vm/scripts/portforward.sh
open http://localhost:5173
```

## What gets brought up

`just integration-up` (script: `deploy/dev/integration-up.sh`) brings:

| Service | Port  | Backed by |
|---|---|---|
| Postgres | 5435 → 5432 | `docker compose -f deploy/docker-compose.dev.yml` |
| fake-gcs-server | 4443 | docker compose (acts as the blob backend) |
| local OCI registry | 5001 | docker compose (anonymous push/pull) |
| coordinator | 8090 | `engram-coordinator`, `mode=coordinator`, GCS blob |
| host-agent | 9101 (gRPC), 9100 (metrics) | `engram-host-agent`, FC backend |
| web SPA | 5173 | `pnpm dev` (vite), proxies `/sessions`+`/api` to coord |

NBD is auto-detected: any `/dev/nbd*` device the dev-vm kernel
exposes is fed into `ENGRAM_NBD_DEVICES`. Modify `nbds_max` on the
nbd module kernel cmdline to expand the slot count.

The host-agent runs under `sudo -E` for `CAP_NET_ADMIN` (TAP
creation). Passwordless sudo is required on the dev-vm.

## Daily workflow

1. **First time on a fresh VM**: see the [dev-vm skill's "Fresh VM"
   workflow](../.claude/skills/dev-vm/SKILL.md). Bootstraps Mutagen,
   installs Firecracker + nix, caches the FC test artifacts.

2. **Start of day**:
   ```bash
   bash .claude/skills/dev-vm/scripts/start.sh
   bash .claude/skills/dev-vm/scripts/sync-start.sh
   bash .claude/skills/dev-vm/scripts/run.sh just integration-up
   bash .claude/skills/dev-vm/scripts/portforward.sh   # laptop, foreground
   ```

3. **Iterate on a session**:
   ```bash
   # Bring up (or reuse) a session at HEAD's commit. Idempotent.
   bash .claude/skills/dev-vm/scripts/run.sh just integration-session

   # With a harness + initial prompt:
   bash .claude/skills/dev-vm/scripts/run.sh \
       env HARNESS=claude PROMPT='read /etc/os-release' just integration-session

   # Hit the API or the SPA directly:
   curl localhost:8090/sessions
   open http://localhost:5173
   ```

4. **After code changes**:
   ```bash
   # Mutagen syncs the source tree automatically; rebuild + restart
   # is just the integration-up cycle:
   bash .claude/skills/dev-vm/scripts/run.sh bash -c 'just integration-down && just integration-up'
   ```

5. **Reset to a clean slate** (drops DB volumes, fake-gcs bucket,
   sandbox state, baked images):
   ```bash
   bash .claude/skills/dev-vm/scripts/run.sh just integration-reset
   ```
   Use this between substantial schema changes, or when an
   on-disk image cache or chunk cache is interfering with the
   current bake.

6. **End of day**:
   ```bash
   bash .claude/skills/dev-vm/scripts/run.sh just integration-down
   bash .claude/skills/dev-vm/scripts/stop.sh
   ```

## Recipes at a glance

| just recipe         | what it does                                            |
|---------------------|---------------------------------------------------------|
| `integration-up`    | Brings up Postgres + fake-gcs + registry + coord + host-agent + web SPA. Idempotent on already-up services. |
| `integration-down`  | Stops the four processes + composes-down (volumes intact). |
| `integration-reset` | Hard reset: down + drop volumes + wipe `./var/integration`, `./var/host-sandboxes-integration`, etc. |
| `integration-test`  | Bake → enable → cascade → create session → delete session. Asserts the cascade lands a templates row. |
| `integration-session` | Like integration-test but keeps the session alive. Idempotent: reuses image + session at HEAD. |

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
  in some scenarios. `integration-up.sh` sets `CI=true` to skip the
  prompt. If you re-run with `pnpm install` manually outside the
  recipe, do the same.

- **Mutagen sync stalls** if you've edited a large untracked dir
  (e.g. `target/` getting excluded after the fact). Check with
  `.claude/skills/dev-vm/scripts/sync-status.sh`; force-reconcile
  with `sync-flush.sh`.

- **`./var/host-sandboxes-integration/` is root-owned** because the
  host-agent runs under `sudo`. `integration-reset` uses `sudo rm
  -rf` to handle this. Don't try to `rm -rf` it as your normal user.

- **NBD devices on the dev-vm: only 4 by default.** Bump
  `nbds_max` if you need more concurrent sandboxes. Production
  hosts run with `nbds_max=64`.
