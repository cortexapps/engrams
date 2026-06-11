# Engram dev recipes. Install `just`: https://github.com/casey/just
#
# Single-line summary of the dev story:
#   `just dev` runs `tilt up` on every host: docker-compose infra
#   (postgres + OCI registry + fake-gcs + jaeger), the KEK one-shot,
#   the coordinator, a host-agent (on real-virt hosts), and the web
#   SPA. Visit http://localhost:10350 for the Tilt dashboard. The
#   backend (Firecracker / VZ / process) is detected per host by
#   deploy/dev/detect-backend.sh — no flag, no per-arch recipe (ADR
#   0024). `just bake-demo` bakes the Claude image; `just pull-kernel`
#   fetches the right kernel.

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
# `cargo install cargo-nextest --locked` (one-time, ~30s).
check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo hakari verify
    cargo nextest run --workspace

# Auto-format the workspace.
fmt:
    cargo fmt --all

# Regenerate the TS bindings for the engram.app contract (ADR 0039 §7)
# from crates/engram-protocol/proto via the repo-root buf.gen.yaml.
# Outputs: web/src/gen + orchestrator/src/gen — commit them; CI's `buf`
# job re-generates and fails on drift. Guarded: until the app contract
# lands (Tasks 4-5) there is nothing to generate. Install buf:
# `brew install bufbuild/buf/buf`.
gen-proto:
    @if [ -d crates/engram-protocol/proto/engram/app ]; then \
        buf generate; \
    else \
        echo "engram/app contract not present yet (ADR 0039 Tasks 4-5) — nothing to generate"; \
    fi

# Regenerate `workspace-hack/Cargo.toml` from the current dep
# graph. Run after adding or removing a workspace dep so
# `cargo hakari verify` (in `just check` and CI) stays green. The
# workspace-hack crate unifies feature sets across members so
# switching between `cargo test -p X` and `cargo build -p Y`
# doesn't re-cook shared deps under different features.
hakari:
    cargo hakari generate
    cargo hakari manage-deps --yes

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
    docker compose -f deploy/docker-compose.dev.yml up -d postgres

# Stop the local Postgres.
db-down:
    docker compose -f deploy/docker-compose.dev.yml down

# Bring up the local OCI registry on http://localhost:5001.
# Anonymous pull/push, plaintext HTTP. Engram's OCI client only
# allows HTTP for loopback hosts so this is safe-by-construction.
registry-up:
    docker compose -f deploy/docker-compose.dev.yml up -d registry

registry-down:
    docker compose -f deploy/docker-compose.dev.yml stop registry

# Bring up Jaeger for ADR 0019 cold-boot tracing. UI at
# http://localhost:16686; OTLP/gRPC collector on :4317. Export
# OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 to make the
# binaries emit spans (`just dev` sets it for you). Works the same on
# macOS and the Linux dev-vm.
trace-up:
    docker compose -f deploy/docker-compose.dev.yml up -d jaeger
    @echo "Jaeger UI → http://localhost:16686  (OTLP/gRPC on :4317)"

trace-down:
    docker compose -f deploy/docker-compose.dev.yml stop jaeger

# Generate a 32-byte master key (KEK) for envelope-encrypted
# registry credentials and write it into `.env` for direnv/`just`
# to pick up. Idempotent: if `.env` already has ENGRAM_KEK_MASTER_KEY
# set, leaves it alone. Run once per dev box.
bootstrap:
    @set -e; \
    touch .env ; \
    if grep -q '^ENGRAM_KEK_MASTER_KEY=' .env 2>/dev/null; then \
        echo "ENGRAM_KEK_MASTER_KEY already present in .env — leaving as-is" ; \
    else \
        if command -v openssl >/dev/null 2>&1; then \
            KEY=$(openssl rand -base64 32) ; \
        else \
            KEY=$(head -c 32 /dev/urandom | base64) ; \
        fi ; \
        printf 'ENGRAM_KEK_MASTER_KEY=%s\n' "$KEY" >> .env ; \
        echo "wrote ENGRAM_KEK_MASTER_KEY to .env (32 random bytes, base64)" ; \
    fi

# psql into the dev Postgres.
psql:
    docker compose -f deploy/docker-compose.dev.yml exec postgres \
        psql -U engram -d engram

# Apply migrations (sqlx-cli not required — coordinator runs them on
# startup, but this is useful for ad-hoc psql work).
migrate:
    docker compose -f deploy/docker-compose.dev.yml exec -T postgres \
        psql -U engram -d engram < deploy/migrations/0001_initial.sql

# Drop the dev DB volume (destructive). Use when migrations diverge.
db-reset:
    docker compose -f deploy/docker-compose.dev.yml down -v
    just db-up

