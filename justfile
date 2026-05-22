# Engram dev recipes. Install `just`: https://github.com/casey/just
#
# Single-line summary of the dev story:
#   `just dev` runs `tilt up`, which brings up the docker-compose
#   infra (postgres + OCI registry), one-shots (KEK bootstrap,
#   harness packs, VZ codesign on Mac), the coordinator (cargo run),
#   and the web SPA. Visit http://localhost:10350 for the Tilt
#   dashboard. Backend (VZ vs Firecracker) is auto-selected by arch.

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
    cargo nextest run --workspace

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
# `just dev` — full-stack orchestrated by Tilt.
#
# Brings up postgres + OCI registry (docker-compose), runs the KEK
# + harness one-shots, starts the coordinator (cargo run, manual
# restart), and starts the web SPA (vite HMR). All processes
# stream into Tilt's UI at http://localhost:10350.
#
# Backend (VZ vs Firecracker) is auto-selected by arch. Mac Apple
# Silicon → VZ; Linux + KVM x86_64 → Firecracker. Other hosts are
# rejected with a clear error.
#
# Prereqs (one-time):
#   • `brew install tilt-dev/tap/tilt` (or your platform's install)
#   • a kernel artifact for the chosen backend — VZ:
#     `just vz-pull-kernel`; FC: run the fc-test fetch script.
#     The Tiltfile reads .env / process env for ENGRAM_VZ_KERNEL_PATH
#     or ENGRAM_KERNEL_IMAGE_PATH and falls back to the standard
#     cache locations.
# ------------------------------------------------------------------
dev:
    tilt up

# Same as `just dev` but in prod-shape split mode: coordinator runs
# `mode=coordinator` (no in-process sandbox backend), a separate
# `engram-host-agent` process registers + heartbeats over HTTP and
# answers gRPC HostService calls — exactly what production runs.
# Blob backend is forced to `gcs` (against fake-gcs-server) so
# the OCI → BlobStorage materialization on enable-image is
# exercised end-to-end.
#
# Use this when validating cross-host behavior locally: warm-pool
# refill, register-time template delivery, the canonical-path
# symlink contract, blob-backend coupling bugs.
dev-split:
    ENGRAM_DEV_SPLIT=1 tilt up

# Bring everything down: kill the coordinator + web processes,
# stop the docker-compose services, leave volumes intact.
dev-down:
    tilt down

# ------------------------------------------------------------------
# `just integration-up` — prod-shape stack without Tilt.
#
# Designed for the dev VM, which has docker + just but not Tilt.
# Brings up:
#   • postgres (compose)
#   • fake-gcs-server (compose)
#   • local OCI registry (compose)
#   • KEK + GCS bucket seed (one-shot)
#   • coordinator in `mode=coordinator` (background, logs in
#     ./var/integration/coord.log)
#   • engram-host-agent dialing the coordinator (background,
#     logs in ./var/integration/host-agent.log)
#
# Once everything is up, run `just integration-test` to exercise
# the bake → enable → warm-pool → session-create loop end-to-end
# against the local stack. `just integration-down` stops the
# processes and the compose services.
#
# Linux-only (Firecracker requires KVM); the rig assumes a kernel
# artifact in $HOME/.cache/engram-fc-test/. Run the fetch script
# (crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)
# if you don't have one.
# ------------------------------------------------------------------
integration-up:
    bash deploy/dev/integration-up.sh

# Stop the integration stack. Kills the background coord +
# host-agent processes, stops docker compose services. Volumes
# stay (db, gcs bucket) so re-running picks up state.
integration-down:
    bash deploy/dev/integration-down.sh

# Hard-reset the integration stack: kills processes, drops compose
# volumes (postgres + fake-gcs + registry), wipes ./var dirs. Use
# when prior runs have left stale `templates` rows whose snapshot
# blobs are gone — the warm-pool refill loop spams logs and can
# starve real session creates. After this, run `just integration-up`.
integration-reset:
    bash deploy/dev/integration-reset.sh

# Smoke-test the full bake → enable → warm-pool → session flow
# against the local integration stack. Times each step and
# asserts the warm path actually triggers. Run after
# `just integration-up`.
integration-test:
    bash deploy/dev/integration-test.sh

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
    cargo build -p engram-agentd    --target $TARGET --release; \
    cargo build -p engram-bootstrap --target $TARGET --release; \
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
        --inject-agent     "target/$TARGET/release/engram-agentd" \
        --inject-bootstrap "target/$TARGET/release/engram-bootstrap" \
        --push localhost:5001/{{repo}}; \
    echo ""; \
    echo "✓ pushed localhost:5001/{{repo}}:$TAG"; \
    echo "  use it: engram session create --image localhost:5001/{{repo}}:$TAG ..."

