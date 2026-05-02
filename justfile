# Engram dev recipes. Install `just`: https://github.com/casey/just
#
# Single-line summary of the dev story:
#   `just dev` brings up Postgres + the coordinator wired to the
#   subprocess sandbox backend. Works on macOS Apple Silicon, no nested
#   virt, no Firecracker. Real isolation comes from
#   `engram-sandbox-firecracker` on Linux production hosts — see DESIGN.md.

set shell := ["bash", "-cu"]
set dotenv-load := true

# `just` with no args prints the recipe list.
default:
    @just --list

# ------------------------------------------------------------------
# Build / quality gates
# ------------------------------------------------------------------

# Format, clippy, build, and test gates — run before pushing.
#
# Uses `cargo nextest` for ~3-5× speedup over `cargo test --workspace`.
# Inside `nix develop` it's already on $PATH; outside Nix install with
# `cargo install cargo-nextest --locked` (one-time, ~30s). The doctest
# pass stays on `cargo test` because nextest doesn't run doctests yet.
check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo nextest run --workspace
    cargo test --workspace --doc

# Auto-format the workspace.
fmt:
    cargo fmt --all

# Run all tests via nextest (faster). Pass extra args after `--`.
test *ARGS:
    cargo nextest run --workspace {{ARGS}}

# Run a single crate's tests with output. Stays on `cargo test` so
# `--nocapture` works the way you expect.
test-pkg pkg *ARGS:
    cargo test -p {{pkg}} -- --nocapture {{ARGS}}

# ------------------------------------------------------------------
# Local stack — Postgres in docker, coordinator in cargo.
# ------------------------------------------------------------------

# Start Postgres only. Coordinator runs via `just dev`.
db-up:
    docker compose -f deploy/docker-compose.yml up -d postgres

# Stop the local Postgres.
db-down:
    docker compose -f deploy/docker-compose.yml down

# psql into the dev Postgres.
psql:
    docker compose -f deploy/docker-compose.yml exec postgres \
        psql -U engram -d engram

# Apply migrations (sqlx-cli not required — coordinator runs them on
# startup, but this is useful for ad-hoc psql work).
migrate:
    docker compose -f deploy/docker-compose.yml exec -T postgres \
        psql -U engram -d engram < deploy/migrations/0001_initial.sql

# Drop the dev DB volume (destructive). Use when migrations diverge.
db-reset:
    docker compose -f deploy/docker-compose.yml down -v
    just db-up

# ------------------------------------------------------------------
# Coordinator — local dev (subprocess sandbox backend)
# ------------------------------------------------------------------

# The Process backend was demoted to a test-only fixture, so
# `just dev` (Process-based, single-binary) is gone. Use:
#   - `just dev-vz`           on macOS Apple Silicon
#   - `just dev-firecracker`  on Linux + KVM
# Both run the coordinator with a real VMM; pass
# `--harness <name>` to `engram session create` for per-session
# agent selection.

# Run the coordinator wired to the Firecracker backend. Requires
# Linux + KVM. Will not work on macOS — use `just dev-vz` instead.
#
# Set ENGRAM_KERNEL_IMAGE_PATH to a vmlinux Firecracker can boot.
# The fc-test artifact path under ~/.cache/engram-fc-test/ works
# (run `bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh`
# once to populate it).
#
# The sandbox image you create sessions against must be baked with
# `engram image build --inject-agent <agentd-musl-binary> --format ext4`
# so the in-VM agent is present. Without it `session exec` will
# hang waiting for vsock to come up.
dev-firecracker: db-up install-harnesses
    : "${ENGRAM_KERNEL_IMAGE_PATH:?set ENGRAM_KERNEL_IMAGE_PATH to a vmlinux on this host}"
    DATABASE_URL=postgres://engram:engram@localhost:5435/engram \
    ENGRAM_BIND_ADDR=127.0.0.1:8090 \
    ENGRAM_MODE=all \
    ENGRAM_SANDBOX_BACKEND=firecracker \
    ENGRAM_SANDBOX_WORK_DIR=./var/sandboxes \
    ENGRAM_LOCAL_PATH=./var/engram \
    ENGRAM_KERNEL_IMAGE_PATH=$ENGRAM_KERNEL_IMAGE_PATH \
    ENGRAM_DEFAULT_IMAGE=${ENGRAM_DEFAULT_IMAGE:-warm-bootstrap} \
    ENGRAM_WARM_POOL_SIZE=${ENGRAM_WARM_POOL_SIZE:-1} \
    RUST_LOG=info,engram=debug \
    cargo run -p engram-coordinator

