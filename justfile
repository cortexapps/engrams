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
    cargo hakari generate --diff
    cargo nextest run --workspace

# Auto-format the workspace.
fmt:
    cargo fmt --all

# Regenerate `workspace-hack/Cargo.toml` from the current dep
# graph. Run after adding or removing a workspace dep so
# `cargo hakari generate --diff` (in `just check` and CI) stays
# green. The workspace-hack crate unifies feature sets across
# members so switching between `cargo test -p X` and
# `cargo build -p Y` doesn't re-cook shared deps under different
# features.
#
# `workspace-hack/Cargo.toml` is GENERATED. Never hand-edit it, and
# never bump a version inside it — the only correct content is what
# this recipe emits.
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
# cargo tooling; `jq` is installed on demand — the fake-codex test scripts
# shell out to it, and `rust:bookworm` doesn't ship it. `rust:bookworm`
# tracks the latest stable, matching our pinned `channel = "stable"`.
# `protoc` is installed too — every crate in the `engram-protocol`
# dependency closure (engram-host-agent among them, which owns the
# Linux-gated dirty-file recovery tests) fails its build script without it.
# Example: `just test-linux engram-harness-claude`.
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
                bash -c "command -v jq >/dev/null && command -v protoc >/dev/null || (apt-get update -qq && apt-get install -y -qq jq protobuf-compiler); cargo test -p {{pkg}} {{ARGS}}" ;;
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
# registry credentials and write it into the SHARED `.env` — the primary
# checkout's, discovered via `git rev-parse --git-common-dir`. All git
# worktrees of one clone share it, because the dev Postgres/GCS are
# machine-global: data sealed under one KEK must unseal from every
# worktree. In the primary checkout the shared file IS `./.env`, so
# behavior there is unchanged. Idempotent. A worktree-local `.env` keeps
# working for overrides (backend, kernel paths, ...) but may never carry
# a KEK — bootstrap strips one (adopting it into the shared file when
# the shared file lacks a KEK; discarding it loudly otherwise, since
# sealed state lives under the shared KEK).
bootstrap:
    #!/usr/bin/env bash
    set -euo pipefail
    shared="$(cd "$(git rev-parse --git-common-dir)/.." && pwd)"
    env_file="$shared/.env"
    [ -f "$env_file" ] || touch "$env_file"
    in_worktree=0
    [ "$shared" != "$(pwd)" ] && in_worktree=1
    local_kek=""
    if [ "$in_worktree" = 1 ] && [ -f .env ]; then
        local_kek="$(grep '^ENGRAM_KEK_MASTER_KEY=' .env | head -1 || true)"
    fi
    if grep -q '^ENGRAM_KEK_MASTER_KEY=' "$env_file"; then
        echo "ENGRAM_KEK_MASTER_KEY already present in $env_file — leaving as-is"
    elif [ -n "$local_kek" ]; then
        # First shared-aware run of a clone whose only KEK lives in a
        # worktree: adopt it rather than minting a competing one.
        printf '%s\n' "$local_kek" >> "$env_file"
        echo "adopted the worktree's ENGRAM_KEK_MASTER_KEY into $env_file"
    else
        if command -v openssl >/dev/null 2>&1; then
            KEY=$(openssl rand -base64 32)
        else
            KEY=$(head -c 32 /dev/urandom | base64)
        fi
        printf 'ENGRAM_KEK_MASTER_KEY=%s\n' "$KEY" >> "$env_file"
        echo "wrote ENGRAM_KEK_MASTER_KEY to $env_file (32 random bytes, base64)"
    fi
    if [ -n "$local_kek" ]; then
        shared_kek="$(grep '^ENGRAM_KEK_MASTER_KEY=' "$env_file" | head -1)"
        if [ "$local_kek" != "$shared_kek" ]; then
            echo "WARNING: worktree .env carried a DIFFERENT KEK than $env_file — discarding it (sealed dev data lives under the shared KEK)" >&2
        fi
        grep -v '^ENGRAM_KEK_MASTER_KEY=' .env > .env.tmp && mv .env.tmp .env
        echo "stripped ENGRAM_KEK_MASTER_KEY from the worktree .env (the shared file owns it)"
    fi

