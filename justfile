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
check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

# Auto-format the workspace.
fmt:
    cargo fmt --all

# Run all tests.
test *ARGS:
    cargo test --workspace {{ARGS}}

# Run a single crate's tests with output.
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
# Postgres must already be up (`just db-up`).
dev: db-up
    DATABASE_URL=postgres://engram:engram@localhost:5435/engram \
    ENGRAM_BIND_ADDR=127.0.0.1:8090 \
    ENGRAM_SANDBOX_BACKEND=process \
    ENGRAM_SANDBOX_WORK_DIR=./var/sandboxes \
    ENGRAM_STORAGE_BACKEND=local \
    ENGRAM_STORAGE_LOCAL_PATH=./var/snapshots \
    ENGRAM_DEFAULT_IMAGE=warm-bootstrap \
    RUST_LOG=info,engram=debug \
    cargo run -p engram-coordinator

# Run the coordinator wired to the Firecracker backend. Requires
# Linux + KVM. Will not work on macOS — use `just dev` instead.
dev-firecracker: db-up
    DATABASE_URL=postgres://engram:engram@localhost:5435/engram \
    ENGRAM_BIND_ADDR=127.0.0.1:8090 \
    ENGRAM_SANDBOX_BACKEND=firecracker \
    ENGRAM_SANDBOX_WORK_DIR=./var/sandboxes \
    ENGRAM_STORAGE_BACKEND=local \
    ENGRAM_STORAGE_LOCAL_PATH=./var/snapshots \
    RUST_LOG=info,engram=debug \
    cargo run -p engram-coordinator

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
        -d '{"repo":"hello-world","branch":"main"}' | jq

# Drop everything in ./var/* (sandbox cwds + snapshots).
clean-var:
    rm -rf ./var
