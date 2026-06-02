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

set -euo pipefail

COORD_URL="${ENGRAM_COORD_URL:-http://127.0.0.1:8090}"
IMAGE_URI="${ENGRAM_E2E_IMAGE_URI:-ghcr.io/cortex/demo:warm-1}"

note() { printf '\033[1;36m==> %s\033[0m\n' "$*"; }
ok()   { printf '\033[1;32m✓ %s\033[0m\n' "$*"; }
die()  { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

# --- step 1: precondition checks --------------------------------------
note "checking coord reachability"
curl -fsS "$COORD_URL/healthz" >/dev/null || die "coord unreachable at $COORD_URL"

note "checking host count (expect ≥ 2 for evac to actually relocate)"
HOSTS_JSON=$(curl -fsS "$COORD_URL/api/v1/hosts")
HOST_COUNT=$(echo "$HOSTS_JSON" | grep -o '"hostname"' | wc -l | tr -d ' ')
if [ "$HOST_COUNT" -lt 2 ]; then
    die "only $HOST_COUNT host(s) registered. Restart with ENGRAM_INTEG_TWO_HOSTS=1 just dev."
fi
ok "$HOST_COUNT hosts registered"

# --- step 2: create session ------------------------------------------
note "creating session against $IMAGE_URI"
CREATE_BODY=$(cat <<JSON
{
  "image": "$IMAGE_URI",
  "harness": {"kind": "none"}
}
JSON
)
CREATE_RESP=$(curl -fsS -X POST -H 'content-type: application/json' \
    -d "$CREATE_BODY" "$COORD_URL/api/v1/sessions")
SESSION_ID=$(echo "$CREATE_RESP" | grep -o '"session_id":"[^"]*"' | cut -d'"' -f4)
[ -n "$SESSION_ID" ] || die "couldn't parse session_id from create response: $CREATE_RESP"
ok "session $SESSION_ID created"

# Pause to let scheduling settle.
sleep 2

note "fetching pre-evac session row"
SESSION_BEFORE=$(curl -fsS "$COORD_URL/api/v1/sessions/$SESSION_ID")
HOST_BEFORE=$(echo "$SESSION_BEFORE" | grep -o '"host_id":"[^"]*"' | head -1 | cut -d'"' -f4)
SANDBOX_BEFORE=$(echo "$SESSION_BEFORE" | grep -o '"sandbox_id":"[^"]*"' | head -1 | cut -d'"' -f4)
STATUS_BEFORE=$(echo "$SESSION_BEFORE" | grep -o '"status":"[^"]*"' | head -1 | cut -d'"' -f4)
[ "$STATUS_BEFORE" = "active" ] || die "session not Active before evac (got $STATUS_BEFORE)"
ok "pre-evac: host=$HOST_BEFORE sandbox=$SANDBOX_BEFORE status=$STATUS_BEFORE"

# --- step 3: write canary to disk + flush ---------------------------
note "writing canary file inside guest"
EXEC_PAYLOAD='dd if=/dev/urandom of=/var/evac-canary.bin bs=64K count=4 status=none && sync && md5sum /var/evac-canary.bin'
EXEC_RESP=$(curl -fsS -X POST -H 'content-type: application/json' \
    -d "{\"argv\":[\"sh\",\"-c\",\"$EXEC_PAYLOAD\"]}" \
    "$COORD_URL/api/v1/sessions/$SESSION_ID/exec")
MD5_BEFORE=$(echo "$EXEC_RESP" | grep -o '[0-9a-f]\{32\}' | head -1)
[ -n "$MD5_BEFORE" ] || die "couldn't parse md5 from exec output: $EXEC_RESP"
ok "canary md5 before: $MD5_BEFORE"

note "forcing flush so live_disk_manifest is published before evac"
curl -fsS -X POST -H 'authorization: Bearer dev' "$COORD_URL/api/v1/admin/sessions/$SESSION_ID/flush-now" \
    -d '{}' -H 'content-type: application/json' >/dev/null || die "flush-now failed"

# --- step 4: evacuate (async shape, ADR 0018 commit 12) -------------
note "triggering admin evacuate (async — handler returns 202; scanner resumes on peer)"
HTTP_RESP=$(curl -fsS -X POST -H 'authorization: Bearer dev' \
    -H 'content-type: application/json' -d '{}' \
    -w '\n%{http_code}' \
    "$COORD_URL/api/v1/admin/sessions/$SESSION_ID/evacuate")
HTTP_CODE=$(echo "$HTTP_RESP" | tail -1)
EVAC_BODY=$(echo "$HTTP_RESP" | head -n -1)
[ "$HTTP_CODE" = "202" ] || die "expected 202 Accepted, got $HTTP_CODE body=$EVAC_BODY"
EVAC_STATUS=$(echo "$EVAC_BODY" | grep -o '"status":"[^"]*"' | cut -d'"' -f4)
[ "$EVAC_STATUS" = "evacuating" ] || die "expected status=evacuating, got '$EVAC_STATUS' body=$EVAC_BODY"
ok "evac dispatched: scanner will resume on peer"

# --- step 5: verify post-evac state ---------------------------------
# Async shape: poll until session leaves Evacuating and lands at
# Active on a different host. Scanner cadence is ~10s; allow a
# generous 120s budget for slow dev-vm + image-prefetch + harness
# rebuild.
note "polling session state until Active on new host (scanner-driven)"
ACTIVE_AT=""
SAW_EVACUATING=0
for _ in $(seq 1 120); do
    SESSION_AFTER=$(curl -fsS "$COORD_URL/api/v1/sessions/$SESSION_ID")
    STATUS_AFTER=$(echo "$SESSION_AFTER" | grep -o '"status":"[^"]*"' | head -1 | cut -d'"' -f4)
    if [ "$STATUS_AFTER" = "evacuating" ]; then
        SAW_EVACUATING=1
    fi
    if [ "$STATUS_AFTER" = "active" ]; then
        ACTIVE_AT="$SESSION_AFTER"
        break
    fi
    sleep 1
done
[ -n "$ACTIVE_AT" ] || die "session never reached Active after evac"
[ "$SAW_EVACUATING" = "1" ] || ok "(scanner moved through Evacuating faster than the poll cadence — fine)"
HOST_AFTER=$(echo "$ACTIVE_AT" | grep -o '"host_id":"[^"]*"' | head -1 | cut -d'"' -f4)
SANDBOX_AFTER=$(echo "$ACTIVE_AT" | grep -o '"sandbox_id":"[^"]*"' | head -1 | cut -d'"' -f4)
[ "$HOST_AFTER" != "$HOST_BEFORE" ] || die "post-evac host_id=$HOST_AFTER same as source; scanner picked source (exclude_host regressed?)"
[ "$SANDBOX_AFTER" != "$SANDBOX_BEFORE" ] || die "post-evac sandbox_id unchanged; relocate didn't actually mint a new sandbox"
ok "post-evac: host=$HOST_AFTER sandbox=$SANDBOX_AFTER status=active"

# --- step 6: verify disk preservation -------------------------------
note "re-reading canary on the new host"
EXEC_AFTER=$(curl -fsS -X POST -H 'content-type: application/json' \
    -d '{"argv":["sh","-c","md5sum /var/evac-canary.bin"]}' \
    "$COORD_URL/api/v1/sessions/$SESSION_ID/exec")
MD5_AFTER=$(echo "$EXEC_AFTER" | grep -o '[0-9a-f]\{32\}' | head -1)
[ -n "$MD5_AFTER" ] || die "couldn't parse md5 on new host: $EXEC_AFTER"
ok "canary md5 after:  $MD5_AFTER"

if [ "$MD5_BEFORE" = "$MD5_AFTER" ]; then
    ok "DISK PRESERVED across relocate — M4 closes the loop"
else
    die "DISK MISMATCH: live_disk_manifest pipeline regressed (before=$MD5_BEFORE after=$MD5_AFTER)"
fi

# --- cleanup --------------------------------------------------------
note "deleting test session"
curl -fsS -X DELETE "$COORD_URL/api/v1/sessions/$SESSION_ID" >/dev/null || true
ok "done"