# ------------------------------------------------------------------
# `just dev` — the whole stack, one command, every host (ADR 0024).
#
# Runs `tilt up`: postgres + OCI registry + fake-gcs + jaeger
# (docker-compose), the KEK one-shot, the coordinator, a host-agent
# (when the host has real virt), and the web SPA. All processes stream
# into Tilt's UI at http://localhost:10350.
#
# No backend flag, no per-arch recipe. `deploy/dev/detect-backend.sh`
# picks the backend from the host — /dev/kvm → Firecracker (split),
# macOS+arm64 → VZ (split), else → ProcessBackend (in-process mode=all).
# The binaries receive a concrete ENGRAM_SANDBOX_BACKEND; the switch
# never reaches this file.
#
# Prereqs (one-time): `just pull-kernel` (fetches the right kernel for
# this host; a no-op on the process backend). Tilt comes from the nix
# devShell (`nix develop`) or `brew install tilt-dev/tap/tilt`.
# ------------------------------------------------------------------
dev:
    tilt up

# Bring everything down: stop the Tilt-managed processes + the
# docker-compose services, leave volumes intact.
dev-down:
    tilt down

# Build the Claude harness from source, publish it to the local OCI
# registry, and bake deploy/demo-claude/ against it — pushing the image
# to localhost:5001 (the registry `just dev` runs). Arch + transport are
# detected; no per-backend recipe. Requires `just bootstrap` (KEK) and a
# running local registry (it's up under `just dev`, or `just registry-up`).
bake-demo:
    bash deploy/dev/bake-demo.sh

# Fetch the kernel artifact this host's backend needs (VZ → Kata arm64
# kernel; Firecracker → FC test kernel+rootfs; process → nothing).
pull-kernel:
    bash deploy/dev/pull-kernel.sh

# Stage the ADR 0027 RO bundles (skills, playwright) as UNPACKED trees
# under var/bundles/ for the dev ProcessBackend, which symlinks them in
# instead of mounting a squashfs. `skills` is a plain copy; `playwright`
# needs Docker (glibc browser build) and is best-effort — skip it and only
# skills get wired (no browser tooling in dev). Re-run after editing a skill.
bundles:
    deploy/bundles/skills/build.sh --stage var/bundles/skills
    deploy/bundles/playwright/build.sh --stage var/bundles/playwright \
        || echo "playwright bundle skipped (needs Docker) — dev sessions get skills only"

# ADR 0035: build + stage the squashfs bundles CONTENT-ADDRESSED
# (<name>-<sha256>.squashfs + current.json stamp) under var/shared/, the
# dev mirror of the FC-host image's /var/lib/engram/shared. Run the
# host-agent with ENGRAM_BUNDLE_DIR=$PWD/var/shared so FC dev sessions
# resolve/capture against it. Linux-only (mksquashfs; FC is Linux-only
# anyway). Re-run after editing a skill — the stamp repoints and new
# sessions pick the fresh generation up via the §3 swap.
bundles-squashfs:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p var/shared
    stamp="{"
    sep=""
    for name in skills playwright; do
        tmp="var/shared/.$name.build.squashfs"
        if ! "deploy/bundles/$name/build.sh" "$tmp"; then
            echo "$name bundle build failed; skipping (sessions degrade gracefully)" >&2
            rm -f "$tmp"
            continue
        fi
        sha="$(sha256sum "$tmp" | cut -d' ' -f1)"
        mv "$tmp" "var/shared/$name-$sha.squashfs"
        stamp="$stamp$sep\"$name\": \"$sha\""
        sep=", "
    done
    echo "$stamp}" > var/shared/current.json
    cat var/shared/current.json

# ------------------------------------------------------------------
# Smoke / e2e helpers — run against a stack brought up by `just dev`
# (ADR 0024 retired the standalone `integration-up.sh`; the prod-shape
# stack is now just `just dev`, on the dev-vm typically inside tmux).
# ------------------------------------------------------------------

# Smoke-test the full bake → enable → host-prefetch → session flow
# against the running stack. Times each step. Run after `just dev`.
integration-test:
    bash deploy/dev/integration-test.sh

# ADR 0018 M4 e2e evacuation test. Run after
# `ENGRAM_INTEG_TWO_HOSTS=1 just dev` (which adds a second host-agent so
# coord sees a 2-host cluster). Creates a session, writes a deterministic
# disk canary, evacuates via the admin endpoint, asserts the session
# reaches Active on a different host with the disk contents preserved.
# The gold-standard "M4 actually works" check.
integration-evac-test:
    bash deploy/dev/integration-evac-test.sh

# Persistent dev session — bakes + enables + creates a session and
# leaves it running so you can poke at it with curl, wscat, or the
# web UI. Idempotent: reuses an already-enabled image and an
# already-live session for this commit. Companion to
# integration-test (which always cleans up).
#
# Usage:
#   just integration-session
#   HARNESS=claude PROMPT='hi' just integration-session
integration-session:
    bash deploy/dev/integration-session.sh