# ------------------------------------------------------------------
# Lower-level recipes (composed by `just dev` via Tilt; useful
# directly when you want to skip Tilt or debug a specific layer).
# ------------------------------------------------------------------

# Note: `just dev-vz` / `just dev-firecracker` predate the Tilt
# orchestration and run the coordinator standalone (without web,
# without one-shot supervision). Kept for backwards compat — they
# still work, but `just dev` is the recommended path.

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
dev-firecracker: db-up registry-up bootstrap
    : "${ENGRAM_KERNEL_IMAGE_PATH:?set ENGRAM_KERNEL_IMAGE_PATH to a vmlinux on this host}"
    DATABASE_URL=postgres://engram:engram@localhost:5435/engram \
    ENGRAM_BIND_ADDR=127.0.0.1:8090 \
    ENGRAM_MODE=all \
    ENGRAM_SANDBOX_BACKEND=firecracker \
    ENGRAM_SANDBOX_WORK_DIR=./var/sandboxes \
    ENGRAM_LOCAL_PATH=./var/engram \
    ENGRAM_KERNEL_IMAGE_PATH=$ENGRAM_KERNEL_IMAGE_PATH \
    ENGRAM_DEFAULT_IMAGE=${ENGRAM_DEFAULT_IMAGE:-warm-bootstrap} \
    RUST_LOG=info,engram=debug \
    cargo run -p engram-coordinator

# Build a harness pack and push it to the local OCI registry as
# `localhost:5001/cortex/harness-<name>:<tag>`. **Registry-only** —
# this recipe deliberately does NOT touch Postgres. After it
# finishes, register the pack with the coordinator via the dashboard
# (Settings → Harnesses) or `engram harness add --name <name>
# --registry-uri ...`. Same separation as `just bake` for images.
#
# Cross-compiles the wrapper for aarch64-unknown-linux-musl on macOS
# (VZ guests) and x86_64-unknown-linux-musl on Linux (FC guests) —
# both produce Linux binaries since the harness runs inside the
# guest VM, not on the host. For `claude`, also downloads the
# matching bundled CLI from `downloads.claude.ai/claude-code-releases`
# and bundles it into the OCI artifact alongside the wrapper.
#
# Usage:
#   just bake-harness noop  v1
#   just bake-harness claude v1
bake-harness NAME TAG="v1":
    @set -e; \
    if [ "$(uname -s)" = "Darwin" ]; then \
        TARGET=aarch64-unknown-linux-musl ; \
        CLAUDE_PLAT=linux-arm64 ; \
    else \
        TARGET=x86_64-unknown-linux-musl ; \
        CLAUDE_PLAT=linux-x64 ; \
    fi ; \
    rustup target add $TARGET >/dev/null 2>&1 || true ; \
    STAGE=$(mktemp -d) ; \
    cargo build -p engram-harness-{{NAME}} --target $TARGET --release ; \
    cp -p "target/$TARGET/release/engram-harness-{{NAME}}" "$STAGE/harness" ; \
    if [ "{{NAME}}" = "claude" ]; then \
        CLAUDE_VERSION=$(curl -fsSL https://downloads.claude.ai/claude-code-releases/latest) ; \
        CACHE_DIR="$HOME/.cache/engram-claude-cli/$CLAUDE_VERSION/$CLAUDE_PLAT" ; \
        CACHED="$CACHE_DIR/claude" ; \
        if [ -x "$CACHED" ]; then \
            echo "using cached claude $CLAUDE_VERSION ($CLAUDE_PLAT) from $CACHED" ; \
        else \
            echo "downloading claude $CLAUDE_VERSION ($CLAUDE_PLAT) -> $CACHED ..." ; \
            mkdir -p "$CACHE_DIR" ; \
            curl -fsSL --retry 3 -o "$CACHED.tmp" \
                "https://downloads.claude.ai/claude-code-releases/$CLAUDE_VERSION/$CLAUDE_PLAT/claude" ; \
            chmod +x "$CACHED.tmp" ; \
            mv "$CACHED.tmp" "$CACHED" ; \
        fi ; \
        cp -p "$CACHED" "$STAGE/claude" ; \
    fi ; \
    URI=localhost:5001/cortex/harness-{{NAME}}:{{TAG}} ; \
    cargo run -p engram-cli -- harness push --from "$STAGE" --to "$URI" ; \
    rm -rf "$STAGE"

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
#   3. Any harness pack you want to use has been baked + registered
#      via `just bake-harness <name>` (pushes to the local registry
#      AND registers a `harness_packs` row).
#
# The recipe codesigns the coord binary first; without the
# entitlement VZ refuses to instantiate any VM.
dev-vz: db-up registry-up bootstrap vz-codesign
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
# the registry via `just bake-harness <name>` (registers automatically)
# and pick at session-create time with `--harness <name>`.
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
