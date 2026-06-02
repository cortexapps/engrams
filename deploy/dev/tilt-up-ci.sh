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

# The Tiltfile reads .env at parse time (for ENGRAM_KEK_MASTER_KEY etc.),
# but its `bootstrap` resource only writes the KEK at runtime — too late
# for the coord's serve_env, which is captured at load. Seed it here so a
# fresh CI runner's coordinator gets a real KEK. Idempotent.
if ! grep -q '^ENGRAM_KEK_MASTER_KEY=' .env 2>/dev/null; then
    printf 'ENGRAM_KEK_MASTER_KEY=%s\n' "$(head -c 32 /dev/urandom | base64)" >>.env
fi

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

# Wait for at least one host-agent to register (FC backend always runs
# the split topology, so a host-agent is expected).
#
# `{ grep || true; }` scopes grep's no-match exit-1 so `set -o pipefail`
# + `set -e` don't abort the script on the first poll (before any host
# has registered, /api/hosts is `{"hosts":[]}` and grep matches nothing).
# Same guard integration-up.sh used; omitting it kills the loop instantly.
echo "==> waiting for host registration"
for _ in $(seq 1 180); do
    n=$(curl -fsS http://127.0.0.1:8090/api/v1/hosts 2>/dev/null \
        | { grep -o '"hostname"' || true; } | wc -l | tr -d ' ')
    [ "${n:-0}" -ge 1 ] && break
    sleep 1
done
n=$(curl -fsS http://127.0.0.1:8090/api/v1/hosts 2>/dev/null \
    | { grep -o '"hostname"' || true; } | wc -l | tr -d ' ')
if [ "${n:-0}" -lt 1 ]; then
    echo "ERROR: no host-agent registered" >&2
    tail -80 "$LOG" >&2 || true
    exit 1
fi
echo "    host registered; prod-shape stack ready"