# ADR 0039 characterization net. Requires `just dev` and the no-harness
# demo image (`just integration-session` once). Run before merging any
# task of the ADR 0039 plan. (The snap spec self-skips here: it only
# screenshots when SNAP_PATHS is set.)
e2e:
    cd web && pnpm e2e

# Visual validation protocol: full-page screenshots of the given paths.
# Usage: just snap "/,/sessions/<id>,/sessions/<id>?tab=raw"
# Extras: `?tab=<id>` clicks that session-detail tab first; `#new` opens
# the new-session dialog. Output: web/e2e/__shots__/<slug>.png — compare
# (by reading the images) against web/e2e/__shots__/baseline/.
snap paths="/":
    cd web && SNAP_PATHS="{{paths}}" pnpm exec playwright test snap --reporter=list

# Bake an image from a directory containing Dockerfile + engram.toml,
# then push it to the local OCI registry. Auto-selects cross-compile
# target + `--transport` flag based on host arch. Tag defaults to
# `warm-<rfc3339>`; override with `TAG=...`.
#
# Usage:
#   just bake cortex/api ./examples/api
#   just bake cortex/api .
#   TAG=warm-2 just bake cortex/api ./examples/api
#
# After this, the new image appears in the dashboard's "create
# session" dropdown immediately (the coordinator polls
# `image_versions` on every `/api/images` GET) and is pullable via
# `engram session create --image localhost:5001/<repo>:<tag>`.
bake repo dir='.':
    @set -e; \
    : "${ENGRAM_KEK_MASTER_KEY:?run \`just bootstrap\` first to generate a KEK}"; \
    if [ "$(uname -s -m)" = "Darwin arm64" ]; then \
        TARGET=aarch64-unknown-linux-musl; PLATFORM=linux/arm64; TRANSPORT=console; \
    elif [ "$(uname -s -m)" = "Linux x86_64" ]; then \
        TARGET=x86_64-unknown-linux-musl; PLATFORM=linux/amd64; TRANSPORT=vsock; \
    else \
        echo "unsupported host: $(uname -s -m)" >&2; exit 1; \
    fi; \
    rustup target add $TARGET >/dev/null 2>&1 || true; \
    cargo build -p engram-agentd --target $TARGET --release; \
    TAG="${TAG:-warm-$(date -u +%Y%m%dT%H%M%SZ)}"; \
    STAGING="./var/bake/{{repo}}"; \
    rm -rf "$STAGING"; mkdir -p "$STAGING"; \
    cp "{{dir}}/Dockerfile"  "$STAGING/Dockerfile"; \
    cp "{{dir}}/engram.toml" "$STAGING/engram.toml"; \
    if [ "$PLATFORM" = "linux/arm64" ]; then \
        sed -i.bak 's|^FROM \([^ ]*\)$|FROM --platform=linux/arm64 \1|' "$STAGING/Dockerfile"; \
        rm -f "$STAGING/Dockerfile.bak"; \
    fi; \
    PATH="/opt/homebrew/opt/e2fsprogs/sbin:$PATH" \
    cargo run -p engram-cli -- image build \
        --repo {{repo}} \
        --tag $TAG \
        --source $STAGING \
        --format ext4 \
        --images-dir ./var/bake/_staging \
        --transport $TRANSPORT \
        --inject-agent "target/$TARGET/release/engram-agentd" \
        --push localhost:5001/{{repo}}; \
    echo ""; \
    echo "✓ pushed localhost:5001/{{repo}}:$TAG"; \
    echo "  use it: engram session create --image localhost:5001/{{repo}}:$TAG ..."

# ------------------------------------------------------------------
# Apple Silicon — Virtualization.framework codesigning + VZ tests.
#
# `just dev` (Tilt) is the way to run the stack; these recipes are the
# VZ-specific build helpers it and the test suite rely on. The
# standalone `dev-vz` / `dev-firecracker` runners and the per-arch
# `fc-bake-demo` / `vz-bake-demo` / `bake-harness` recipes were retired
# in ADR 0024 — `just dev` + `just bake-demo` + `just pull-kernel`
# cover every host now.
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

# POST a session and print the session_id. Requires a demo image in
# the registry (`just bake-demo`).
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
# `just dev` already runs the web SPA; this recipe is for running it
# standalone against a coordinator on 127.0.0.1:8090. Vite proxies
# /sessions and /api there.
# ------------------------------------------------------------------

# Install web deps (idempotent — pnpm skips if lockfile is fresh).
web-install:
    cd web && pnpm install

# Run the Vite dev server. Defaults to :5173; override with `PORT`
# (e.g. `PORT=5174 just web`) when 5173 is taken. No production build
# is wired up yet — the dashboard is a dev-time tool.
web port='5173': web-install
    cd web && pnpm dev --port {{port}} --strictPort
