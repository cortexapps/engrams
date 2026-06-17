#!/usr/bin/env bash
# CI bring-up: start `tilt up` in the background and block until the
# prod-shape stack is ready, then return (leaving it running for the
# subsequent bake + e2e-test steps). The CI counterpart to `just dev` —
# one orchestrator, the same Tiltfile (ADR 0024). Replaces the retired
# integration-up.sh.
#
# `tilt ci` would tear the coord/host-agent down on exit (they're its
# serve_cmd children), so CI can't use it for "bring up, then test in a
# later step". Instead we background `tilt up --stream` (persists across
# workflow steps, same as the old nohup'd processes) and poll for
# readiness here.
#
# Env (set by the workflow):
#   ENGRAM_INTEG_BIN_DIR     release binaries to exec (no cargo build)
#   ENGRAM_KERNEL_IMAGE_PATH  vmlinux for the FC backend
#   ENGRAM_SKIP_WEB=1         no browser in CI
#   ENGRAM_EGRESS_PROXY_PORT  guests route out via the proxy on Blacksmith

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

mkdir -p var/integration
LOG=var/integration/tilt.log

# (KEK seeding used to live here: the Tiltfile read .env at parse time
# but its bootstrap resource wrote the KEK only at runtime, too late for
# the coord's load-time serve_env. The Tiltfile now runs `just bootstrap`
# at parse time, ahead of the read, so CI no longer needs to pre-seed.)

# Background tilt. `--stream` is the headless, no-TUI log mode. `setsid`
# detaches it into its own session so it outlives this step (the e2e
# test runs in a later step against the still-up stack).
setsid bash -c "exec tilt up --stream >'$LOG' 2>&1" &
disown 2>/dev/null || true
echo "==> tilt up started (log: $LOG)"

# Wait for the coordinator HTTP API.
echo "==> waiting for coord /healthz"
for _ in $(seq 1 180); do
    curl -fsS http://127.0.0.1:8090/healthz >/dev/null 2>&1 && break
    sleep 1
done
if ! curl -fsS http://127.0.0.1:8090/healthz >/dev/null 2>&1; then
    echo "ERROR: coord never came up" >&2
    tail -80 "$LOG" >&2 || true
    exit 1
fi
echo "    coord up"

# Wait for the host-agent(s) to register. The two-host topology
# (`ENGRAM_INTEG_TWO_HOSTS=1`, used by the teleport e2e) brings up
# host-agent + host-agent-b, so require both before returning — else the
# bake/teleport step races a half-up fleet. Single-host default needs 1.
#
# `{ grep || true; }` scopes grep's no-match exit-1 so `set -o pipefail`
# + `set -e` don't abort the script on the first poll (before any host
# has registered, /api/hosts is `{"hosts":[]}` and grep matches nothing).
# Same guard integration-up.sh used; omitting it kills the loop instantly.
case "${ENGRAM_INTEG_TWO_HOSTS:-}" in
    1 | true | yes) want_hosts=2 ;;
    *) want_hosts=1 ;;
esac
# ADR 0051: the web-facing REST `/api/v1/hosts` list is gone — query host
# registration over the coordinator's app-gRPC via engram-cli (the surface the
# whole stack uses now). Endpoint + bearer default to the Tiltfile's coord
# app-gRPC; the workflow may override via ENGRAM_APP_GRPC_ADDR/TOKENS.
ENGRAM_CLI="${ENGRAM_INTEG_BIN_DIR:-./target/release}/engram-cli"
export ENGRAM_APP_GRPC_ADDR="${ENGRAM_APP_GRPC_ADDR:-http://127.0.0.1:50061}"
export ENGRAM_APP_GRPC_TOKENS="${ENGRAM_APP_GRPC_TOKENS:-dev-app-grpc-token}"
host_count() {
    # `--json hosts list` => {"hosts":[{"hostname":...},...]}; count via the
    # same `"hostname"` marker the old REST poll used. Trailing `|| true` so a
    # transient gRPC error (app-gRPC still warming) yields 0, not a `set -e`
    # abort under pipefail.
    "$ENGRAM_CLI" --json hosts list 2>/dev/null \
        | { grep -o '"hostname"' || true; } | wc -l | tr -d ' ' || true
}

echo "==> waiting for host registration (want >= $want_hosts)"
for _ in $(seq 1 180); do
    n=$(host_count)
    [ "${n:-0}" -ge "$want_hosts" ] && break
    sleep 1
done
n=$(host_count)
if [ "${n:-0}" -lt "$want_hosts" ]; then
    echo "ERROR: expected >= $want_hosts host-agent(s), got ${n:-0}" >&2
    tail -80 "$LOG" >&2 || true
    exit 1
fi
echo "    $n host(s) registered; prod-shape stack ready"
