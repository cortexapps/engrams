---
name: dev-vm
description: Manage the GCP Linux dev VM (engram-dev) — start/stop, bootstrap, mirror the local repo with Mutagen, and run commands inside its Nix shell. Use whenever the user says "/dev-vm <subcommand>" or asks to "run X on the VM", "start/stop the VM", "sync to the box", etc.
---

# dev-vm — local-only skill for the GCP Linux dev VM

This skill is **not committed** (gitignored under `/.Codex/`). Each user's
copy is personal: VM identity, paths, and the Mutagen session live here.

The VM (`engram-dev` in `cortex-test-1608327238078`/`us-west2-a`) is the
KVM-capable box we use for Phase 2 / Firecracker work. Local Mac edits,
remote Linux build/test — Mutagen keeps the two trees in sync.

## When to use

Invoke this skill when the user asks you to:
- start, stop, or check status of the dev VM
- bootstrap a fresh VM (or repair a drifted one)
- start/stop file sync between the laptop and the VM
- run any command "on the VM" or "over there" (cargo, just, nix, etc.)
- SSH into the box

Don't use for anything that runs on the laptop, or for long-lived servers.

## Subcommands

Pass the subcommand as the skill argument. Each maps 1:1 to a script in
`scripts/`. Run them with `bash <path>`:

| arg                 | script                       | what it does                                         |
|---------------------|------------------------------|------------------------------------------------------|
| `bootstrap-local`   | `scripts/bootstrap-local.sh` | Install Mutagen + write gcloud SSH config entries.   |
| `bootstrap-remote`  | `scripts/bootstrap-remote.sh`| Install Nix, Firecracker, gh, Docker on the VM.      |
| `start`             | `scripts/start.sh`           | Start the VM, poll until sshd is up.                 |
| `stop`              | `scripts/stop.sh`            | Pause Mutagen, stop the VM.                          |
| `status`            | `scripts/status.sh`          | One-screen VM + sync state.                          |
| `ssh [cmd...]`      | `scripts/ssh.sh`             | Drop into a shell, or run a single command.          |
| `run <cmd...>`      | `scripts/run.sh`             | Flush sync, run cmd in `nix develop` over there.     |
| `sync-start`        | `scripts/sync-start.sh`      | Create/resume the Mutagen session.                   |
| `sync-stop`         | `scripts/sync-stop.sh`       | Terminate the Mutagen session.                       |
| `sync-status`       | `scripts/sync-status.sh`     | Detailed sync state (conflicts, problems).           |
| `sync-flush`        | `scripts/sync-flush.sh`      | Force a reconcile pass; block until both sides agree.|
| `portforward`       | `scripts/portforward.sh`     | IAP tunnels for web (5173), coord (8090), registry (5001). Foreground; Ctrl-C tears down. |

`config.sh` is sourced by every other script and holds the constants
(project, zone, instance name, remote dir). Override via env if needed.

## How to dispatch

When invoked with an argument like `start`, run the matching script:

```bash
bash .Codex/skills/dev-vm/scripts/start.sh
```

When invoked with `run <cmd...>`, forward the rest as one shell command:

```bash
bash .Codex/skills/dev-vm/scripts/run.sh cargo check --workspace
```

Argument-less invocation: print this file's "Subcommands" table so the
user can pick.

## Typical workflows

**Daily start**
```
/dev-vm start
/dev-vm sync-start
# edit code on the Mac
/dev-vm run cargo check --workspace
```

**Daily stop**
```
/dev-vm stop          # pauses Mutagen + stops VM (~$3/mo while stopped)
```

**Fresh VM** (after delete + recreate, or first time on a new laptop)
```
/dev-vm bootstrap-local
/dev-vm start
/dev-vm bootstrap-remote
# clone the repo on the VM yourself: gh repo clone cortexapps/engrams ~/engrams
/dev-vm sync-start
# cache the Firecracker test artifacts (vmlinux + ubuntu rootfs) so
# `cargo test -p engram-sandbox-firecracker -- --ignored` and the
# `just fc-bake-demo` / `just dev-firecracker` recipes can find them
/dev-vm run bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh
```

