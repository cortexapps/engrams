#!/usr/bin/env bash
# Forward the dev-vm's `just dev` ports to localhost on this Mac.
#
# Runs gcloud IAP tunnels for:
#   • 5173  — web SPA (vite, http://localhost:5173)
#   • 8090  — coordinator HTTP API + WS
#   • 5001  — local OCI registry
#   • 10350 — Tilt UI (http://localhost:10350)
#
# Foreground; Ctrl-C tears down all tunnels. Re-run after starting
# (or restarting) `just dev` on the dev-vm.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/config.sh"

pids=()
cleanup() {
    for p in "${pids[@]}"; do
        kill "$p" 2>/dev/null || true
    done
}
trap cleanup EXIT INT TERM

start_tunnel() {
    local port="$1"
    local label="$2"
    echo "  $label: http://localhost:$port (-> dev-vm:$port)"
    gcloud compute start-iap-tunnel "$GCP_INSTANCE" "$port" \
        --zone "$GCP_ZONE" \
        --project "$GCP_PROJECT" \
        --local-host-port="localhost:$port" \
        >/dev/null 2>&1 &
    pids+=("$!")
}

echo "starting IAP tunnels to $GCP_INSTANCE — Ctrl-C to tear down"
start_tunnel 5173 "web"
start_tunnel 8090 "coord"
start_tunnel 5001 "registry"
start_tunnel 10350 "tilt"
echo ""
echo "ready — open http://localhost:5173 in a browser"
wait
