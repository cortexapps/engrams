#!/usr/bin/env bash
# Engram graceful-drain hook.
#
# Triggered by systemd `ExecStop=` on the engram-host-agent unit, so
# fires on shutdown (rolling MIG update, preemption notice, manual
# `systemctl stop`, instance delete). Tells the coord to migrate
# active sessions off this host before the agent dies.
#
# The coord's `POST /api/hosts/$HOST_ID/drain?migrate_active=true`
# endpoint does the work: marks the host `draining`, prevents new
# session assignments, kicks off per-session migration (pause →
# snapshot → restore on a sibling host). With chunked storage in
# place (ADR 0007), the migration is sub-2s per session because the
# disk + memory state already lives in BlobStorage; the new host
# materializes from chunks.
#
# Falls back to plain agent shutdown when the coord URL or host
# ID isn't known — e.g. on a host that hasn't registered yet, or
# during the boot window before /etc/engram/host-agent.env exists.

set -uo pipefail

ENV_FILE="${ENV_FILE:-/etc/engram/host-agent.env}"
DEADLINE_SECS="${DEADLINE_SECS:-240}"

if [ -f "$ENV_FILE" ]; then
    # shellcheck disable=SC1090
    . "$ENV_FILE"
fi

if [ -z "${ENGRAM_COORDINATOR_ENDPOINT:-}" ]; then
    echo "engram-drain: ENGRAM_COORDINATOR_ENDPOINT unset; nothing to drain. Letting the agent stop normally."
    exit 0
fi

# Host id: the agent generates one per-process today. For drain we
# need the same id the coord sees on the dialer. The agent writes it
# to /run/engram/host-id on connect; missing means we never
# registered (boot failure, no coord), so drain is a no-op.
HOST_ID_FILE="${HOST_ID_FILE:-/run/engram/host-id}"
if [ ! -f "$HOST_ID_FILE" ]; then
    echo "engram-drain: no $HOST_ID_FILE; host never registered. Skipping coord drain."
    exit 0
fi
HOST_ID="$(cat "$HOST_ID_FILE")"

# Drop trailing slash so URL joining is idempotent.
COORD="${ENGRAM_COORDINATOR_ENDPOINT%/}"
# Translate ws:// → http:// if needed — the drain endpoint is HTTPS.
case "$COORD" in
    ws://*)  COORD="http://${COORD#ws://}" ;;
    wss://*) COORD="https://${COORD#wss://}" ;;
esac

URL="${COORD}/api/hosts/${HOST_ID}/drain?migrate_active=true&deadline_secs=${DEADLINE_SECS}"
echo "engram-drain: POST $URL"

AUTH_HEADER=()
if [ -n "${ENGRAM_COORDINATOR_TOKEN:-}" ]; then
    AUTH_HEADER=(-H "Authorization: Bearer ${ENGRAM_COORDINATOR_TOKEN}")
fi

# Best-effort. systemd's TimeoutStopSec= caps us at 5min anyway.
curl -sS -X POST -m "$DEADLINE_SECS" "${AUTH_HEADER[@]}" "$URL" || {
    echo "engram-drain: coord call failed; proceeding to local shutdown" >&2
}

echo "engram-drain: done"
