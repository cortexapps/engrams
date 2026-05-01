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

# Run the coordinator wired to the subprocess sandbox backend.
# Postgres must already be up (`just db-up`). Auto-spawns the noop
# harness on every new session so `engram session log <id>` shows
# tool-call traffic out of the box. Override which harness via
# `ENGRAM_DEV_AUTO_AGENT=noop|claude`; unset it to disable auto-spawn.
dev: db-up dev-build-harness
    DATABASE_URL=postgres://engram:engram@localhost:5435/engram \
    ENGRAM_BIND_ADDR=127.0.0.1:8090 \
    ENGRAM_MODE=all \
    ENGRAM_SANDBOX_BACKEND=process \
    ENGRAM_SANDBOX_WORK_DIR=./var/sandboxes \
    ENGRAM_LOCAL_PATH=./var/engram \
    ENGRAM_DEFAULT_IMAGE=warm-bootstrap \
    ENGRAM_WARM_POOL_SIZE=${ENGRAM_WARM_POOL_SIZE:-1} \
    ENGRAM_DEV_AUTO_AGENT=${ENGRAM_DEV_AUTO_AGENT:-noop} \
    RUST_LOG=info,engram=debug \
    cargo run -p engram-coordinator

# Build the noop-harness binary so `--dev-auto-noop` finds it next
# to the coordinator's exe. Cheap no-op once it's built.
dev-build-harness:
    cargo build -p engram-harness-noop --bin engram-harness-noop

# Run the coordinator wired to the Firecracker backend. Requires
# Linux + KVM. Will not work on macOS — use `just dev` instead.
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
dev-firecracker: db-up
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
    ENGRAM_DEV_AUTO_AGENT=${ENGRAM_DEV_AUTO_AGENT:-} \
    ENGRAM_DEV_NOOP_HARNESS_PATH=${ENGRAM_DEV_NOOP_HARNESS_PATH:-/sbin/engram-harness-noop} \
    ENGRAM_DEV_CLAUDE_HARNESS_PATH=${ENGRAM_DEV_CLAUDE_HARNESS_PATH:-/sbin/engram-harness-claude} \
    RUST_LOG=info,engram=debug \
    cargo run -p engram-coordinator

# Bake a tiny Firecracker image (debian-slim + engram-agentd +
# engram-bootstrap + engram-harness-noop) for the `local://demo`
# repo and register it under `./var/engram/images/`. The repo name
# passed to `engram image build` must match what the session-create
# body will carry (`local://demo`), because the image registry
# resolves images by literal repo string.
#
# `--inject-bootstrap` + `--inject-harness` light up the Phase 4
# harness path on FC: bootstrap listens on vsock 1025 in the guest,
# the host's `start_agent` pushes a BootstrapLaunch frame, bootstrap
# exec's the harness, harness dials back via vsock to the harness
# hub. Without these flags the image is exec-only (no live tool
# call timeline / idle eviction).
fc-bake-demo:
    cargo build -p engram-agentd     --target x86_64-unknown-linux-musl --release
    cargo build -p engram-bootstrap  --target x86_64-unknown-linux-musl --release
    cargo build -p engram-harness-noop --target x86_64-unknown-linux-musl --release
    mkdir -p ./var/fc-bake
    printf 'FROM debian:bookworm-slim\n' > ./var/fc-bake/Dockerfile
    printf 'name = "local-demo"\n'         > ./var/fc-bake/engram.toml
    cargo run -p engram-cli -- image build \
        --repo local://demo \
        --tag warm-1 \
        --source ./var/fc-bake \
        --format ext4 \
        --images-dir ./var/engram/images \
        --inject-agent     target/x86_64-unknown-linux-musl/release/engram-agentd \
        --inject-bootstrap target/x86_64-unknown-linux-musl/release/engram-bootstrap \
        --inject-harness   engram-harness-noop=target/x86_64-unknown-linux-musl/release/engram-harness-noop