# Build the harness packs and lay them out at
# `./var/engram/harnesses/<name>/` — the directory tree the coord
# scans on startup and mounts read-only into every sandbox at
# `/run/engram/harnesses` via virtio-fs. After this runs, `engram
# session create --harness <name>` works for {claude, noop}
# regardless of which image the session lands on. Idempotent.
#
# Pack layout: each `<name>/` carries the wrapper at `harness` plus
# any sidecars the wrapper needs at runtime. Today only `claude`
# has a sidecar — Anthropic's bundled-Bun `claude` binary,
# downloaded from the official releases endpoint.
#
# Cross-compiles wrappers for aarch64-unknown-linux-musl on macOS
# (VZ guests) and x86_64-unknown-linux-musl on Linux (FC guests) —
# both produce Linux binaries since the harness runs inside the
# guest VM, not on the host. Downloads the matching bundled
# `claude` from `downloads.claude.ai/claude-code-releases`.
install-harnesses:
    @set -e; \
    mkdir -p ./var/engram/harnesses ; \
    if [ "$(uname -s)" = "Darwin" ]; then \
        TARGET=aarch64-unknown-linux-musl ; \
        CLAUDE_PLAT=linux-arm64 ; \
    else \
        TARGET=x86_64-unknown-linux-musl ; \
        CLAUDE_PLAT=linux-x64 ; \
    fi ; \
    rustup target add $TARGET >/dev/null 2>&1 || true ; \
    cargo build -p engram-harness-noop   --target $TARGET --release ; \
    cargo build -p engram-harness-claude --target $TARGET --release ; \
    \
    mkdir -p ./var/engram/harnesses/noop ; \
    cp -p "target/$TARGET/release/engram-harness-noop" ./var/engram/harnesses/noop/harness ; \
    \
    mkdir -p ./var/engram/harnesses/claude ; \
    cp -p "target/$TARGET/release/engram-harness-claude" ./var/engram/harnesses/claude/harness ; \
    CLAUDE_VERSION=$(curl -fsSL https://downloads.claude.ai/claude-code-releases/latest) ; \
    CLAUDE_DEST=./var/engram/harnesses/claude/claude ; \
    if [ ! -f "$CLAUDE_DEST" ] || [ "$(cat ./var/engram/harnesses/claude/.version 2>/dev/null)" != "$CLAUDE_VERSION-$CLAUDE_PLAT" ]; then \
        echo "downloading claude $CLAUDE_VERSION ($CLAUDE_PLAT) ..." ; \
        curl -fsSL --retry 3 -o "$CLAUDE_DEST.tmp" \
            "https://downloads.claude.ai/claude-code-releases/$CLAUDE_VERSION/$CLAUDE_PLAT/claude" ; \
        chmod +x "$CLAUDE_DEST.tmp" ; \
        mv "$CLAUDE_DEST.tmp" "$CLAUDE_DEST" ; \
        printf "%s-%s\n" "$CLAUDE_VERSION" "$CLAUDE_PLAT" > ./var/engram/harnesses/claude/.version ; \
    else \
        echo "claude $CLAUDE_VERSION ($CLAUDE_PLAT) already installed" ; \
    fi ; \
    \
    echo "harnesses installed at ./var/engram/harnesses/" ; \
    ls -la ./var/engram/harnesses/*/

# Bake the canonical workspace image for Firecracker (Linux + KVM).
# debian:bookworm-slim + git + ttyd, no harness-specific runtime —
# any harness in `./var/engram/harnesses/` works against this image
# (they're mounted into every sandbox via virtio-fs). Image
# manifest at deploy/demo/engram.toml declares no harness secrets;
# image-level secrets are reserved for workspace-side things
# (NPM_TOKEN, GITHUB_TOKEN, etc.).
#
# After this recipe, kick off:
#   ENGRAM_DEFAULT_IMAGE=warm-1 just dev-firecracker
#
# Then create a session — pass `--harness claude` (or `--harness
# noop`) to attach an agent. Auth credentials for claude come from
# the env (CLAUDE_CODE_OAUTH_TOKEN or ANTHROPIC_API_KEY) or from
# the dashboard's session-create form.
fc-bake-demo:
    cargo build -p engram-agentd    --target x86_64-unknown-linux-musl --release
    cargo build -p engram-bootstrap --target x86_64-unknown-linux-musl --release
    mkdir -p ./var/fc-bake-demo
    cp deploy/demo/Dockerfile  ./var/fc-bake-demo/Dockerfile
    cp deploy/demo/engram.toml ./var/fc-bake-demo/engram.toml
    cargo run -p engram-cli -- image build \
        --repo local://demo \
        --tag warm-1 \
        --source ./var/fc-bake-demo \
        --format ext4 \
        --images-dir ./var/engram/images \
        --inject-agent     target/x86_64-unknown-linux-musl/release/engram-agentd \
        --inject-bootstrap target/x86_64-unknown-linux-musl/release/engram-bootstrap

# ------------------------------------------------------------------
# Apple Silicon — Virtualization.framework backend
#
# `just dev-vz` is the macOS counterpart to `just dev-firecracker`.
# Same wire surface (vsock UDS at <work_dir>/<sandbox>.vsock_*),
# same chat-shaped event stream, same idle-evict-and-resume cycle —
# different VMM. Boots arm64 Linux guests via VZ.
# ------------------------------------------------------------------

# Ad-hoc codesign the coordinator + the engram-sandbox-vz test
# binaries with the com.apple.security.virtualization entitlement.
# Without this, every VZ API call returns NSError "process doesn't
# have the com.apple.security.virtualization entitlement" — see the
# smoke test in crates/engram-sandbox-vz/src/vm.rs.
#
# Idempotent: re-running on an already-signed binary is a no-op
# beyond a few ms of cycle. `dev-vz` and `vz-test` depend on it.
#
# We sign with the ad-hoc identity (`-`), which is enough for
# locally-built dev binaries on Apple Silicon. CI does the same.
# Distribution to other machines would need a real signing
# identity + notarization; out of scope here.
vz-codesign:
    @if [ "$(uname -s)" != "Darwin" ]; then \
        echo "vz-codesign is macOS-only; skipping" >&2; exit 0; \
    fi
    cargo build -p engram-coordinator -p engram-sandbox-vz --tests
    bash crates/engram-sandbox-vz/scripts/codesign.sh debug

# Run the engram-sandbox-vz crate's unit tests, including the live
# VZ smoke test gated behind --ignored. Codesigns first so the
# entitlement check passes when the test reaches into VZ.
vz-test: vz-codesign
    cargo nextest run -p engram-sandbox-vz
    cargo nextest run -p engram-sandbox-vz -- --ignored

# Run the coordinator with the VZ backend.
#
# Prereqs (one-time):
#   1. ENGRAM_VZ_KERNEL_PATH points at an arm64 Linux vmlinux with
#      VIRTIO_BLK / VIRTIO_NET / VIRTIO_CONSOLE enabled. Default
#      cache path is ~/.cache/engram-vz-test/vmlinux-arm64; populate
#      it via `just vz-pull-kernel` (downloads the Kata Containers
#      static kernel — same one apple/container uses).
#   2. `just vz-bake-demo` has run, so a warm-1 image exists for
#      `local://demo` under `./var/engram/images/`.
#   3. `just install-harnesses` has run so harness binaries are in
#      `./var/engram/harnesses/` for virtio-fs mounting.
#
# The recipe codesigns the coord binary first; without the
# entitlement VZ refuses to instantiate any VM.
dev-vz: db-up vz-codesign install-harnesses
    @if [ "$(uname -s)" != "Darwin" ]; then \
        echo "dev-vz only runs on macOS"; exit 1; \
    fi
    DATABASE_URL=postgres://engram:engram@localhost:5435/engram \
    ENGRAM_BIND_ADDR=127.0.0.1:8090 \
    ENGRAM_MODE=all \
    ENGRAM_SANDBOX_BACKEND=vz \
    ENGRAM_SANDBOX_WORK_DIR=./var/sandboxes \
    ENGRAM_LOCAL_PATH=./var/engram \
    ENGRAM_VZ_KERNEL_PATH=${ENGRAM_VZ_KERNEL_PATH:-$HOME/.cache/engram-vz-test/vmlinux-arm64} \
    ENGRAM_DEFAULT_IMAGE=${ENGRAM_DEFAULT_IMAGE:-warm-1} \
    ENGRAM_WARM_POOL_SIZE=${ENGRAM_WARM_POOL_SIZE:-1} \
    RUST_LOG=info,engram=debug \
    target/debug/engram-coordinator

# Pull the Kata Containers static arm64 kernel. This is the same
# kernel `apple/container` (Apple's container CLI built on
# Virtualization.framework) uses by default — Linux 6.18.15 with a
# VZ-tuned kconfig that strips PCI/ACPI/USB/sound/graphics and ships
# VIRTIO_BLK/NET/CONSOLE built in. Cold boot is sub-second on
# Apple Silicon.
#
# Replaces the older Ubuntu cloud-image kernel — that one was
# ~5x slower to boot and has the wrong subsystem mix for VZ. The
# Kata tarball is ~290 MB; we extract just the kernel (~25 MB) and
# drop the rest. Cached at `~/.cache/engram-vz-test/vmlinux-arm64`.
#
# License: Linux is GPLv2 (redistributable). The Kata release tarball
# is published as a release artifact on
# github.com/kata-containers/kata-containers.
vz-pull-kernel:
    bash crates/engram-sandbox-vz/scripts/pull-kernel.sh

# Deprecated alias for vz-pull-kernel. The Ubuntu generic kernel
# we used to pull here boots in ~3-5s and has a kconfig profile
# that doesn't match VZ's device set (it expects PCI/ACPI which
# aren't there). Kept for reference; just-pull-kernel handles
# everything now.
vz-pull-ubuntu-kernel: vz-pull-kernel

# Bake the noop demo image for the VZ backend. arm64 + virtio-block.
# Mirror of fc-bake-demo but cross-compiled for aarch64 and built
# under linux/arm64 buildx so the rootfs binaries match the kernel.
#
# One-time setup on Apple Silicon:
#   brew install musl-cross         # x86_64-linux-musl-gcc + aarch64-linux-musl-gcc
#   brew install e2fsprogs          # mke2fs (keg-only — see PATH munge below)
#
# `.cargo/config.toml` wires the cross-linker; the e2fsprogs PATH
# is added inline by these recipes so a bare `just vz-bake-demo`
# works without the operator munging their shell profile.
# Bake the canonical workspace image for VZ (Apple Silicon) — arm64
# sibling of `fc-bake-demo`. Same Dockerfile under deploy/demo/;
# only the cross-compile target + e2fsprogs PATH differ. Harness
# binaries are NOT in the image — drop them in
# `./var/engram/harnesses/` via `just install-harnesses` and pick
# at session-create time with `--harness <name>`.
vz-bake-demo:
    rustup target add aarch64-unknown-linux-musl >/dev/null 2>&1 || true
    cargo build -p engram-agentd    --target aarch64-unknown-linux-musl --release
    cargo build -p engram-bootstrap --target aarch64-unknown-linux-musl --release
    mkdir -p ./var/vz-bake-demo
    cp deploy/demo/Dockerfile  ./var/vz-bake-demo/Dockerfile
    cp deploy/demo/engram.toml ./var/vz-bake-demo/engram.toml
    # Force linux/arm64 base for the docker build on macOS so the
    # rootfs binaries match the kernel arch.
    sed -i.bak 's|^FROM debian:|FROM --platform=linux/arm64 debian:|' ./var/vz-bake-demo/Dockerfile
    rm -f ./var/vz-bake-demo/Dockerfile.bak
    PATH="/opt/homebrew/opt/e2fsprogs/sbin:$PATH" \
    cargo run -p engram-cli -- image build \
        --repo local://demo \
        --tag warm-1 \
        --source ./var/vz-bake-demo \
        --format ext4 \
        --images-dir ./var/engram/images \
        --transport console \
        --inject-agent     target/aarch64-unknown-linux-musl/release/engram-agentd \
        --inject-bootstrap target/aarch64-unknown-linux-musl/release/engram-bootstrap

# Hot-reload the coordinator on file changes. Requires `cargo watch`:
#   cargo install cargo-watch
watch:
    cargo watch -x 'run -p engram-coordinator -- --sandbox-backend=process'

# ------------------------------------------------------------------
# Smoke tests against a running coordinator (`just dev` in another term)
# ------------------------------------------------------------------

# Hit GET /healthz.
smoke-health:
    curl -s http://localhost:8090/healthz | jq

# POST a session and print the session_id. Requires the demo image
# from `just vz-bake-demo` (or `fc-bake-demo`).
smoke-create:
    curl -s -X POST http://localhost:8090/sessions \
        -H 'content-type: application/json' \
        -d '{ \
              "image": {"kind":"registry","repo":"local://demo","tag":"warm-1"}, \
              "workspace": {"kind":"empty"}, \
              "harness": {"kind":"none"} \
            }' | jq

# Smallest-possible dev session: empty workspace, no agent. Useful
# for confirming the orthogonal axes in isolation — no git remote
# to clone, no harness binary to attach, just a VM with a shell.
# Pair with the dashboard's SHELL tab or `engram session exec`.
dev-shell:
    curl -s -X POST http://localhost:8090/sessions \
        -H 'content-type: application/json' \
        -d '{ \
              "image": {"kind":"registry","repo":"local://demo","tag":"warm-1"}, \
              "workspace": {"kind":"empty"}, \
              "harness": {"kind":"none"} \
            }' | jq -r '.session_id'

# Drop everything in ./var/* (sandbox cwds + snapshots).
clean-var:
    rm -rf ./var

# ------------------------------------------------------------------
# Web dashboard — read-only live view of the running coordinator.
# Run alongside `just dev` (or `just dev-vz`) in another terminal;
# Vite proxies /sessions and /api to 127.0.0.1:8090.
# ------------------------------------------------------------------

# Install web deps (idempotent — pnpm skips if lockfile is fresh).
web-install:
    cd web && pnpm install

# Run the Vite dev server. Defaults to :5173; override with `PORT`
# (e.g. `PORT=5174 just web`) when 5173 is taken. No production build
# is wired up yet — the dashboard is a dev-time tool.
web port='5173': web-install
    cd web && pnpm dev --port {{port}} --strictPort
