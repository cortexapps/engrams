---
title: Developing engrams
description: The repository's layout, the toolchain, the dev stack, and the gates a change has to pass.
sidebar:
  order: 1
---

engrams is one repository with a Rust workspace at the root and three TypeScript projects
beside it.

| Path | What it is | Tooling |
|---|---|---|
| `crates/` | The Rust workspace: the coordinator, the host agent, the in-guest daemon, the sandbox backends, the storage and cloud backends, the harness adapters | cargo |
| `orchestrator/` | The product API and integrations, on Bun and Hono | bun |
| `web/` | The dashboard, a React app | pnpm |
| `cli/` | The `engrams` CLI | bun |
| `site/` | This site, Astro and Starlight | pnpm |
| `deploy/` | Helm charts, Terraform, the demo image, the harness descriptors | |
| `docs/` | Design notes and runbooks for people working on engrams | |

`AGENTS.md` at the root is the working guide for the repository: the commands, the test
lanes, and the conventions. Read it before a first change; this page is the short version.

## Toolchain

`nix develop` gives you everything pinned: Rust, `just`, Tilt, nextest, the protobuf
compiler, Node and pnpm, Bun, and the musl cross-compilers for the guest binaries. The same
hashes work on macOS and Linux. Without Nix, the
[local quickstart](../../getting-started/local-quickstart/) lists what to install by hand.

## The dev stack

`just dev` runs the whole thing under Tilt with the backend picked for your machine. Git
worktrees share one dev identity: the master key and the baked bundles live with the primary
checkout, so a worktree never re-bakes and never breaks a sealed key. `just bootstrap` and
`just pull-kernel` are one-time.

The inner loop for Rust is `cargo check -p <crate>` and `cargo nextest run -p <crate>`;
workspace-wide cargo mid-edit queues behind rust-analyzer. `just check` is the full Rust gate,
`cargo fmt --check`, `cargo clippy -D warnings`, the dependency-hash check, and every test,
and it runs once before a pull request that touches Rust. Do not run it for a change that
only touches docs, the web app, or the orchestrator.

## The gates a change has to pass

Use the smallest gate that covers what you changed.

- **Docs only:** `git diff --check`.
- **Site only:** from `site/`, `pnpm build`, which runs the leak lint, `astro check`, and the
  build.
- **Web only:** from `web/`, `pnpm format:check`, `pnpm lint`, the focused `pnpm test`
  targets, and `pnpm build`.
- **Orchestrator only:** from `orchestrator/`, `bun run typecheck` and the focused `bun test`
  targets.
- **Rust or shared backend:** per-crate checks and tests while editing, then `just check`
  once before the pull request. Linux-only code is invisible to clippy on a Mac, so cross-check
  it with `cargo clippy --target aarch64-unknown-linux-musl -p <crate> --all-targets`.

CI is path-gated: only the lanes a change can affect run, and one aggregate check, `CI Gate`,
is what a pull request needs green. Tests that need a Linux host with KVM run on KVM runners
and must be listed in the workflow to run at all; a new test that CI never invokes is a bug.

## Conventions

One logical change per commit. Commit messages and pull request descriptions carry the
reasoning; a design record is for a new crate, a new trait boundary, a schema or wire change,
or a decision later work has to build on, and nothing smaller. Prefer a clean break over a
compatibility shim: image rebuilds and schema changes are acceptable, deprecated stubs are
not. Never silence the type checker or a lint to make an error go away; a mismatch is the
compiler telling you the shapes do not line up.

In the coordinator and Postgres code, time and randomness are injected: read the clock and
the entropy source from the service context rather than calling the system, so the same code
runs under the deterministic simulator. Every background driver keeps its logic in a
`run_once` step that tests drive directly, with the timer loop as a thin wrapper around it.

When a test fails, read the production code before you touch the assertion; a failing test is
usually a real bug. A flake gets fixed the day it is noticed.

## Filing issues

Issues and pull requests are on [GitHub](https://github.com/cortexapps/engrams). For a bug,
include the backend (Firecracker, Apple Virtualization, or process), the commit, and the
coordinator and host-agent log lines around the failure; for a session that misbehaved, the
session id and the event log from `engrams session logs` are the most useful thing you can
attach.
