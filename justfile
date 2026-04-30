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
