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

# Regenerate `workspace-hack/Cargo.toml` from the current dep
# graph. Run after adding or removing a workspace dep so
# `cargo hakari verify` (in `just check` and CI) stays green. The
# workspace-hack crate unifies feature sets across members so
# switching between `cargo test -p X` and `cargo build -p Y`
# doesn't re-cook shared deps under different features.
hakari:
    cargo hakari generate
    cargo hakari manage-deps --yes

# Regenerate the TS bindings for the app contract (ADR 0051 §7), driven
# by buf.gen.yaml. Run after editing crates/engram-protocol/proto/engram/app/**.
# The output dirs (web/src/gen, orchestrator/src/gen) land with their
# consumers (ADR 0051 Tasks 4-5); until then there is nothing to generate.
gen-proto:
    @if [ -d crates/engram-protocol/proto/engram/app ]; then \
        buf generate; \
    else \
        echo "engram/app contract not present yet (ADR 0051 Tasks 4-5) — nothing to generate"; \
    fi

# Run all tests via nextest (faster). Pass extra args after `--`.
test *ARGS:
    cargo nextest run --workspace {{ARGS}}

# Run a single crate's tests with output. Stays on `cargo test` so
# `--nocapture` works the way you expect.
test-pkg pkg *ARGS:
    cargo test -p {{pkg}} -- --nocapture {{ARGS}}

# Run a crate's Linux-gated tests (the `#[cfg(target_os = "linux")]` ones —
# e.g. engram-harness-claude's hook/socket/resume suite). On Linux this is
# just `cargo test`. On macOS those tests don't compile or run natively, so
# this builds + runs them inside a `rust` Docker container — on Apple
# Silicon that's the native arch (no emulation). The host cargo registry is
# mounted so crates aren't re-downloaded, and a container-local
# CARGO_TARGET_DIR keeps the macOS `target/` (a different target triple)
# untouched. Uses `cargo test` (not nextest) so the container needs no extra
# tooling. `rust:bookworm` tracks the latest stable, matching our pinned
# `channel = "stable"`. Example: `just test-linux engram-harness-claude`.
test-linux pkg='engram-harness-claude' *ARGS:
    #!/usr/bin/env bash
    set -euo pipefail
    case "$(uname -s)" in
        Linux)
            cargo test -p {{pkg}} {{ARGS}} ;;
        Darwin)
            if ! command -v docker >/dev/null 2>&1; then
                echo "test-linux on macOS needs Docker — the cfg(target_os=\"linux\") tests can't run on darwin." >&2
                exit 1
            fi
            echo ">> macOS host: running {{pkg}} Linux tests in a rust:bookworm container (native arch)…" >&2
            # Target dir is a named volume (lives in Docker's VM on a native
            # fs — fast incremental rebuilds, persists across runs — unlike a
            # virtiofs bind mount). Registry is bind-mounted from the host so
            # crates aren't re-downloaded.
            docker run --rm \
                -v "$PWD":/work \
                -v "$HOME/.cargo/registry":/usr/local/cargo/registry \
                -v engram-linux-target:/lxtarget \
                -w /work \
                -e CARGO_TARGET_DIR=/lxtarget \
                rust:bookworm \
                bash -c "cargo test -p {{pkg}} {{ARGS}}" ;;
        *)
            echo "unsupported host: $(uname -s)" >&2; exit 1 ;;
    esac

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
    [ -f .env ] || touch .env ; \
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

# Create + migrate the orchestrator DB (ADR 0051). The Tiltfile's
# `orchestrator-migrate` resource does this on `tilt up`; this recipe is
# the same steps for ad-hoc use (e.g. after a `just db-reset`).
migrate-orchestrator:
    cd orchestrator && bun install --silent && \
        (PGPASSWORD=engram createdb -h localhost -p 5435 -U engram engram_orchestrator 2>/dev/null || true) && \
        ORCHESTRATOR_DATABASE_URL=postgres://engram:engram@localhost:5435/engram_orchestrator \
        bunx drizzle-kit migrate

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