# Point this worktree's `var/shared` (content-addressed bundle store +
# stamp) and `var/bundles` (unpacked Process-backend trees) at the
# primary checkout's, so every worktree reuses the same baked bundles
# and build cache instead of re-baking from scratch. A no-op in the
# primary checkout. Existing real dirs are adopted: content-addressed
# files merge in (identical names are identical bytes), then the dir is
# replaced by a symlink. Safe concurrently — only one tilt stack can run
# at a time (machine-global compose ports).
dev-link-shared:
    #!/usr/bin/env bash
    set -euo pipefail
    shared="$(cd "$(git rev-parse --git-common-dir)/.." && pwd)"
    if [ "$shared" = "$(pwd)" ]; then
        exit 0
    fi
    mkdir -p var
    for d in shared bundles; do
        tgt="$shared/var/$d"
        loc="var/$d"
        mkdir -p "$tgt"
        if [ -L "$loc" ]; then
            if [ "$(readlink "$loc")" != "$tgt" ]; then
                rm "$loc"
                ln -s "$tgt" "$loc"
            fi
            continue
        fi
        if [ -d "$loc" ]; then
            # Adopt: merge content-addressed artifacts + cache fingerprints,
            # keep the shared copy on any name collision, drop the rest
            # (everything under var/ is regenerable).
            shopt -s nullglob dotglob
            for f in "$loc"/*; do
                base="$(basename "$f")"
                if [ "$base" = ".fingerprints" ] && [ -d "$f" ]; then
                    mkdir -p "$tgt/.fingerprints"
                    for fp in "$f"/*; do
                        mv -n "$fp" "$tgt/.fingerprints/" 2>/dev/null || true
                    done
                elif [ -f "$f" ]; then
                    mv -n "$f" "$tgt/" 2>/dev/null || true
                fi
            done
            shopt -u nullglob dotglob
            rm -rf "$loc"
            echo "adopted $loc into $tgt"
        fi
        ln -s "$tgt" "$loc"
        echo "linked $loc -> $tgt"
    done

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
        bun x drizzle-kit migrate

# Provision an orchestrator login (ADR 0051 better-auth). Prompts for
# email / admin? / password, then creates the user through the running
# orchestrator's sign-up endpoint (so the password is hashed exactly like
# a real sign-up) and, if you asked for admin, promotes the role via SQL.
# Assumes the stack is already up (`just dev`) — it talks to the live
# orchestrator on :8787 and the postgres container. Idempotent: an
# already-existing email is tolerated and (if admin) still gets promoted.
orchestrator-user:
    #!/usr/bin/env bash
    set -euo pipefail
    ORIGIN="http://localhost:5173"
    PORT="${ORCHESTRATOR_PORT:-8787}"
    URL="http://127.0.0.1:${PORT}/api/auth/sign-up/email"

    read -rp "Email: " EMAIL
    [[ -n "$EMAIL" ]] || { echo "email is required" >&2; exit 1; }
    read -rp "Admin? [y/N]: " ADMIN_ANS
    read -rsp "Password (min 8 chars): " PASSWORD; echo
    [[ ${#PASSWORD} -ge 8 ]] || { echo "password must be at least 8 characters" >&2; exit 1; }
    NAME="${EMAIL%@*}"

    echo "→ creating ${EMAIL} via ${URL}"
    # Password goes through jq's env (not argv) and reaches curl over stdin,
    # so it never lands in a process arg list.
    BODY="$(PW="$PASSWORD" jq -n --arg e "$EMAIL" --arg n "$NAME" \
        '{email:$e, password:env.PW, name:$n}')"
    CODE="$(printf '%s' "$BODY" | curl -sS -o /tmp/orchestrator-user.out -w '%{http_code}' \
        -X POST "$URL" -H 'content-type: application/json' -H "origin: ${ORIGIN}" --data @-)"

    if [[ "$CODE" == "200" ]]; then
        echo "✓ user created"
    elif grep -qi 'already' /tmp/orchestrator-user.out; then
        echo "• user already exists — continuing"
    else
        echo "✗ sign-up failed (HTTP ${CODE}):" >&2
        cat /tmp/orchestrator-user.out >&2; echo >&2
        echo "  (is the orchestrator running? \`just dev\`)" >&2
        exit 1
    fi
    rm -f /tmp/orchestrator-user.out

    if [[ "$ADMIN_ANS" =~ ^[Yy] ]]; then
        EMAIL_SQL="${EMAIL//\'/\'\'}"
        echo "→ promoting ${EMAIL} to admin"
        docker compose -f deploy/docker-compose.dev.yml exec -T postgres \
            psql -U engram -d engram_orchestrator \
            -c "UPDATE \"user\" SET role='admin' WHERE email='${EMAIL_SQL}'"
        echo "✓ role set to admin — sign out and back in to pick it up"
    fi

    docker compose -f deploy/docker-compose.dev.yml exec -T postgres \
        psql -U engram -d engram_orchestrator \
        -c "SELECT email, coalesce(role,'user') AS role FROM \"user\" WHERE email='${EMAIL//\'/\'\'}'"

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

# ADR 0082: like `just dev`, but the host-agent (+ its Firecracker stack)
# runs INSIDE a dedicated Colima VM instead of as a Mac-local process —
# the only way to exercise the FC-only surfaces (NBD, UFFD, netns egress,
# squashfs patch-drives) on Apple Silicon. Coordinator/orchestrator/web/
# compose stay on the Mac exactly as in plain `just dev`; only the
# Tiltfile's host-agent resource moves. The env var is the real switch —
# the Tiltfile reads ENGRAM_FC_COLIMA_PROFILE directly — this recipe is
# sugar so you don't have to remember its name. First-time setup:
# `just fc-colima-provision [profile]`.
dev-fc profile='fc-dev' mac_docker_context='colima':
    #!/usr/bin/env bash
    set -euo pipefail
    # ADR 0082: only the host-agent + FC stack run in the Colima VM (as a raw
    # `colima ssh` process, NOT a container); the docker-compose deps stay on the
    # Mac. `colima start` persistently repoints the docker CLI at the VM daemon
    # (writes currentContext=colima-<profile> to ~/.docker/config.json), so a
    # plain `tilt up` makes Tilt's docker_compose() deploy the deps INTO the VM —
    # where the VM's localhost→Mac DNAT (engram-dev-fwd) routes registry/GCS
    # traffic away from them and the Mac-side stack can't reach them. Pin
    # DOCKER_HOST to the Mac docker for the tilt process ONLY (no global-context
    # mutation — other shells keep whatever colima set). Mac context defaults to
    # `colima` (the default-profile daemon, per ADR 0082 "default Colima docker
    # daemon untouched"); for Docker Desktop: `just dev-fc {{profile}} desktop-linux`.
    mac_host="$(docker context inspect '{{mac_docker_context}}' 2>/dev/null \
        | python3 -c 'import sys,json; print(json.load(sys.stdin)[0]["Endpoints"]["docker"]["Host"])' 2>/dev/null || true)"
    if [ -z "$mac_host" ]; then
        echo "dev-fc: docker context '{{mac_docker_context}}' not found or has no docker endpoint. Available:" >&2
        docker context ls >&2
        echo "Pass one explicitly, e.g.: just dev-fc {{profile}} desktop-linux" >&2
        exit 1
    fi
    echo "dev-fc: compose deps -> Mac docker '{{mac_docker_context}}' ($mac_host); host-agent -> colima VM '{{profile}}'"
    ENGRAM_FC_COLIMA_PROFILE={{profile}} DOCKER_HOST="$mac_host" tilt up

# ADR 0082: create/update the named Colima VM (aarch64 Ubuntu, nested
# virt, /dev/kvm, Firecracker + the aarch64 guest kernel, NBD/UFFD host
# prep, the localhost→Mac DNAT unit engram-dev-fwd) — everything `dev-fc` needs
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
#   3. Prune stale RO skill bundles in var/shared: any `<sha>.{erofs,squashfs}`
#      NOT referenced by the live `current.json` stamp. Old generations pile up
#      on every re-bundle, and a VZ→FC backend switch strands the ENTIRE .erofs
#      set (VZ stages erofs; FC stages squashfs and can't mount erofs) — that
#      alone was ~9 GiB. When ENGRAM_FC_COLIMA_PROFILE is set (ADR 0082) the
#      same prune runs inside the VM's /opt/engram-dev/shared, where the synced
#      bundles actually consume the small VM disk.
#   4. (ADR 0082, fc-colima only) Sweep orphaned base-snapshot dirs in the VM:
#      /opt/engram-dev/var/sandboxes/snapshots/<id> with no live coordinator DB
#      row. The snapshot GC works off DB rows, so a dir left by a hard DB delete
#      or a FAILED capture is never reclaimed — and each holds a GiB-sized
#      memory dump that fills the small VM disk. Live ids come from Postgres.
#
# Checkpoints (snapshots) are otherwise reclaimed by the coordinator's
# snapshot/chunk GC once their owning sessions are gone; the warm-pool base
# snapshot an enabled image clones from is preserved (re-captured on re-enable).
#
# Use it to reclaim disk or get a clean slate before a re-bake. Drives the
# LOCAL orchestrator (:8787) via the `engrams` CLI (bun, cli/) with
# ENGRAMS_API_KEY auth; the stack must be up.
reap-sessions profile='' mac_docker_context='colima':
    #!/usr/bin/env bash
    set -euo pipefail
    # The `engrams` CLI drives the orchestrator (the coordinator is internal).
    # The URL is HARDCODED to the local dev stack — the CLI's default host is
    # prod, and a stray ENGRAMS_URL (or a stored prod login in hosts.json) must
    # never point a reaper at it. Auth is ENGRAMS_API_KEY only: from the env,
    # else var/dev-api-key (`just dev` seeds it via Tilt's dev-api-key
    # resource); the env var also stops the CLI falling back to hosts.json.
    export ENGRAMS_URL="http://localhost:8787"
    export ENGRAMS_API_KEY="${ENGRAMS_API_KEY:-$(cat var/dev-api-key 2>/dev/null || true)}"
    [ -n "$ENGRAMS_API_KEY" ] || { echo "no ENGRAMS_API_KEY and no var/dev-api-key — is the stack up (just dev)?" >&2; exit 1; }
    # fc-colima profile for the VM-side stages (3 bundles, 4 snapshots). The
    # `{{profile}}` param wins; else fall back to ENGRAM_FC_COLIMA_PROFILE. Unlike
    # `just dev-fc`, a bare `just reap-sessions` has NO env var set (dev-fc sets it
    # inline for tilt only), so the VM stages used to silently skip — pass the
    # profile explicitly: `just reap-sessions fc-dev`.
    fc_profile="{{profile}}"; [ -z "$fc_profile" ] && fc_profile="${ENGRAM_FC_COLIMA_PROFILE:-}"
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
    (cd cli && bun install --silent)
    cli() { bun cli/src/main.ts "$@"; }
    before_kb="$(dir_kb "$sandboxes_dir")"; before_kb="${before_kb:-0}"
    # 1. Reap every live session via the production DeleteSession path.
    echo "==> loading sessions from the orchestrator"
    ids="$(cli --json session list \
        | python3 -c 'import sys,json;print("\n".join(s["id"] for s in json.load(sys.stdin)["sessions"]))')"
    total=0; for id in $ids; do total=$((total+1)); done
    echo "found $total live session(s)"
    count=0
    for id in $ids; do
        printf '==> reaping %s ... ' "$id"
        if cli session delete "$id" >/dev/null 2>&1; then echo deleted; count=$((count+1)); else echo "(skipped)"; fi
    done
    echo "reaped $count live session(s)."
    # 2. Sweep orphaned rootfs files no still-live session owns.
    echo "==> sweeping orphaned sandbox rootfs files"
    live=" $(cli --json session list \
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
    # 3. Prune stale RO skill bundles (var/shared): <sha>.{erofs,squashfs} not
    #    referenced by current.json. Keeps the download cache (.cache) and the
    #    hidden .*.build.* / .*.stage temp entries (dotfiles don't match the
    #    non-dot globs below). No current.json -> nothing is "live", so skip
    #    rather than nuke the lot.
    prune_bundles() {  # prune_bundles <dir>   (runs on the Mac; var/shared)
        local d="$1" keep sha f pruned=0
        [ -f "$d/current.json" ] || { echo "  ($d: no current.json — skip)"; return 0; }
        keep="$(grep -oE '[0-9a-f]{64}' "$d/current.json" | sort -u)"
        shopt -s nullglob
        for f in "$d"/*.erofs "$d"/*.squashfs; do
            sha="$(basename "$f")"; sha="${sha%.*}"
            printf '%s\n' "$keep" | grep -qx "$sha" && continue
            rm -f "$f"; pruned=$((pruned+1))
        done
        echo "  pruned $pruned stale bundle(s) from $d"
    }
    echo "==> pruning stale skill bundles (var/shared)"
    before_shared_kb="$(dir_kb var/shared)"
    prune_bundles var/shared
    after_shared_kb="$(dir_kb var/shared)"
    shared_freed=$(( before_shared_kb > after_shared_kb ? before_shared_kb - after_shared_kb : 0 ))
    [ "$shared_freed" -gt 0 ] && echo "  reclaimed $(human_kb "$shared_freed") from var/shared."
    # ADR 0082: the synced bundles live on the small fc-colima VM disk — prune
    # the VM's /opt/engram-dev/shared the same way. The prune runs from a
    # helper piped to `sudo bash -s` over stdin (NOT a heredoc: an unindented
    # heredoc terminator would break `just`'s recipe indentation, and colima
    # ssh doesn't run remote args through a shell anyway).
    if [ -n "$fc_profile" ] && command -v colima >/dev/null 2>&1; then
        echo "==> pruning stale bundles inside the fc-colima VM ($fc_profile)"
        colima ssh --profile "$fc_profile" -- sudo bash -s < deploy/dev/reap-vm-bundles.sh
        # 4. Sweep orphaned base-snapshot dirs in the VM: the coordinator's
        #    snapshot GC works off DB rows, so a snapshot dir left by a hard DB
        #    delete or a failed capture is never reclaimed — and each carries a
        #    GiB-sized memory dump that fills the small VM disk. Gather the live
        #    snapshot-id set from Postgres (via the always-up dev container) and
        #    pass it (space-joined) to the VM sweeper; anything else is orphaned.
        echo "==> sweeping orphaned base-snapshot dirs in the VM"
        # The compose deps (Postgres) live on the MAC docker daemon, but
        # `colima start --profile <p>` persistently repoints the docker CLI at
        # the VM daemon (currentContext=colima-<p>). So a bare `docker compose
        # exec postgres` here hits the VM — which has no Postgres — the query
        # fails, and (guarded in reap-vm-snapshots.sh) the sweep is refused
        # rather than nuking every base snapshot. Pin the Mac docker context
        # like `dev-fc` does. Default `colima`; for Docker Desktop:
        # `just reap-sessions {{profile}} desktop-linux`.
        mac_host="$(docker context inspect '{{mac_docker_context}}' 2>/dev/null \
            | python3 -c 'import sys,json; print(json.load(sys.stdin)[0]["Endpoints"]["docker"]["Host"])' 2>/dev/null || true)"
        if [ -z "$mac_host" ]; then
            echo "    docker context '{{mac_docker_context}}' not found or has no endpoint. Available:" >&2
            docker context ls >&2
            echo "    Pass the Mac context, e.g.: just reap-sessions $fc_profile desktop-linux" >&2
            exit 1
        fi
        # No `2>/dev/null` on the query: a failure must be VISIBLE (paired with
        # the empty-set guard in the sweeper) — a silently-swallowed error used
        # to abort with a cryptic exit code, or worse, feed an empty live set.
        live_snaps="$(DOCKER_HOST="$mac_host" docker compose -f deploy/docker-compose.dev.yml exec -T postgres \
            psql -U engram -d engram -tAc \
            "select id::text from snapshots union select base_snapshot_id::text from enabled_images where base_snapshot_id is not null" \
            | tr '\n' ' ' | tr -s ' ')"
        colima ssh --profile "$fc_profile" -- sudo bash -s -- "$live_snaps" < deploy/dev/reap-vm-snapshots.sh
    elif command -v colima >/dev/null 2>&1; then
        echo "==> SKIPPING fc-colima VM stages (bundles + orphaned snapshots) — no profile."
        echo "    The VM holds the GiB-sized base-snapshot dumps; pass the profile to sweep them:"
        echo "      just reap-sessions fc-dev"
    fi
    after_kb="$(dir_kb "$sandboxes_dir")"; after_kb="${after_kb:-0}"
    reclaimed_kb=$(( before_kb > after_kb ? before_kb - after_kb : 0 ))
    if [ "$reclaimed_kb" -gt 0 ]; then
        echo "reclaimed $(human_kb "$reclaimed_kb") of dev sandbox disk."
    else
        echo "no dev sandbox disk reclaimed (already clean)."
    fi
    echo "reap-sessions: done. Checkpoints GC in the background once their sessions are gone."

# Clear stale bundle generations / snapshot blobs / chunks from BLOB STORAGE
# via the coordinator's own GC RPCs (ADR 0035 §5 / ADR 0028 addendum / ADR
# 0016 Phase C). The pin set is Postgres truth — anything referenced by a
# live snapshot or the mount/harness catalogs is never touched. Two steps:
#   1. Deregister DEAD host rows. A dead leftover host-agent (e.g. the
#      fc-colima VM's after `just dev-fc`, killed or VM stopped) otherwise
#      keeps feeding the fleet bundle catalog — VZ sessions then resolve the
#      FC host's SQUASHFS generations, which the VZ guest kernel can't mount
#      ("cannot find valid erofs superblock" → the agentd-bundle panic) —
#      and keeps winning session placement. DeleteHost refuses (FAILED_
#      PRECONDITION) while sessions are still bound, so this can't drop a
#      row out from under live work; `ready`/`draining` hosts are never
#      touched (the dead-host detector owns that transition).
#   2. Run the three sweeps with a zero grace window so unpinned blobs
#      delete in the same pass — the dev "clear it now" posture (prod
#      trusts the background loop's 24h grace).
# Blob generations are pinned by snapshots: run `just reap-sessions` FIRST if
# old sessions still reference the generations you want gone. Local staged-
# file pruning (var/shared on the Mac, /opt/engram-dev/shared in the fc VM)
# also lives in reap-sessions. The stack must be up — the `engrams` CLI
# drives the orchestrator (:8787), which forwards FleetService (admin-gated).
#
# Deregister dead host rows + GC stale bundle/snapshot/chunk blobs (grace 0).
reap-bundles:
    #!/usr/bin/env bash
    set -euo pipefail
    # Hardcoded local orchestrator + ENGRAMS_API_KEY auth — same rationale as
    # reap-sessions: never let env/hosts.json aim a GC pass at prod.
    export ENGRAMS_URL="http://localhost:8787"
    export ENGRAMS_API_KEY="${ENGRAMS_API_KEY:-$(cat var/dev-api-key 2>/dev/null || true)}"
    [ -n "$ENGRAMS_API_KEY" ] || { echo "no ENGRAMS_API_KEY and no var/dev-api-key — is the stack up (just dev)?" >&2; exit 1; }
    (cd cli && bun install --silent)
    cli() { bun cli/src/main.ts "$@"; }
    echo "==> deregistering dead host rows"
    dead="$(cli --json host list \
        | python3 -c 'import sys,json;print("\n".join(h["id"] for h in json.load(sys.stdin)["hosts"] if h["status"]=="dead"))')"
    if [ -z "$dead" ]; then
        echo "  (no dead hosts)"
    else
        for id in $dead; do
            printf '  deleting dead host %s ... ' "$id"
            if cli host delete "$id" >/dev/null 2>&1; then
                echo deleted
            else
                echo "SKIPPED (sessions still bound? reap those first: just reap-sessions)"
            fi
        done
    fi
    echo "==> GC sweeps (apply, grace 0)"
    cli admin gc --apply --grace-secs 0
    echo "reap-bundles: done."

# Params forward to reap-sessions (fc-colima profile + Mac docker context for
# the VM-side stages): `just reap-all fc-dev`. Order is load-bearing: deleting
# sessions first drops their snapshots' pins, so the bundle/blob GC pass that
# follows can actually delete the generations they were holding.
#
# Full dev-stack reclaim: reap-sessions, then reap-bundles.
reap-all profile='' mac_docker_context='colima': (reap-sessions profile mac_docker_context) reap-bundles

# Bake the canonical `demo` image (deploy/demo/) and push it to the local OCI
# registry as demo:warm-1 (localhost:5001, the registry `just dev` runs). Arch +
# transport are detected; no per-backend recipe. ADR 0062: the image carries NO
# harness — the built-in `claude` harness is a per-session selection staged on
# the fleet, not baked in. Requires `just bootstrap` (KEK) and a running local
# registry (it's up under `just dev`, or `just registry-up`). This only PUSHES;
# use `just bake-demo-enable` to also make it live on the coord.
bake-demo:
    bash deploy/dev/bake-demo.sh

# Bake the demo image AND make it live on the running coord in one step — the
# inner-loop cycle after editing deploy/demo/ (engram.toml vcpus/mem, or the
# rootfs). `bake-demo` only pushes; this then registers it and BLOCKS until the
# base-snapshot capture (the enable job) reports ready, exiting non-zero if it
# fails. Because `warm-1` is a fixed tag, a re-bake moves it to a NEW digest: if
# the image is already enabled we `image refresh` (re-fetch the moved tag + force
# a re-capture), since `image enable` is idempotent on an already-enabled URI and
# would keep serving the STALE base snapshot. Requires the stack up
# (`just dev` / `just dev-fc`).
bake-demo-enable:
    #!/usr/bin/env bash
    set -euo pipefail
    bash deploy/dev/bake-demo.sh
    # The `engrams` CLI drives the orchestrator; `just dev` seeds the admin
    # credential (Tilt's dev-api-key resource → var/dev-api-key).
    export ENGRAMS_URL="${ENGRAMS_URL:-http://localhost:8787}"
    export ENGRAMS_API_KEY="${ENGRAMS_API_KEY:-$(cat var/dev-api-key 2>/dev/null || true)}"
    [ -n "$ENGRAMS_API_KEY" ] || { echo "no ENGRAMS_API_KEY and no var/dev-api-key — is the stack up (just dev)?" >&2; exit 1; }
    (cd cli && bun install --silent)
    cli() { bun cli/src/main.ts "$@"; }
    uri=localhost:5001/demo:warm-1
    if cli --json image list \
        | python3 -c "import sys,json; sys.exit(0 if any(i.get('image_uri')=='$uri' for i in json.load(sys.stdin).get('images',[])) else 1)"; then
        echo "==> $uri already enabled — refreshing (re-fetch moved tag + re-capture base snapshot)"
        cli image refresh --uri "$uri" --recapture
    else
        echo "==> enabling $uri (captures base snapshot)"
        cli image enable --uri "$uri"
    fi

# Fetch the kernel artifact this host's backend needs (VZ → Kata arm64
# kernel; Firecracker → FC test kernel+rootfs; process → nothing).
pull-kernel:
    bash deploy/dev/pull-kernel.sh

# Stage the ADR 0027 RO bundles (skills, integrations-cli, browser, ide) as
# UNPACKED trees under var/bundles/ for the dev ProcessBackend, which symlinks
# them in instead of mounting a squashfs. `skills` is a plain copy;
# `integrations-cli`/`browser`/`ide` need Docker (glibc builds) and are
# best-effort — skip them and only skills get wired (no browser/IDE tooling in
# dev). Re-run after editing a skill. Depends on dev-link-shared so a fresh
# worktree stages into the shared store, never a divergent local dir.
bundles: dev-link-shared
    deploy/bundles/skills/build.sh --stage var/bundles/skills
    deploy/bundles/integrations-cli/build.sh --stage var/bundles/integrations-cli \
        || echo "integrations-cli bundle skipped (needs Docker) — dev sessions get no integration CLIs"
    deploy/bundles/browser/build.sh --stage var/bundles/browser \
        || echo "browser bundle skipped (needs Docker) — dev sessions get no browser"
    deploy/bundles/ide/build.sh --stage var/bundles/ide \
        || echo "ide bundle skipped (needs Docker) — dev sessions get no IDE"

# Stage the host-native built-in harnesses plus the core skills bundle for the
# Linux ProcessBackend. This is the inner `just dev` path in dev-engrams: no
# squashfs mount exists, so the backend symlinks these unpacked trees into each
# selected dyn slot. The script caches the large upstream CLI payloads.
bundles-process: dev-link-shared
    bash deploy/dev/stage-process-bundles.sh

# ADR 0035/0055: build + stage the squashfs bundles CONTENT-ADDRESSED
# (<sha256>.squashfs + current.json stamp) under var/shared/, the
# dev mirror of the FC-host image's /var/lib/engram/shared. Run the
# host-agent with ENGRAM_BUNDLE_DIR=$PWD/var/shared so FC dev sessions
# resolve/capture against it. Needs mksquashfs: on Linux that's
# `apt install squashfs-tools`; on macOS (ADR 0082's fc-colima dev mode —
# FC itself still only ever boots inside the Colima VM) run this from
# `nix develop`, which provides mksquashfs on darwin too. Re-run after
# editing a skill — the stamp repoints and new sessions pick the fresh
# generation up via the §3 swap.
#
# Unchanged bundles are NOT rebuilt: deploy/bundles/_cache.sh fingerprints each
# bundle's inputs and reuses the <sha>.squashfs those inputs produced last time
# (the Docker bundles each cost a container + apt-get + several downloads, paid
# on every Tilt trigger before this). Run with ENGRAM_BUNDLES_FORCE=1 to ignore
# the cache — the fingerprint covers tracked files and pinned versions, not the
# floating apt/base-image layers the Docker bundles pull. Depends on
# dev-link-shared so a fresh worktree builds into (and reuses) the shared
# content-addressed store, never a divergent local dir.
bundles-squashfs: dev-link-shared
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p var/shared
    . deploy/bundles/_cache.sh
    stamp="{"
    sep=""
    add_stamp() { stamp="$stamp$sep\"$1\": \"$2\""; sep=", "; }
    # The host arch selects a different download in several build scripts
    # (ttyd, code-server, the CLIs), so it is part of every fingerprint.
    arch="arch=$(uname -m)"
    # ADR 0055: `sentinel` rides every reserved dyn-* slot; skills/
    # integrations-cli/browser/ide are catalog skills swapped in per session;
    # guest-tools (ADR 0080 §D) carries the pinned static ttyd for the SHELL
    # tab (reserved slot dyn_2). Files are content-keyed (<sha>.squashfs);
    # the stamp maps logical name -> sha.
    for name in sentinel skills integrations-cli browser ide guest-tools; do
        # These bundles assemble their own tree, so their inputs are exactly
        # the bundle dir + the shared packer.
        fp="$(bundle_fingerprint "deploy/bundles/$name" deploy/bundles/_pack.sh "$arch")"
        if sha="$(bundle_cache_get var/shared "$name" "$fp")"; then
            echo "==> $name bundle unchanged — reusing $sha.squashfs"
            add_stamp "$name" "$sha"
            continue
        fi
        tmp="var/shared/.$name.build.squashfs"
        if ! "deploy/bundles/$name/build.sh" "$tmp"; then
            echo "$name bundle build failed; skipping (sessions degrade gracefully)" >&2
            rm -f "$tmp"
            continue
        fi
        sha="$(sha256sum "$tmp" | cut -d' ' -f1)"
        mv "$tmp" "var/shared/$sha.squashfs"
        bundle_cache_put var/shared "$name" "$fp" "$sha"
        add_stamp "$name" "$sha"
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
        # and bake-images.yml. Questions and plan approval ride the injected
        # MCP tools (ask_user_question / exit_plan_mode) since the CLI removed
        # the AskUserQuestion + ExitPlanMode built-ins from headless mode
        # (cortexapps/engrams#431). Bump deliberately and re-verify the
        # deferred-tool spine: defer parks the turn, `--resume` re-fires
        # id-stable, and the MCP bridge serves the stash.
        CLAUDE_VERSION=2.1.212
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
        # The tree IS the content here (build.sh only packs it), so fingerprint
        # the staged tree rather than deploy/bundles/harness-claude. The cargo
        # build above still runs — it is the only way to learn the binary's
        # content — but an unchanged tree skips the pack.
        fp="$(bundle_fingerprint "$harness_tree" deploy/bundles/harness-claude deploy/bundles/_pack.sh)"
        if sha="$(bundle_cache_get var/shared harness-claude "$fp")"; then
            echo "==> harness-claude bundle unchanged — reusing $sha.squashfs"
        else
            tmp="var/shared/.harness-claude.build.squashfs"
            deploy/bundles/harness-claude/build.sh "$harness_tree" "$tmp"
            sha="$(sha256sum "$tmp" | cut -d' ' -f1)"
            mv "$tmp" "var/shared/$sha.squashfs"
            bundle_cache_put var/shared harness-claude "$fp" "$sha"
        fi
        add_stamp harness-claude "$sha"
        # Drop the locally-built stage tree (keep the download cache); an
        # externally-provided ENGRAM_HARNESS_CLAUDE_TREE is left untouched.
        [ "$harness_tree" = "$PWD/var/shared/.harness-claude.stage" ] && rm -rf "$harness_tree"
    fi
    # Built-in Codex tree. CI supplies the already-staged tree; local dev
    # cross-builds the wrapper and downloads the checksum-verified official
    # Linux package for the guest architecture.
    codex_tree="${ENGRAM_HARNESS_CODEX_TREE:-}"
    if [ -z "$codex_tree" ]; then
        case "$(uname -m)" in
            arm64 | aarch64) codex_arch=aarch64; codex_target=aarch64-unknown-linux-musl ;;
            x86_64 | amd64) codex_arch=x86_64; codex_target=x86_64-unknown-linux-musl ;;
            *) codex_arch="" ;;
        esac
        if [ -n "$codex_arch" ]; then
            tree="$PWD/var/shared/.harness-codex.stage"
            if cargo build --release --target "$codex_target" -p engram-harness-codex \
                && deploy/harness-codex/stage.sh "$codex_arch" \
                    "target/$codex_target/release/engram-harness-codex" "$tree"; then
                codex_tree="$tree"
            else
                echo "harness-codex local build failed; skipping" >&2
            fi
        fi
    fi
    if [ -n "$codex_tree" ]; then
        [ -x "$codex_tree/harness" ] && [ -x "$codex_tree/codex" ] || {
            echo "Codex harness tree must contain executable harness + codex" >&2
            exit 1
        }
        fp="$(bundle_fingerprint "$codex_tree" deploy/bundles/harness-codex deploy/bundles/_pack.sh)"
        if sha="$(bundle_cache_get var/shared harness-codex "$fp")"; then
            echo "==> harness-codex bundle unchanged — reusing $sha.squashfs"
        else
            tmp="var/shared/.harness-codex.build.squashfs"
            deploy/bundles/harness-codex/build.sh "$codex_tree" "$tmp"
            sha="$(sha256sum "$tmp" | cut -d' ' -f1)"
            mv "$tmp" "var/shared/$sha.squashfs"
            bundle_cache_put var/shared harness-codex "$fp" "$sha"
        fi
        add_stamp harness-codex "$sha"
        [ "$codex_tree" = "$PWD/var/shared/.harness-codex.stage" ] && rm -rf "$codex_tree"
    fi
    # ADR 0080: the agentd bundle (reserved slot dyn_1) — MANDATORY, not
    # best-effort: without it no guest can boot (the stage-1 init execs
    # agentd out of this mount) and no capture can run. CI/e2e hands the
    # built musl binary in via ENGRAM_AGENTD_BIN; a local run cross-builds
    # it for the host arch (FC guests match the host).
    agentd_bin="${ENGRAM_AGENTD_BIN:-}"
    if [ -z "$agentd_bin" ]; then
        case "$(uname -m)" in
            arm64 | aarch64) atarget=aarch64-unknown-linux-musl ;;
            *)               atarget=x86_64-unknown-linux-musl ;;
        esac
        cargo build --release --target "$atarget" -p engram-agentd
        agentd_bin="target/$atarget/release/engram-agentd"
    fi
    fp="$(bundle_fingerprint "$agentd_bin" deploy/bundles/agentd deploy/bundles/_pack.sh)"
    if sha="$(bundle_cache_get var/shared agentd "$fp")"; then
        echo "==> agentd bundle unchanged — reusing $sha.squashfs"
    else
        tmp="var/shared/.agentd.build.squashfs"
        deploy/bundles/agentd/build.sh "$agentd_bin" "$tmp"
        sha="$(sha256sum "$tmp" | cut -d' ' -f1)"
        mv "$tmp" "var/shared/$sha.squashfs"
        bundle_cache_put var/shared agentd "$fp" "$sha"
    fi
    add_stamp agentd "$sha"
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

# ADR 0080: build a session image from a directory containing a
# Dockerfile and push it to the local OCI registry — a PLAIN
# `docker build && docker push` (the user contract). The coordinator
# materializes the rootfs host-side at enable time; nothing is chunked
# or injected locally. Tag defaults to `warm-<rfc3339>`; override TAG=...
#
# Usage:
#   just bake cortex/api ./examples/api
#   just bake cortex/api .
#   TAG=warm-2 just bake cortex/api ./examples/api
#
# Then enable it (once, applying runtime config out-of-band):
#   engram image enable --uri localhost:5001/<repo>:<tag> --config <toml>
# and reference it: `engram session create --image localhost:5001/<repo>:<tag>`.
bake repo dir='.':
    @set -e; \
    if [ "$(uname -s -m)" = "Darwin arm64" ]; then \
        PLATFORM=linux/arm64; \
    elif [ "$(uname -s -m)" = "Linux x86_64" ]; then \
        PLATFORM=linux/amd64; \
    elif [ "$(uname -s -m)" = "Linux aarch64" ]; then \
        PLATFORM=linux/arm64; \
    else \
        echo "unsupported host: $(uname -s -m)" >&2; exit 1; \
    fi; \
    TAG="${TAG:-warm-$(date -u +%Y%m%dT%H%M%SZ)}"; \
    URI="localhost:5001/{{repo}}:$TAG"; \
    docker build --platform "$PLATFORM" -t "$URI" -f "{{dir}}/Dockerfile" "{{dir}}"; \
    docker push "$URI"; \
    echo ""; \
    echo "✓ pushed $URI"; \
    echo "  enable it: engram image enable --uri $URI --config <image-config.toml>"; \
    echo "  then:      engram session create --image $URI ..."

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

# Ad-hoc codesign the engram-sandbox-vz test binaries with the
# com.apple.security.virtualization entitlement. Without this, every
# VZ API call returns NSError "process doesn't have the
# com.apple.security.virtualization entitlement" — see the smoke test
# in crates/engram-sandbox-vz/src/vm.rs.
#
# The build MUST be `cargo nextest run --no-run` — the exact
# invocation shape `vz-test` runs with. A plain `cargo build -p a -p b
# --tests` resolves features differently (AGENTS.md: `-p X -p Y`
# invalidates the cache), so nextest would silently RECOMPILE fresh,
# unsigned test binaries after we signed the stale ones — the live VZ
# tests then die on the missing entitlement (ADR 0096).
#
# Idempotent: re-running on an already-signed binary is a no-op
# beyond a few ms of cycle. `vz-test` and the Tiltfile depend on it
# (the Tiltfile builds + signs `engram-host-agent` itself, atomically
# with its own build — not here).
#
# We sign with the ad-hoc identity (`-`), which is enough for
# locally-built dev binaries on Apple Silicon. CI does the same.
# Distribution to other machines would need a real signing
# identity + notarization; out of scope here.
vz-codesign:
    @if [ "$(uname -s)" != "Darwin" ]; then \
        echo "vz-codesign is macOS-only; skipping" >&2; exit 0; \
    fi
    cargo nextest run -p engram-sandbox-vz --no-run
    bash crates/engram-sandbox-vz/scripts/codesign.sh debug

# Run the engram-sandbox-vz crate's unit tests, then the live VZ
# tests gated behind #[ignore]. Codesigns first so the entitlement
# check passes when the test reaches into VZ. (`--run-ignored
# ignored-only` is nextest's native spelling; the old `-- --ignored`
# worked only via libtest-compat emulation — CI already uses this.)
vz-test: vz-codesign
    cargo nextest run -p engram-sandbox-vz
    cargo nextest run -p engram-sandbox-vz --run-ignored ignored-only

# ADR 0096: stage the MINIMAL bundle set the live VZ e2e boots with —
# agentd (hard: the init shim execs it out of its slot, ADR 0080),
# guest-tools (the SHELL-tab ttyd the lifecycle e2e asserts), and the
# sentinel — into var/vz-e2e/shared/ (content-addressed squashfs +
# current.json, the exact layout `resolve_agentd_slot` reads). A
# stripped-down `bundles-squashfs` (no harnesses, no Docker bundles)
# so the e2e stages in seconds; the per-bundle build.sh scripts do
# the reproducible mksquashfs pack themselves (deploy/bundles/_pack.sh,
# SOURCE_DATE_EPOCH=0).
vz-test-bundles:
    #!/usr/bin/env bash
    set -euo pipefail
    sha256_of() {
        if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1;
        else shasum -a 256 "$1" | cut -d' ' -f1; fi
    }
    out=var/vz-e2e/shared
    mkdir -p "$out"
    stamp="{"
    sep=""
    for name in sentinel guest-tools; do
        img="$out/.$name.build.squashfs"
        rm -f "$img"
        "deploy/bundles/$name/build.sh" "$img" >/dev/null
        sha="$(sha256_of "$img")"
        mv "$img" "$out/$sha.squashfs"
        stamp="$stamp$sep\"$name\": \"$sha\""
        sep=", "
    done
    # agentd — always the arm64 musl build (the VZ guest is arm64 Linux).
    cargo build --release --target aarch64-unknown-linux-musl -p engram-agentd
    agentd_bin="target/aarch64-unknown-linux-musl/release/engram-agentd"
    img="$out/.agentd.build.squashfs"
    rm -f "$img"
    deploy/bundles/agentd/build.sh "$agentd_bin" "$img" >/dev/null
    sha="$(sha256_of "$img")"
    mv "$img" "$out/$sha.squashfs"
    stamp="$stamp$sep\"agentd\": \"$sha\""
    echo "$stamp}" > "$out/current.json"
    cat "$out/current.json"

# ADR 0096: the ONE-COMMAND live VZ e2e. Stages everything from HEAD —
# kernel, bundles, a fresh Docker-free rootfs (real init shim + mkext4),
# codesigned test binaries — then boots real VMs through the whole
# suite. Rebuilt every run, so the live loop can't silently rot the way
# the old "point ENGRAM_VZ_ROOTFS at a stale bake" flow did.
# ENGRAM_VZ_REQUIRE=1 turns any leftover preflight SKIP into a failure.
vz-e2e: pull-kernel vz-test-bundles vz-codesign
    bash crates/engram-sandbox-vz/scripts/make-test-rootfs.sh var/vz-e2e/rootfs.ext4
    ENGRAM_VZ_ROOTFS="$PWD/var/vz-e2e/rootfs.ext4" \
    ENGRAM_VZ_BUNDLE_DIR="$PWD/var/vz-e2e/shared" \
    ENGRAM_VZ_REQUIRE=1 \
        cargo nextest run -p engram-sandbox-vz --run-ignored ignored-only -E 'test(e2e_vz)'

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
# Deterministic simulation (ADR 0098). One seed replays one exact
# interleaving; the swarm explores many. A failure prints the seed +
# trace tail — replay it with `just sim SEED=<n>`.
# ------------------------------------------------------------------

# Replay a single seed (default profile: chaos).
sim SEED STEPS='1500' PROFILE='chaos':
    cargo run -p engram-dst --release --bin sim -- --seed {{SEED}} --steps {{STEPS}} --profile {{PROFILE}}

# Seeded swarm over a range (`just sim-swarm 0..500`).
sim-swarm SEEDS='0..200' STEPS='1500' PROFILE='chaos':
    cargo run -p engram-dst --release --bin sim -- --seeds {{SEEDS}} --steps {{STEPS}} --profile {{PROFILE}}

# Host-internal simulator (ADR 0098 Phase 2): replay a single seed against
# the acked-write durability oracle over the portable disk/spool machinery.
sim-host SEED STEPS='400' PROFILE='chaos':
    cargo run -p engram-dst-host --release --bin sim-host -- --seed {{SEED}} --steps {{STEPS}} --profile {{PROFILE}}

# Host-internal seeded swarm over a range (`just sim-host-swarm 0..50`).
sim-host-swarm SEEDS='0..50' STEPS='400' PROFILE='chaos':
    cargo run -p engram-dst-host --release --bin sim-host -- --seeds {{SEEDS}} --steps {{STEPS}} --profile {{PROFILE}}

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
