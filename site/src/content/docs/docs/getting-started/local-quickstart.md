---
title: Run engrams locally
description: Start the whole stack on one machine and enable the demo image.
sidebar:
  order: 2
---

This page runs every part of engrams on one machine so you can see a session boot, stream,
and snapshot. It is not a production shape. On an Apple Silicon Mac the sessions run under
Apple's Virtualization framework; on a Linux machine with `/dev/kvm` they run under
Firecracker; anywhere else they run as plain subprocesses with no isolation at all.

## Prerequisites

If you have Nix, `nix develop` in the repository gives you the pinned toolchain and you can
skip to the next section. If you use direnv, `direnv allow` once and the shell activates on
its own.

Without Nix, on macOS:

```sh
# Rust, per rust-toolchain.toml (https://rustup.rs), then the guest target:
rustup target add aarch64-unknown-linux-musl
brew install just tilt-dev/tap/tilt jq squashfs protobuf pkg-config openssl node pnpm bun
brew install FiloSottile/musl-cross/musl-cross --with-aarch64
```

You also need Docker Desktop, OrbStack, or Colima for the registry and Postgres containers,
and the Xcode Command Line Tools for `codesign`.

On Linux you need the same Rust toolchain, Docker, `just`, `tilt`, `jq`, and `bun`. If you
want Firecracker rather than subprocesses, check that you can use KVM:

```sh
[ -r /dev/kvm ] && [ -w /dev/kvm ] && echo OK || echo FAIL
```

`FAIL` usually means your user is not in the `kvm` group. engrams falls back to subprocesses
in that case and says so when it starts.

## Start the stack

From the repository root:

```sh
git clone https://github.com/cortexapps/engrams.git && cd engrams
just bootstrap     # writes a local master key to .env (once)
just pull-kernel   # fetches the guest kernel for this machine's backend (once)
just dev           # starts everything under Tilt
```

`just dev` picks the backend for this machine on its own: Firecracker when `/dev/kvm` is
usable, Apple Virtualization on an Apple Silicon Mac, subprocesses otherwise. It brings up
Postgres, a local container registry, and the blob-store emulator in Docker, then the
coordinator, the host agent, the orchestrator, and the dashboard as local processes.

Tilt's status page is at http://localhost:10350 and shows every process and its log. The
dashboard is at http://localhost:5173. Open it and create an account; registration is open in
the dev stack, and there is nothing to configure.

## Enable the demo image

In a second terminal:

```sh
just bake-demo-enable
```

This builds the image in `deploy/demo/`, pushes it to the local registry as
`localhost:5001/demo:warm-1`, enables it, and waits while engrams captures its base snapshot.
The capture boots a VM, runs the image's warm-up command, and freezes the result; it takes a
minute or two the first time. The command exits non-zero if the capture fails, with the
failing stage and the last of its output.

## Give the agent a key

The Claude Code harness needs a credential. In the dashboard, open Settings and either
connect your Claude Code token, which authenticates sessions you start yourself, or add an org
secret named `ANTHROPIC_API_KEY`, which authenticates sessions started from the CLI, Slack,
and other automations. For Codex the org secret is `CODEX_API_KEY`, and the personal path is
a ChatGPT connection under Settings → Credentials.

Now you have a running stack with one enabled image. [Your first session](../first-session/)
starts an agent in it.

## Stop and reset

`just dev-down` stops every process and container and leaves the data in place, so the next
`just dev` starts where you left off. `just clean-var` removes the local sandbox working
directories and snapshots, and `just db-reset` destroys and recreates the dev database.