# Bake a Firecracker image containing the real Claude Code CLI plus
# engram-agentd / engram-bootstrap / engram-harness-claude.
#
# The base is node:20-slim because Anthropic ships claude as an npm
# package (`@anthropic-ai/claude-code`). git + ca-certificates are
# pulled in so Claude can read/write the workspace and reach
# api.anthropic.com.
#
# Image manifest declares ANTHROPIC_API_KEY as a required secret
# under SecretMode::Literal (the dev default — values land directly
# in env). The operator must `export ANTHROPIC_API_KEY=sk-...`
# before `just dev-firecracker` for session-create to succeed; the
# coordinator's EnvSecretStore reads it from the host process env.
#
# After this recipe, kick off:
#   ENGRAM_DEFAULT_IMAGE=warm-1 \
#   ENGRAM_DEV_AUTO_AGENT=claude \
#   just dev-firecracker
#
# then `engram session create --repo local://claude-demo --prompt "..."`.
fc-bake-claude:
    cargo build -p engram-agentd         --target x86_64-unknown-linux-musl --release
    cargo build -p engram-bootstrap      --target x86_64-unknown-linux-musl --release
    cargo build -p engram-harness-claude --target x86_64-unknown-linux-musl --release
    mkdir -p ./var/fc-bake-claude
    cp deploy/fc-bake-claude/Dockerfile  ./var/fc-bake-claude/Dockerfile
    cp deploy/fc-bake-claude/engram.toml ./var/fc-bake-claude/engram.toml
    cargo run -p engram-cli -- image build \
        --repo local://claude-demo \
        --tag warm-1 \
        --source ./var/fc-bake-claude \
        --format ext4 \
        --images-dir ./var/engram/images \
        --inject-agent     target/x86_64-unknown-linux-musl/release/engram-agentd \
        --inject-bootstrap target/x86_64-unknown-linux-musl/release/engram-bootstrap \
        --inject-harness   engram-harness-claude=target/x86_64-unknown-linux-musl/release/engram-harness-claude

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
#   2. `just vz-bake-claude` (or vz-bake-demo) has run, so a
#      warm-1 image exists for `local://claude-demo`.
#
# The recipe codesigns the coord binary first; without the
# entitlement VZ refuses to instantiate any VM.
dev-vz: db-up vz-codesign
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
    ENGRAM_DEV_AUTO_AGENT=${ENGRAM_DEV_AUTO_AGENT:-} \
    ENGRAM_DEV_NOOP_HARNESS_PATH=${ENGRAM_DEV_NOOP_HARNESS_PATH:-/sbin/engram-harness-noop} \
    ENGRAM_DEV_CLAUDE_HARNESS_PATH=${ENGRAM_DEV_CLAUDE_HARNESS_PATH:-/sbin/engram-harness-claude} \
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
vz-bake-demo:
    rustup target add aarch64-unknown-linux-musl >/dev/null 2>&1 || true
    cargo build -p engram-agentd       --target aarch64-unknown-linux-musl --release
    cargo build -p engram-bootstrap    --target aarch64-unknown-linux-musl --release
    cargo build -p engram-harness-noop --target aarch64-unknown-linux-musl --release
    mkdir -p ./var/vz-bake
    printf 'FROM --platform=linux/arm64 debian:bookworm-slim\n' > ./var/vz-bake/Dockerfile
    printf 'name = "vz-demo"\n'                                 > ./var/vz-bake/engram.toml
    PATH="/opt/homebrew/opt/e2fsprogs/sbin:$PATH" \
    cargo run -p engram-cli -- image build \
        --repo local://demo \
        --tag warm-1 \
        --source ./var/vz-bake \
        --format ext4 \
        --images-dir ./var/engram/images \
        --transport console \
        --inject-agent     target/aarch64-unknown-linux-musl/release/engram-agentd \
        --inject-bootstrap target/aarch64-unknown-linux-musl/release/engram-bootstrap \
        --inject-harness   engram-harness-noop=target/aarch64-unknown-linux-musl/release/engram-harness-noop

# Bake the Claude image for VZ — arm64 sibling of fc-bake-claude.
# Reuses the same Dockerfile / engram.toml under deploy/fc-bake-claude/
# since the VZ build is architecture-neutral apart from the npm
# install step (which docker buildx handles via --platform).
vz-bake-claude:
    rustup target add aarch64-unknown-linux-musl >/dev/null 2>&1 || true
    cargo build -p engram-agentd         --target aarch64-unknown-linux-musl --release
    cargo build -p engram-bootstrap      --target aarch64-unknown-linux-musl --release
    cargo build -p engram-harness-claude --target aarch64-unknown-linux-musl --release
    mkdir -p ./var/vz-bake-claude
    cp deploy/fc-bake-claude/Dockerfile  ./var/vz-bake-claude/Dockerfile
    cp deploy/fc-bake-claude/engram.toml ./var/vz-bake-claude/engram.toml
    PATH="/opt/homebrew/opt/e2fsprogs/sbin:$PATH" \
    cargo run -p engram-cli -- image build \
        --repo local://claude-demo \
        --tag warm-1 \
        --source ./var/vz-bake-claude \
        --format ext4 \
        --images-dir ./var/engram/images \
        --transport console \
        --inject-agent     target/aarch64-unknown-linux-musl/release/engram-agentd \
        --inject-bootstrap target/aarch64-unknown-linux-musl/release/engram-bootstrap \
        --inject-harness   engram-harness-claude=target/aarch64-unknown-linux-musl/release/engram-harness-claude

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

# POST a session and print the session_id.
smoke-create:
    curl -s -X POST http://localhost:8090/sessions \
        -H 'content-type: application/json' \
        -d '{"repo":"local://hello-world","branch":"main"}' | jq

# Drop everything in ./var/* (sandbox cwds + snapshots).
clean-var:
    rm -rf ./var
