#!/usr/bin/env bash
# Tear down the integration stack started by `integration-up.sh`.
# Kills the background coord + host-agent processes, then stops
# the docker compose services. Volumes (postgres data, fake-gcs
# bucket contents) stay intact so re-running picks up state.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
INTEG_DIR="./var/integration"

kill_by_pidfile() {
    local pidfile="$1"
    local label="$2"
    if [ ! -f "$pidfile" ]; then
        echo "==> no $label PID file ($pidfile); skipping"
        return
    fi
    local pid
    pid=$(cat "$pidfile" 2>/dev/null || true)
    if [ -z "$pid" ]; then
        return
    fi
    if kill -0 "$pid" 2>/dev/null; then
        echo "==> stopping $label (PID $pid)"
        kill "$pid" 2>/dev/null || true
        # Give it a moment to drain.
        for _ in $(seq 1 10); do
            if ! kill -0 "$pid" 2>/dev/null; then
                break
            fi
            sleep 0.5
        done
        if kill -0 "$pid" 2>/dev/null; then
            echo "   $label didn't exit on SIGTERM; SIGKILL"
            kill -9 "$pid" 2>/dev/null || true
        fi
    else
        echo "==> $label PID $pid already gone"
    fi
    rm -f "$pidfile"
}

# host-agent first so it gets a chance to deregister with the coord.
kill_by_pidfile "$INTEG_DIR/host-agent.pid" "host-agent"
kill_by_pidfile "$INTEG_DIR/coord.pid" "coordinator"

echo "==> docker compose down (volumes preserved)"
docker compose -f deploy/docker-compose.dev.yml down

echo "✓ integration stack down"
