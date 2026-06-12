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

# Wait for the coordinator HTTP API. /healthz is a retained HTTP route
# (ADR 0039 Task 32) so this curl is correct here.
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

# Wait for at least one host-agent to register, via the app gRPC surface
# (ADR 0039). The engram-cli binary is available — it was built in a
# prior CI step (ENGRAM_INTEG_BIN_DIR or ./target/release).
CLI="${ENGRAM_INTEG_BIN_DIR:-./target/release}/engram-cli"
GRPC="${ENGRAM_APP_GRPC:-http://127.0.0.1:50061}"
# Fail-closed app surface (ADR 0039 Task 9): a bearer is always required.
# Default to the Tiltfile's dev literal (CI's coordinator gets the same
# value via the Tiltfile env_or default) — without this the registration
# poll dies silently unauthenticated.
ENGRAM_APP_TOKEN="${ENGRAM_APP_TOKEN:-dev-app-grpc-token}"
TOKEN_FLAG=(--token "$ENGRAM_APP_TOKEN")

echo "==> waiting for host registration"
for _ in $(seq 1 180); do
    n=$("$CLI" ${TOKEN_FLAG[@]+"${TOKEN_FLAG[@]}"} --grpc-addr "$GRPC" --json host list 2>/dev/null \
        | python3 -c "import sys,json; d=json.load(sys.stdin); print(len(d.get('hosts',[])))" 2>/dev/null || echo 0)
    [ "${n:-0}" -ge 1 ] && break
    sleep 1
done
n=$("$CLI" ${TOKEN_FLAG[@]+"${TOKEN_FLAG[@]}"} --grpc-addr "$GRPC" --json host list 2>/dev/null \
    | python3 -c "import sys,json; d=json.load(sys.stdin); print(len(d.get('hosts',[])))" 2>/dev/null || echo 0)
if [ "${n:-0}" -lt 1 ]; then
    echo "ERROR: no host-agent registered" >&2
    tail -80 "$LOG" >&2 || true
    exit 1
fi
echo "    host registered; prod-shape stack ready"
