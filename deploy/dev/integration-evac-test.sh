#!/usr/bin/env bash
# ADR 0018 M4: end-to-end evacuation test against a 2-host integration
# stack. Validates the load-bearing user-visible promise of M4:
# "session survives a host loss without manual intervention."
#
# Preconditions:
#   - `ENGRAM_INTEG_TWO_HOSTS=1 just dev` is running (adds host-agent-b).
#   - A baked demo image exists (ghcr.io/cortex/demo:warm-1 or the
#     equivalent ENGRAM_E2E_IMAGE_URI).
#
# What it does:
#   1. Create a session on the cluster (lands on whichever host the
#      scheduler picks).
#   2. Write a deterministic payload to /var/evac-canary.bin inside
#      the guest + capture its md5 BEFORE evac.
#   3. POST /api/admin/sessions/:id/evacuate. The coord picks the
#      other host as target (exclude_host filters the source) and
#      drives the full evacuate_to → finish_resume_to_active path.
#   4. Verify the session ended at Active on a DIFFERENT host_id.
#   5. Read /var/evac-canary.bin on the new host, compare md5.
#      Match = disk preserved across the relocate. Mismatch = the
#      live_disk_manifest pipeline regressed somewhere.
#
# This is the gold-standard test for "M4 actually works." Run on
# dev-vm after every change to the evac primitives or
# finish_resume_to_active.
#
# NOTE: the canonical, CI-gated version of this test is the Rust e2e
# `e2e_two_host_evacuate_preserves_sentinel` in
# crates/engram-coordinator/tests/e2e_stack.rs — it drives the same
# `FleetService.EvacuateSession` RPC and asserts byte-identical disk
# across the relocate. This shell script is the manual dev tool; the
# `engrams host evacuate` verb (FleetService passthrough) closed the old
# "no CLI surface for evacuate" gap, so it now runs end to end.

set -euo pipefail

COORD_URL="${ENGRAM_COORD_URL:-http://127.0.0.1:8090}"
IMAGE_URI="${ENGRAM_E2E_IMAGE_URI:-ghcr.io/cortex/demo:warm-1}"

# All verbs go through the `engrams` CLI → the orchestrator's Connect
# surface. Endpoint + admin key default to the `just dev` stack (Tilt
# seeds var/dev-api-key). (The coord `/healthz` check below is a KEPT
# internal REST route, left as-is.) The helper resolves cli/ +
# var/dev-api-key relative to the repo root.
cd "$(git rev-parse --show-toplevel)"
source deploy/dev/engrams-cli.sh

note() { printf '\033[1;36m==> %s\033[0m\n' "$*"; }
ok()   { printf '\033[1;32m✓ %s\033[0m\n' "$*"; }
die()  { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

# --- step 1: precondition checks --------------------------------------
note "checking coord reachability"
curl -fsS "$COORD_URL/healthz" >/dev/null || die "coord unreachable at $COORD_URL"

note "checking host count (expect ≥ 2 for evac to actually relocate)"
HOST_COUNT=$(engrams --json hosts list \
    | grep -o '"hostname"' | wc -l | tr -d ' ')
if [ "$HOST_COUNT" -lt 2 ]; then
    die "only $HOST_COUNT host(s) registered. Restart with ENGRAM_INTEG_TWO_HOSTS=1 just dev."
fi
ok "$HOST_COUNT hosts registered"

# --- step 2: create session ------------------------------------------
# ADR 0051 + ADR 0021 P1.3: harness is an image property, not a per-session
# choice. `--dev-vm` boots the image with its baked harness undriven (the
# app-gRPC analog of the old `harness {kind:none}`).
note "creating session against $IMAGE_URI"
SESSION_ID=$(engrams session create --image "$IMAGE_URI" --dev-vm)
[ -n "$SESSION_ID" ] || die "couldn't create session"
ok "session $SESSION_ID created"

# Reap the test session on any exit (success, die, or Ctrl-C) so repeat
# runs don't stack orphaned sessions.
cleanup() { engrams session delete "$SESSION_ID" >/dev/null 2>&1 || true; }
trap cleanup EXIT

# Pause to let scheduling settle.
sleep 2

note "fetching pre-evac session row"
SESSION_BEFORE=$(engrams --json session get "$SESSION_ID")
HOST_BEFORE=$(echo "$SESSION_BEFORE" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("host_id") or "")')
SANDBOX_BEFORE=$(echo "$SESSION_BEFORE" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("sandbox_id") or "")')
STATUS_BEFORE=$(echo "$SESSION_BEFORE" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("status") or "")')
[ "$STATUS_BEFORE" = "active" ] || die "session not Active before evac (got $STATUS_BEFORE)"
ok "pre-evac: host=$HOST_BEFORE sandbox=$SANDBOX_BEFORE status=$STATUS_BEFORE"

# --- step 3: write canary to disk + flush ---------------------------
# `session exec <id> <cmd>` runs the string under `sh -c` and streams
# stdout (exiting non-zero if the remote command does).
note "writing canary file inside guest"
EXEC_PAYLOAD='dd if=/dev/urandom of=/var/evac-canary.bin bs=64K count=4 status=none && sync && md5sum /var/evac-canary.bin'
EXEC_RESP=$(engrams session exec "$SESSION_ID" "$EXEC_PAYLOAD")
MD5_BEFORE=$(echo "$EXEC_RESP" | grep -o '[0-9a-f]\{32\}' | head -1)
[ -n "$MD5_BEFORE" ] || die "couldn't parse md5 from exec output: $EXEC_RESP"
ok "canary md5 before: $MD5_BEFORE"

note "forcing flush so live_disk_manifest is published before evac"
engrams admin flush "$SESSION_ID" >/dev/null || die "admin flush failed"

# --- step 4: evacuate (async shape, ADR 0018 commit 12) -------------
note "evacuating session $SESSION_ID"
engrams host evacuate "$SESSION_ID" >/dev/null || die "evacuate RPC failed"

# --- step 5: wait for the relocate (Evacuating -> Active on a peer) --
note "waiting for the session to land on a peer host"
DEADLINE=$(( $(date +%s) + 120 ))
while :; do
    ROW=$(engrams --json session get "$SESSION_ID")
    STATUS=$(echo "$ROW" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("status") or "")')
    HOST_AFTER=$(echo "$ROW" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("host_id") or "")')
    if [ "$STATUS" = "active" ] && [ -n "$HOST_AFTER" ] && [ "$HOST_AFTER" != "$HOST_BEFORE" ]; then
        break
    fi
    [ "$(date +%s)" -lt "$DEADLINE" ] || die "session did not relocate within 120s (status=$STATUS host=$HOST_AFTER)"
    sleep 2
done
ok "post-evac: host=$HOST_AFTER (was $HOST_BEFORE) status=$STATUS"

# --- step 6: canary survives byte-identical ---------------------------
note "verifying canary md5 after relocate"
EXEC_RESP=$(engrams session exec "$SESSION_ID" 'md5sum /var/evac-canary.bin')
MD5_AFTER=$(echo "$EXEC_RESP" | grep -o '[0-9a-f]\{32\}' | head -1)
[ "$MD5_AFTER" = "$MD5_BEFORE" ] || die "canary changed across evac: $MD5_BEFORE -> $MD5_AFTER"
ok "canary intact across evacuation ($MD5_AFTER)"
echo ""
ok "evacuation smoke passed"