**Firecracker integration smoke test** (after bootstrap)
```
/dev-vm run bash crates/engram-sandbox-firecracker/scripts/run-boot-test.sh all
```
Runs the five FC integration tests (boot, lifecycle, snapshot,
snapshot_uffd, exec_real_vm) against real microVMs.

**Drive the coordinator API on Firecracker** — see
`docs/demo-firecracker.md` for the full runbook. Short version:
```
/dev-vm run just fc-bake-demo                # build + bake an agent-baked image
/dev-vm run just db-up
# start the coord in tmux (SSH-spawned background jobs die on
# disconnect, so the runbook uses `tmux new-session -d -s engram ...`).
```

## Sync model

- **Direction**: two-way-resolved. Both sides editable; the more-recent
  edit wins on conflict.
- **Excluded**: `target/`, `node_modules/`, `var/`, `.sqlx/`, `.direnv/`,
  `result*`, `.DS_Store`, `*.swp`, `.Codex/`. These are build outputs /
  personal caches that shouldn't ping-pong. `node_modules/` in particular
  holds platform-specific native binaries (e.g. `@rollup/rollup-darwin-arm64`
  on the Mac vs the linux-x64 build on the VM), so each side runs its own
  `pnpm install` — syncing it two-way corrupts both. `.Codex/` is excluded
  because it's local Codex config + the (Mac-side) dev-vm skill + nested
  git worktrees — hundreds of MB the VM never needs.
- **`.git/`**: synced in **main-checkout mode** (so `git status` works on
  either side). **Excluded in worktree mode** (see below) — a worktree's
  `.git` is a gitdir-pointer file that would aim VM-side git at a
  nonexistent Mac path. Either way, do commits/pushes on the Mac; the
  Linux box is for `cargo test` / clippy / Firecracker validation.
- **Local tree / git worktrees**: by default the synced tree is the main
  checkout. To validate code that lives in a **git worktree** (e.g.
  `.Codex/worktrees/<name>` for an ADR branch) WITHOUT the old
  copy-into-main-checkout dance, point the sync at it:

  ```
  ENGRAM_DEV_LOCAL_DIR=/abs/path/to/.Codex/worktrees/<name> /dev-vm sync-start
  ```

  If `ENGRAM_DEV_LOCAL_DIR` is unset, `config.sh` auto-detects the git
  worktree/checkout containing `$PWD` (falling back to the main checkout).
  Only one dev-vm session runs at a time, so `sync-start` simply
  **repoints** (terminates + recreates) the Mutagen session when
  `LOCAL_DIR` changes — switch worktrees by re-running `sync-start` with a
  new `ENGRAM_DEV_LOCAL_DIR`. The remote dir (`~/engrams`) is unchanged, so
  `run.sh` / `run-boot-test.sh` work as-is. (VM-side git is unavailable in
  worktree mode — `cargo`/`just` don't need it.)

## Notes / gotchas

- After `bootstrap-remote`, the kvm/docker group additions need a fresh
  SSH session to take effect. The `ssh.sh` script always opens a new
  one, so this is handled automatically.
- `run.sh` passes commands through `nix develop`, so the toolchain is
  whatever `flake.nix` pins — even if the VM has stale system installs.
- If sync stalls, check `sync-status` first; `sync-flush` forces it to
  catch up. Conflicts surface there with file paths.
- The VM's external IP changes on every stop/start. Mutagen connects
  by SSH alias (via `gcloud compute config-ssh`), so this is invisible
  unless you've hardcoded the IP elsewhere.
- **Background processes on the VM die when SSH disconnects**, even
  with `nohup ... &` and `disown`. The user systemd cgroup reaps the
  whole session on logout. Use `tmux new-session -d -s <name> '<cmd>'`
  for anything that has to outlive the SSH connection (e.g. running
  the coordinator while you exec sessions against it from a second
  shell). `pkill -9 -f <pattern>` works fine for one-shot kills, but
  for long-running daemons reach for tmux.
- Firecracker leaves orphaned `firecracker --api-sock ...` processes
  if the coord exits ungracefully. `pgrep -af firecracker` to find
  them, `kill -9 <pid>` to clean up. The dev-firecracker run path
  cleans these on its own restart, but a Ctrl-C followed by a fresh
  start can leave a stale one behind.