# ADR 0068: like `just dev`, but the host-agent (+ its Firecracker stack)
# runs INSIDE a dedicated Colima VM instead of as a Mac-local process —
# the only way to exercise the FC-only surfaces (NBD, UFFD, netns egress,
# squashfs patch-drives) on Apple Silicon. Coordinator/orchestrator/web/
# compose stay on the Mac exactly as in plain `just dev`; only the
# Tiltfile's host-agent resource moves. The env var is the real switch —
# the Tiltfile reads ENGRAM_FC_COLIMA_PROFILE directly — this recipe is
# sugar so you don't have to remember its name. First-time setup:
# `just fc-colima-provision [profile]`.
dev-fc profile='fc-dev':
    ENGRAM_FC_COLIMA_PROFILE={{profile}} tilt up

# ADR 0068: create/update the named Colima VM (aarch64 Ubuntu, nested
# virt, /dev/kvm, Firecracker + the aarch64 guest kernel, NBD/UFFD host
# prep, the loopback-forwarding socat units) — everything `dev-fc` needs
# before its first run. Idempotent; safe to re-run after a Colima
# upgrade or a provisioning-script change.
fc-colima-provision profile='fc-dev':
    bash deploy/dev/fc-colima-provision.sh {{profile}}

# Reclaim all dev sandbox disk — the dev analog of the production lifecycle
# GC. Two stages:
#
#   1. Reap every session the coordinator tracks via the production
#      DeleteSession path (terminate -> destroy bound sandbox). There is no
#      fleet-wide reap RPC — the prod idle detector evicts one session at a
#      time (ADR 0034) — so this loops `session delete` over `session list`.
#      Idempotent: an already-terminal session deletes as a no-op.
#   2. Sweep orphaned local sandbox rootfs files. The host-agent teardown
#      reconciler (ADR 0050 E) only reaps sandboxes it still TRACKS; rootfs
#      files leaked by a killed / branch-switched host-agent are invisible to
#      it and pile up (hundreds of MiB each). This removes any
#      `<sandbox>.rootfs.ext4` (+ its vsock sockets) NOT owned by a session
#      that is still live — the live set is re-read AFTER stage 1, so a
#      session created concurrently is never swept.
#
# Checkpoints (snapshots) are reclaimed by the coordinator's snapshot/chunk
# GC once their owning sessions are gone; the warm-pool base snapshot an
# enabled image clones from is preserved (re-captured on image re-enable).
#
# Use it to reclaim disk or get a clean slate before a re-bake. Talks to the
# coordinator app-gRPC via ENGRAM_APP_GRPC_ADDR / ENGRAM_APP_GRPC_TOKEN (dev
# defaults below); the stack must be up.
reap-sessions:
    #!/usr/bin/env bash
    set -euo pipefail
    export ENGRAM_APP_GRPC_TOKEN="${ENGRAM_APP_GRPC_TOKEN:-${ENGRAM_APP_GRPC_TOKENS:-dev-app-grpc-token}}"
    sandboxes_dir="${ENGRAM_HOST_SANDBOX_DIR:-var/host-sandboxes}"
    # Disk usage of the sandbox dir in KiB (0 if it doesn't exist yet). `du -k`
    # is portable across macOS/Linux and reports actual allocated blocks, so
    # the before/after diff is the real disk reclaimed (sparse-aware). Never
    # fails the recipe under `set -e` — a missing dir just reports 0.
    dir_kb() {
        local kb=0
        [ -d "$1" ] && kb="$(du -sk "$1" 2>/dev/null | awk '{print $1}')" || true
        echo "${kb:-0}"
    }
    # KiB -> human-readable, matching `du -h` style.
    human_kb() {
        awk -v kb="${1:-0}" 'BEGIN{
            if (kb >= 1048576) printf "%.2f GB", kb/1048576;
            else if (kb >= 1024) printf "%.1f MB", kb/1024;
            else printf "%d KB", kb;
        }'
    }
    echo "==> starting reaper"
    echo "==> building engram-cli"
    cargo build --quiet -p engram-cli
    cli="$(cargo metadata --format-version=1 --no-deps \
        | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')/debug/engram-cli"
    before_kb="$(dir_kb "$sandboxes_dir")"; before_kb="${before_kb:-0}"
    # 1. Reap every live session via the production DeleteSession path.
    echo "==> loading sessions from the coordinator"
    ids="$("$cli" --json session list \
        | python3 -c 'import sys,json;print("\n".join(s["id"] for s in json.load(sys.stdin)["sessions"]))')"
    total=0; for id in $ids; do total=$((total+1)); done
    echo "found $total live session(s)"
    count=0
    for id in $ids; do
        printf '==> reaping %s ... ' "$id"
        if "$cli" session delete "$id" >/dev/null 2>&1; then echo deleted; count=$((count+1)); else echo "(skipped)"; fi
    done
    echo "reaped $count live session(s)."
    # 2. Sweep orphaned rootfs files no still-live session owns.
    echo "==> sweeping orphaned sandbox rootfs files"
    live=" $("$cli" --json session list \
        | python3 -c 'import sys,json;print(" ".join(s["sandbox_id"] for s in json.load(sys.stdin)["sessions"] if s.get("sandbox_id")))') "
    shopt -s nullglob
    swept=0
    for f in "$sandboxes_dir"/*.rootfs.ext4; do
        sb="$(basename "$f")"; sb="${sb%.rootfs.ext4}"
        case "$live" in *" $sb "*) continue ;; esac
        rm -f "$f" "$sandboxes_dir/$sb".vsock_*
        swept=$((swept+1))
    done
    echo "swept $swept orphaned sandbox rootfs file(s)."
    after_kb="$(dir_kb "$sandboxes_dir")"; after_kb="${after_kb:-0}"
    reclaimed_kb=$(( before_kb > after_kb ? before_kb - after_kb : 0 ))
    if [ "$reclaimed_kb" -gt 0 ]; then
        echo "reclaimed $(human_kb "$reclaimed_kb") of dev sandbox disk."
    else
        echo "no dev sandbox disk reclaimed (already clean)."
    fi
    echo "reap-sessions: done. Checkpoints GC in the background once their sessions are gone."

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

# Stage the ADR 0027 RO bundles (skills, integrations-cli, browser) as
# UNPACKED trees under var/bundles/ for the dev ProcessBackend, which symlinks
# them in instead of mounting a squashfs. `skills` is a plain copy;
# `integrations-cli`/`browser` need Docker (glibc builds) and are best-effort —
# skip them and only skills get wired (no browser tooling in dev). Re-run after
# editing a skill.
bundles:
    deploy/bundles/skills/build.sh --stage var/bundles/skills
    deploy/bundles/integrations-cli/build.sh --stage var/bundles/integrations-cli \
        || echo "integrations-cli bundle skipped (needs Docker) — dev sessions get no integration CLIs"
    deploy/bundles/browser/build.sh --stage var/bundles/browser \
        || echo "browser bundle skipped (needs Docker) — dev sessions get no browser"

# ADR 0035/0055: build + stage the squashfs bundles CONTENT-ADDRESSED
# (<sha256>.squashfs + current.json stamp) under var/shared/, the
# dev mirror of the FC-host image's /var/lib/engram/shared. Run the
# host-agent with ENGRAM_BUNDLE_DIR=$PWD/var/shared so FC dev sessions
# resolve/capture against it. Needs mksquashfs: on Linux that's
# `apt install squashfs-tools`; on macOS (ADR 0068's fc-colima dev mode —
# FC itself still only ever boots inside the Colima VM) run this from
# `nix develop`, which provides mksquashfs on darwin too. Re-run after
# editing a skill — the stamp repoints and new sessions pick the fresh
# generation up via the §3 swap.
bundles-squashfs:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p var/shared
    stamp="{"
    sep=""
    # ADR 0055: `sentinel` rides every reserved dyn-* slot; skills/
    # integrations-cli/browser are catalog skills swapped in per session. Files
    # are content-keyed (<sha>.squashfs); the stamp maps logical name -> sha.
    for name in sentinel skills integrations-cli browser; do
        tmp="var/shared/.$name.build.squashfs"
        if ! "deploy/bundles/$name/build.sh" "$tmp"; then
            echo "$name bundle build failed; skipping (sessions degrade gracefully)" >&2
            rm -f "$tmp"
            continue
        fi
        sha="$(sha256sum "$tmp" | cut -d' ' -f1)"
        mv "$tmp" "var/shared/$sha.squashfs"
        stamp="$stamp$sep\"$name\": \"$sha\""
        sep=", "
    done
    # ADR 0062: the built-in `claude` harness rides the stamp like a skill (key
    # `harness-claude`, mounted on dyn_0). Its tree — the engram-harness-claude
    # entry binary + the pinned `claude` CLI + the committed harness.toml — is
    # the ONE bundle not assembled by a per-bundle build.sh: CI hands it in
    # pre-built via ENGRAM_HARNESS_CLAUDE_TREE (bake-harness-claude-artifact). A
    # local dev stack has no such artifact, so when unset we build the tree HERE
    # (mirroring bake-demo's cross-compile + the bundles-vz path), then pack it
    # via harness-claude/build.sh. Without this the fleet stamp never carries
    # `harness-claude` and `POST /sessions` 400s with "built-in harness `claude`
    # squashfs (`harness-claude`) is not staged on any host yet". Requires the
    # nix cross toolchain on PATH (this recipe runs under `nix develop`).
    harness_tree="${ENGRAM_HARNESS_CLAUDE_TREE:-}"
    if [ -z "$harness_tree" ]; then
        # PINNED — keep in lockstep with ci.yml's bake-harness-claude-artifact
        # and bundles-vz: 2.1.185 is the newest CLI that still offers
        # AskUserQuestion headlessly (cortexapps/engrams#431); bump deliberately
        # and re-verify AUQ.
        CLAUDE_VERSION=2.1.185
        case "$(uname -m)" in
            arm64 | aarch64) htarget=aarch64-unknown-linux-musl; carch=linux-arm64 ;;
            x86_64 | amd64)  htarget=x86_64-unknown-linux-musl;   carch=linux-x64  ;;
            *) echo "harness-claude: unsupported arch $(uname -m); skipping" >&2; htarget="" ;;
        esac
        if [ -n "$htarget" ]; then
            # Best-effort: a cross-build/download failure warns and skips so the
            # stack still comes up (without the built-in claude).
            tree="$PWD/var/shared/.harness-claude.stage"
            cache="var/shared/.cache/claude-$CLAUDE_VERSION-$carch"
            ok=1
            cargo build --release --target "$htarget" -p engram-harness-claude || ok=0
            if [ "$ok" = 1 ] && [ ! -x "$cache" ]; then
                mkdir -p "$(dirname "$cache")"
                curl -fsSL --retry 3 \
                    "https://downloads.claude.ai/claude-code-releases/$CLAUDE_VERSION/$carch/claude" \
                    -o "$cache" && chmod +x "$cache" || ok=0
            fi
            if [ "$ok" = 1 ]; then
                rm -rf "$tree"; mkdir -p "$tree"
                cp -p "target/$htarget/release/engram-harness-claude" "$tree/harness"
                cp -p "$cache" "$tree/claude"
                cp -p deploy/harness-claude/harness.toml "$tree/harness.toml"
                harness_tree="$tree"
            else
                echo "harness-claude local build failed; skipping (dev stack boots without the built-in claude)" >&2
            fi
        fi
    fi
    if [ -n "$harness_tree" ]; then
        [ -x "$harness_tree/harness" ] || {
            echo "harness tree $harness_tree is missing an executable 'harness' entry binary" >&2
            exit 1
        }
        tmp="var/shared/.harness-claude.build.squashfs"
        deploy/bundles/harness-claude/build.sh "$harness_tree" "$tmp"
        sha="$(sha256sum "$tmp" | cut -d' ' -f1)"
        mv "$tmp" "var/shared/$sha.squashfs"
        stamp="$stamp$sep\"harness-claude\": \"$sha\""
        sep=", "
        # Drop the locally-built stage tree (keep the download cache); an
        # externally-provided ENGRAM_HARNESS_CLAUDE_TREE is left untouched.
        [ "$harness_tree" = "$PWD/var/shared/.harness-claude.stage" ] && rm -rf "$harness_tree"
    fi
    echo "$stamp}" > var/shared/current.json
    cat var/shared/current.json

# ADR 0061: build + stage the skill bundles as CONTENT-ADDRESSED erofs
# images (<sha256>.erofs + current.json stamp) under var/shared/ — the
# VZ-backend analog of `bundles-squashfs`. The Kata VZ guest kernel has
# CONFIG_EROFS_FS but no CONFIG_SQUASHFS, so macOS dev stages erofs. Each
# bundle's unpacked tree (build.sh --stage) is packed with mkfs.erofs; the
# stamp maps logical name -> sha of the .erofs file. Run the host-agent
# with ENGRAM_BUNDLE_DIR=$PWD/var/shared (the Tiltfile does this) so VZ
# dev sessions resolve/attach against it. Re-run after editing a skill —
# the stamp repoints and the host-agent restart picks up the new gen.
bundles-vz:
    #!/usr/bin/env bash
    set -euo pipefail
    command -v mkfs.erofs >/dev/null || {
        echo "mkfs.erofs not found — 'brew install erofs-utils' or use 'nix develop'" >&2
        exit 1
    }
    sha256_of() {
        if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1;
        else shasum -a 256 "$1" | cut -d' ' -f1; fi
    }
    # Reproducible erofs pack (the VZ analog of _pack.sh's SOURCE_DATE_EPOCH
    # squashfs). mkfs.erofs embeds a RANDOM fs UUID + the wallclock build time by
    # default, so the SAME tree packs to a DIFFERENT sha every run — which churns
    # every bundle's content-address on each `just bundles-vz`, repoints
    # current.json, restarts the host-agent, and strands base snapshots that
    # pinned the prior generation ("bundle materialize: blob not found"). Pin the
    # UUID, timestamp (-T0, --all-time is the default), and uid/gid (RO in-guest,
    # so ownership is irrelevant) so identical content always yields the same sha.
    # -b 4096: match the guest page size (macOS host pages are 16K, guest is 4K).
    pack_erofs() {  # pack_erofs <out.erofs> <tree-dir>
        mkfs.erofs -b 4096 -T 0 -U 00000000-0000-0000-0000-000000000000 \
            --force-uid=0 --force-gid=0 "$1" "$2" >/dev/null
    }
    mkdir -p var/shared
    stamp="{"
    sep=""
    # `sentinel` rides every reserved dyn slot at capture; skills/
    # integrations-cli/browser are catalog skills swapped in per session.
    # integrations-cli/browser need Docker and are best-effort
    # (skipped on failure), exactly as `just bundles` already degrades.
    for name in sentinel skills integrations-cli browser; do
        # Stage under the repo (absolute, $HOME-rooted), NOT `mktemp -d`: the
        # Docker-built bundles (integrations-cli/browser) bind-mount this dir into the
        # build container, and Docker Desktop on macOS does not share the
        # /var/folders path `mktemp -d` returns — the container's writes never
        # reach the host, silently producing an empty bundle. A path under the
        # repo (in $HOME) is shared, so the bind mount propagates.
        tree="$PWD/var/shared/.$name.stage"
        rm -rf "$tree"; mkdir -p "$tree"
        if ! "deploy/bundles/$name/build.sh" --stage "$tree"; then
            echo "$name bundle stage failed; skipping (sessions degrade gracefully)" >&2
            rm -rf "$tree"
            continue
        fi
        out="var/shared/.$name.build.erofs"
        rm -f "$out"
        if ! pack_erofs "$out" "$tree"; then
            echo "$name erofs pack failed; skipping" >&2
            rm -rf "$tree" "$out"
            continue
        fi
        rm -rf "$tree"
        sha="$(sha256_of "$out")"
        mv "$out" "var/shared/$sha.erofs"
        stamp="$stamp$sep\"$name\": \"$sha\""
        sep=", "
    done
    # ADR 0062: the built-in `claude` harness rides the stamp like a skill (key
    # `harness-claude`, mounted on dyn_0, exec'd as /opt/engram/dyn/0/harness).
    # Its tree — the engram-harness-claude entry binary + the pinned `claude` CLI
    # + the committed harness.toml descriptor — is the ONE bundle not assembled by
    # a build.sh: CI hands it in pre-built via ENGRAM_HARNESS_CLAUDE_TREE (the
    # `bake-harness-claude-artifact` job's downloaded artifact). A local VZ
    # `just dev` has no such artifact, so when the var is unset we build the tree
    # HERE for the arm64 Kata guest (mirroring `bake-demo`'s cross-compile), then
    # pack it exactly like FC's `bundles-squashfs` — only the format differs
    # (erofs, not squashfs). Without this the fleet stamp never carries
    # `harness-claude` and `POST /sessions` 400s with "built-in harness `claude`
    # squashfs (`harness-claude`) is not staged on any host yet".
    harness_tree="${ENGRAM_HARNESS_CLAUDE_TREE:-}"
    if [ -z "$harness_tree" ]; then
        # PINNED — keep in lockstep with ci.yml's bake-harness-claude-artifact:
        # 2.1.185 is the newest CLI that still offers AskUserQuestion headlessly
        # (cortexapps/engrams#431); bump deliberately and re-verify AUQ. The VZ
        # guest is arm64 Linux (Kata kernel), so build the musl harness + fetch
        # the linux-arm64 CLI for that arch (mirrors bake-demo's case).
        CLAUDE_VERSION=2.1.185
        case "$(uname -m)" in
            arm64 | aarch64) htarget=aarch64-unknown-linux-musl; carch=linux-arm64 ;;
            x86_64 | amd64)  htarget=x86_64-unknown-linux-musl;   carch=linux-x64  ;;
            *) echo "harness-claude: unsupported arch $(uname -m); skipping" >&2; htarget="" ;;
        esac
        if [ -n "$htarget" ]; then
            # Best-effort like the Docker bundles above: a cross-build/download
            # failure (e.g. not in `nix develop`, no musl cross toolchain) warns
            # and skips so `just dev` still comes up — just without built-in claude.
            tree="$PWD/var/shared/.harness-claude.stage"
            cache="var/shared/.cache/claude-$CLAUDE_VERSION-$carch"
            ok=1
            cargo build --release --target "$htarget" -p engram-harness-claude || ok=0
            if [ "$ok" = 1 ] && [ ! -x "$cache" ]; then
                mkdir -p "$(dirname "$cache")"
                curl -fsSL --retry 3 \
                    "https://downloads.claude.ai/claude-code-releases/$CLAUDE_VERSION/$carch/claude" \
                    -o "$cache" && chmod +x "$cache" || ok=0
            fi
            if [ "$ok" = 1 ]; then
                rm -rf "$tree"; mkdir -p "$tree"
                cp -p "target/$htarget/release/engram-harness-claude" "$tree/harness"
                cp -p "$cache" "$tree/claude"
                cp -p deploy/harness-claude/harness.toml "$tree/harness.toml"
                harness_tree="$tree"
            else
                echo "harness-claude local build failed; skipping (dev stack boots without the built-in claude)" >&2
            fi
        fi
    fi
    if [ -n "$harness_tree" ]; then
        [ -x "$harness_tree/harness" ] || {
            echo "harness tree $harness_tree is missing an executable 'harness' entry binary" >&2
            exit 1
        }
        out="var/shared/.harness-claude.build.erofs"
        rm -f "$out"
        pack_erofs "$out" "$harness_tree"
        sha="$(sha256_of "$out")"
        mv "$out" "var/shared/$sha.erofs"
        stamp="$stamp$sep\"harness-claude\": \"$sha\""
        sep=", "
        # Drop the locally-built stage tree (keep the download cache); CI's
        # externally-provided ENGRAM_HARNESS_CLAUDE_TREE is left untouched.
        [ "$harness_tree" = "$PWD/var/shared/.harness-claude.stage" ] && rm -rf "$harness_tree"
    fi
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

# Bake an image from a directory containing Dockerfile + engram.toml,
# then push it to the local OCI registry. Auto-selects the cross-compile
# target based on host arch; transport is always vsock (ADR 0066 retired
# console transport — console-baked images no longer boot). Tag defaults
# to `warm-<rfc3339>`; override with `TAG=...`.
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
        TARGET=aarch64-unknown-linux-musl; PLATFORM=linux/arm64; \
    elif [ "$(uname -s -m)" = "Linux x86_64" ]; then \
        TARGET=x86_64-unknown-linux-musl; PLATFORM=linux/amd64; \
    elif [ "$(uname -s -m)" = "Linux aarch64" ]; then \
        TARGET=aarch64-unknown-linux-musl; PLATFORM=linux/arm64; \
    else \
        echo "unsupported host: $(uname -s -m)" >&2; exit 1; \
    fi; \
    TRANSPORT=vsock; \
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
