#!/usr/bin/env bash
# Bring up the prod-shape integration stack on the dev VM.
#
# Layout:
#   docker compose: postgres, fake-gcs-server, registry
#   one-shots:       KEK bootstrap, GCS bucket seed
#   background:      coord in `mode=coordinator`, host-agent
#                    dialing the coord
#
# Logs land in ./var/integration/{coord,host-agent}.log. PIDs
# land in ./var/integration/{coord,host-agent}.pid.
#
# `just integration-down` reads those PID files and tears
# everything down.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

KERNEL_DEFAULT="$HOME/.cache/engram-fc-test/vmlinux-5.10.223"
KERNEL="${ENGRAM_KERNEL_IMAGE_PATH:-$KERNEL_DEFAULT}"

if [ ! -e "$KERNEL" ]; then
    echo "ERROR: FC kernel not found at $KERNEL" >&2
    echo "       Run bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh" >&2
    echo "       or set ENGRAM_KERNEL_IMAGE_PATH to a vmlinux you have." >&2
    exit 1
fi

if ! command -v firecracker >/dev/null; then
    echo "ERROR: firecracker not on PATH." >&2
    echo "       In nix develop the FC binary should be available;" >&2
    echo "       outside of nix install with the same v1.10.1 the test suite uses." >&2
    exit 1
fi

INTEG_DIR="./var/integration"
mkdir -p "$INTEG_DIR"

echo "==> docker compose: postgres + fake-gcs-server + registry"
docker compose -f deploy/docker-compose.dev.yml up -d postgres fake-gcs-server registry

echo "==> wait for postgres health"
for _ in $(seq 1 30); do
    if docker compose -f deploy/docker-compose.dev.yml \
        exec -T postgres pg_isready -U engram -d engram >/dev/null 2>&1; then
        break
    fi
    sleep 1
done

echo "==> wait for fake-gcs-server"
for _ in $(seq 1 30); do
    if curl -fsS http://localhost:4443/storage/v1/b >/dev/null 2>&1; then
        break
    fi
    sleep 1
done

echo "==> KEK bootstrap"
just bootstrap

echo "==> seed GCS bucket"
bash deploy/dev/seed-buckets.sh

echo "==> build coordinator + host-agent"
cargo build -p engram-coordinator -p engram-host-agent

# ADR 0014: host-agent provisions per-VM TAPs (via the `ip` shell-out)
# and writes iptables rules. CAP_NET_ADMIN on the host-agent binary
# alone isn't enough — `ip` is a subprocess and the cap doesn't
# propagate without ambient bits. Run host-agent under `sudo -E` on
# the dev rig (prod uses systemd's AmbientCapabilities). User must
# have NOPASSWD sudo, or this will block.
SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    SUDO="sudo -n --preserve-env=PATH,RUST_LOG,DATABASE_URL,ENGRAM_KERNEL_IMAGE_PATH,ENGRAM_SANDBOX_WORK_DIR,ENGRAM_SANDBOX_BACKEND,ENGRAM_GRPC_LISTEN_ADDR,ENGRAM_GRPC_ADVERTISE_ADDR,ENGRAM_COORDINATOR_ENDPOINT,ENGRAM_BLOB_BACKEND,ENGRAM_GCS_BUCKET,STORAGE_EMULATOR_HOST,ENGRAM_EGRESS_PROXY_PORT,ENGRAM_KEK_MASTER_KEY"
    if ! sudo -n true 2>/dev/null; then
        echo "ERROR: passwordless sudo required for host-agent TAP creation." >&2
        echo "       Add an entry to /etc/sudoers.d/ allowing this user NOPASSWD." >&2
        exit 1
    fi
fi

# Source .env so KEK + any operator overrides land in the
# coord/host-agent's process env. `just bootstrap` writes a fresh
# `ENGRAM_KEK_MASTER_KEY=...` line into .env on first run.
if [ -f .env ]; then
    set -a
    # shellcheck disable=SC1091
    . ./.env
    set +a
fi

echo "==> start coordinator (mode=coordinator)"
DATABASE_URL="postgres://engram:engram@localhost:5435/engram" \
ENGRAM_BIND_ADDR="127.0.0.1:8090" \
ENGRAM_MODE="coordinator" \
ENGRAM_LOCAL_PATH="./var/engram-integration" \
ENGRAM_BLOB_BACKEND="gcs" \
ENGRAM_GCS_BUCKET="${ENGRAM_GCS_BUCKET:-engram-snapshots-test}" \
STORAGE_EMULATOR_HOST="http://localhost:4443" \
RUST_LOG="${RUST_LOG:-info,engram=debug,engram_coordinator::api::enabled_images=trace}" \
nohup ./target/debug/engram-coordinator >"$INTEG_DIR/coord.log" 2>&1 &
echo $! > "$INTEG_DIR/coord.pid"

echo "==> wait for coord /healthz"
for _ in $(seq 1 30); do
    if curl -fsS http://127.0.0.1:8090/healthz >/dev/null 2>&1; then
        break
    fi
    sleep 1
done

echo "==> start host-agent (dial 127.0.0.1:8090)"
ENGRAM_KERNEL_IMAGE_PATH="$KERNEL" \
ENGRAM_SANDBOX_WORK_DIR="./var/host-sandboxes-integration" \
ENGRAM_SANDBOX_BACKEND="firecracker" \
ENGRAM_GRPC_LISTEN_ADDR="127.0.0.1:9101" \
ENGRAM_GRPC_ADVERTISE_ADDR="http://127.0.0.1:9101" \
ENGRAM_COORDINATOR_ENDPOINT="http://127.0.0.1:8090" \
ENGRAM_BLOB_BACKEND="gcs" \
ENGRAM_GCS_BUCKET="${ENGRAM_GCS_BUCKET:-engram-snapshots-test}" \
STORAGE_EMULATOR_HOST="http://localhost:4443" \
ENGRAM_EGRESS_PROXY_PORT="0" \
RUST_LOG="${RUST_LOG:-info,engram=debug,engram_host_agent::warm_pool=debug,engram_host_agent::pooled_backend=debug}" \
nohup $SUDO ./target/debug/engram-host-agent >"$INTEG_DIR/host-agent.log" 2>&1 &
echo $! > "$INTEG_DIR/host-agent.pid"

echo "==> wait for host registration"
for _ in $(seq 1 30); do
    if curl -fsS http://127.0.0.1:8090/api/hosts 2>/dev/null \
        | grep -q '"hostname"'; then
        break
    fi
    sleep 1
done

echo ""
echo "✓ integration stack up"
echo "  coord:      http://127.0.0.1:8090  (PID $(cat $INTEG_DIR/coord.pid))"
echo "  host-agent: 127.0.0.1:9101         (PID $(cat $INTEG_DIR/host-agent.pid))"
echo "  registry:   http://localhost:5001"
echo "  fake-gcs:   http://localhost:4443"
echo ""
echo "  logs:     $INTEG_DIR/coord.log, $INTEG_DIR/host-agent.log"
echo "  smoke:    just integration-test"
echo "  teardown: just integration-down"
