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
# ADR 0051 NOTE: the canonical, CI-gated version of this test is now the
# Rust e2e `e2e_two_host_evacuate_preserves_sentinel` in
# crates/engram-coordinator/tests/e2e_stack.rs — it drives the same
# `FleetService.EvacuateSession` app-gRPC RPC and asserts byte-identical
# disk across the relocate. This shell script is kept as a manual dev
# tool. Its setup/teardown is migrated to engram-cli (app-gRPC); the
# evacuate step itself has NO engram-cli surface (see step 4) — there is
# no `evacuate` subcommand, only the FleetService.EvacuateSession RPC the
# Rust e2e exercises directly. Run the Rust e2e for full coverage.

set -euo pipefail

COORD_URL="${ENGRAM_COORD_URL:-http://127.0.0.1:8090}"
IMAGE_URI="${ENGRAM_E2E_IMAGE_URI:-ghcr.io/cortex/demo:warm-1}"

# ADR 0051 Drip E: coord interaction goes over app-gRPC via engram-cli.
# Endpoint + bearer default to the Tiltfile's coord app-gRPC. (The
# `/healthz` check below is a KEPT REST route, left as-is.)
ENGRAM_CLI="${ENGRAM_INTEG_BIN_DIR:-./target/release}/engram-cli"
export ENGRAM_APP_GRPC_ADDR="${ENGRAM_APP_GRPC_ADDR:-http://127.0.0.1:50061}"
export ENGRAM_APP_GRPC_TOKENS="${ENGRAM_APP_GRPC_TOKENS:-dev-app-grpc-token}"

note() { printf '\033[1;36m==> %s\033[0m\n' "$*"; }
ok()   { printf '\033[1;32m✓ %s\033[0m\n' "$*"; }
die()  { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

# --- step 1: precondition checks --------------------------------------
note "checking coord reachability"
curl -fsS "$COORD_URL/healthz" >/dev/null || die "coord unreachable at $COORD_URL"

note "checking host count (expect ≥ 2 for evac to actually relocate)"
HOST_COUNT=$("$ENGRAM_CLI" --json hosts list \
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
SESSION_ID=$("$ENGRAM_CLI" session create --image "$IMAGE_URI" --dev-vm)
[ -n "$SESSION_ID" ] || die "couldn't create session"
ok "session $SESSION_ID created"

# Reap the test session on any exit (success, die, or Ctrl-C) so repeat
# runs don't stack orphaned sessions.
cleanup() { "$ENGRAM_CLI" session delete "$SESSION_ID" >/dev/null 2>&1 || true; }
trap cleanup EXIT

# Pause to let scheduling settle.
sleep 2

note "fetching pre-evac session row"
SESSION_BEFORE=$("$ENGRAM_CLI" --json session get "$SESSION_ID")
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
EXEC_RESP=$("$ENGRAM_CLI" session exec "$SESSION_ID" "$EXEC_PAYLOAD")
MD5_BEFORE=$(echo "$EXEC_RESP" | grep -o '[0-9a-f]\{32\}' | head -1)
[ -n "$MD5_BEFORE" ] || die "couldn't parse md5 from exec output: $EXEC_RESP"
ok "canary md5 before: $MD5_BEFORE"

note "forcing flush so live_disk_manifest is published before evac"
"$ENGRAM_CLI" admin flush "$SESSION_ID" >/dev/null || die "admin flush failed"

# --- step 4: evacuate (async shape, ADR 0018 commit 12) -------------
# ADR 0051: there is NO engram-cli / app-gRPC-CLI surface for evacuate.
# The web-facing REST `/api/admin/sessions/:id/evacuate` was removed, and
# while `FleetService.EvacuateSession` exists on the app-gRPC surface, no
# `engram-cli` subcommand wraps it. The CI-gated Rust e2e
# `e2e_two_host_evacuate_preserves_sentinel` drives that RPC directly and
# is the source of truth for this coverage. This manual shell tool cannot
# perform the evacuate, so it stops here rather than silently passing.
die "no engram-cli surface for EvacuateSession — run the Rust e2e instead:
       cargo nextest run -p engram-coordinator --test e2e_stack \\
           --run-ignored ignored-only -E 'test(e2e_two_host_evacuate_preserves_sentinel)'
     (setup above — host count, session create, canary write, flush — is
      migrated to app-gRPC and verified; only the evacuate trigger lacks a
      CLI wrapper. The EXIT trap reaps the test session.)"
