---
title: Install engrams with a coding agent
description: Exact steps, checks, and stop conditions for a coding agent that installs engrams on one machine.
sidebar:
  order: 2
---

This page is for a coding agent, such as Claude Code, Codex, or Cursor, that a person asked to
install engrams. Every step has a command and a check. Do the steps in order, and do not go to
the next step until the check passes. A person can read it too; it is the same install as
[Run engrams locally](../local-quickstart/), written for a machine to follow.

The result is the whole engrams stack on this machine: Postgres, a container registry, a blob
store emulator, the control plane, a host agent, and the dashboard, with one enabled image and
one test session that ran a command. It is for evaluation, not production. For a production
install in a cloud account, stop and give the person
[Deploy to your cloud](../../guides/deploy-overview/): it creates cloud resources and needs
their decisions.

## Rules for the agent

- Do not create, guess, or copy model API keys. The last step asks the person for one.
- Do not run `sudo` without asking the person. Group changes such as adding the user to `kvm`
  need their approval.
- `just dev` runs until it is stopped. Start it as a background process and keep its log.
- The first start compiles Rust and can take 10 to 20 minutes. A long wait is not a failure.
  Read the log before you decide that something is stuck.

## 1. Check the machine

engrams runs on macOS on Apple Silicon and on Linux (x86_64 or arm64). Windows is not
supported; use WSL 2 with a Linux distribution.

```sh
uname -sm
docker info --format '{{.ServerVersion}}'
```

Check: `uname` prints `Darwin arm64`, or `Linux` with `x86_64` or `aarch64`. `docker info`
prints a version. If `docker info` fails, Docker is not running: ask the person to start Docker
Desktop, OrbStack, or Colima.

On Linux, check whether sessions can use Firecracker:

```sh
[ -r /dev/kvm ] && [ -w /dev/kvm ] && echo KVM-OK || echo KVM-NO
```

`KVM-NO` is not a stop. The stack still runs, but sessions run as plain subprocesses with no
isolation. Tell the person, and say that adding their user to the `kvm` group fixes it.

## 2. Get the source and the toolchain

```sh
git clone https://github.com/cortexapps/engrams.git
cd engrams
command -v nix && echo NIX-OK || echo NIX-NO
```

With `NIX-OK`, run every later command inside the pinned toolchain. Put `nix develop -c` in
front of each one, for example `nix develop -c just bootstrap`. The first `nix develop`
downloads the toolchain and takes several minutes.

With `NIX-NO`, install the toolchain. On macOS:

```sh
# Rust from https://rustup.rs. The repository's rust-toolchain.toml picks the version.
rustup target add aarch64-unknown-linux-musl
brew install just tilt-dev/tap/tilt jq squashfs protobuf pkg-config openssl node pnpm bun
brew install FiloSottile/musl-cross/musl-cross --with-aarch64
xcode-select -p
```

On Linux you need Rust from rustup, Docker, `just`, `tilt`, `jq`, and `bun`.

Check:

```sh
just --version && tilt version && bun --version && cargo --version
```

Each command prints a version.

## 3. Create the local key and fetch the guest kernel

```sh
just bootstrap
just pull-kernel
```

Check: both exit with code 0, and `.env` contains `ENGRAM_KEK_MASTER_KEY`. These steps run
once per machine. Running them again is safe.

## 4. Start the stack

Start it in the background and write its log to a file:

```sh
mkdir -p var
nohup just dev > var/just-dev.log 2>&1 &
```

Wait until the orchestrator answers and the local admin key exists:

```sh
until curl -fsS http://127.0.0.1:8787/healthz > /dev/null && [ -s var/dev-api-key ]; do
  sleep 15
done
curl -fsS http://127.0.0.1:8787/healthz
```

Check: the last command prints a JSON body with `"ok":true`. Put a limit of 30 minutes on the
wait. If the limit passes, read `var/just-dev.log`, and run `tilt get uiresource` to see which
resource failed. `tilt logs <resource>` prints the log of one resource.

The first lines of the log say which backend the stack picked: Firecracker when `/dev/kvm` is
usable, Apple Virtualization on an Apple Silicon Mac, subprocesses otherwise.

## 5. Enable the demo image

```sh
just bake-demo-enable
```

This builds the image in `deploy/demo/`, pushes it to the local registry as
`localhost:5001/demo:warm-1`, enables it, and waits while engrams captures its base snapshot.
It also builds the `engrams` CLI to `target/debug/engrams`.

Check: the command exits with code 0. A non-zero exit prints the failing stage and the end of
its output.

## 6. Prove that a session runs

Start a session with no agent, run a command in it, and delete it:

```sh
export ENGRAMS_URL=http://127.0.0.1:8787
export ENGRAMS_API_KEY="$(cat var/dev-api-key)"
SID=$(target/debug/engrams session create --image localhost:5001/demo:warm-1 --dev-vm)
target/debug/engrams session exec "$SID" 'echo engrams-ok; uname -a'
target/debug/engrams session delete "$SID"
```

Check: `session exec` prints `engrams-ok` and exits with code 0. The install is complete. This
check needs no model key, because `--dev-vm` starts the VM with no agent in it.

## 7. Hand off to the person

Tell the person these things:

- The dashboard is at http://localhost:5173. Registration is open in the local stack, so they
  can create an account there.
- To let an agent work, they add a model credential in the dashboard under Settings: their
  Claude Code token, or an org secret named `ANTHROPIC_API_KEY` (or `CODEX_API_KEY` for Codex).
- [Your first session](../first-session/) shows how to start an agent, watch it, and see it
  snapshot and resume.
- `just dev-down` stops the stack and keeps its data. The next `just dev` starts where it
  stopped.

## If a step fails

| Symptom | What to do |
|---|---|
| `docker info` fails | Docker is not running. Ask the person to start it. |
| `just: command not found` with Nix | Put `nix develop -c` in front of the command. |
| The health wait passes 30 minutes | Read `var/just-dev.log` and `tilt get uiresource`; report the failed resource and its log to the person. |
| Port 8787, 5173, or 10350 is in use | Another stack, or an old one, is running. Run `just dev-down`, or ask the person what owns the port. |
| `bake-demo-enable` fails at capture | Read the printed stage. On Linux without KVM, check that the log says subprocesses were picked. |
| Sessions run without isolation | `/dev/kvm` is not usable by this user. Tell the person; do not fix group membership without approval. |
