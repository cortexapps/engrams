# Shared `engrams` CLI resolution for the deploy/dev scripts. Source me:
#
#   source deploy/dev/engrams-cli.sh   # from the repo root
#
# then call `engrams …`. The CLI drives the ORCHESTRATOR (:8787) — the
# coordinator's app-gRPC is internal (orchestrator↔coord only).
#
#   ENGRAMS_BIN      — path to a compiled binary (CI downloads one); unset →
#                      `bun cli/src/main.ts` from source (one-time bun install).
#   ENGRAMS_URL      — orchestrator base URL (default the `just dev` stack).
#   ENGRAMS_API_KEY  — admin credential; defaults to var/dev-api-key, which
#                      the Tilt `dev-api-key` resource seeds at stack-up.

export ENGRAMS_URL="${ENGRAMS_URL:-http://127.0.0.1:8787}"
export ENGRAMS_API_KEY="${ENGRAMS_API_KEY:-$(cat var/dev-api-key 2>/dev/null || true)}"
if [ -z "$ENGRAMS_API_KEY" ]; then
    echo "engrams-cli.sh: no ENGRAMS_API_KEY and no var/dev-api-key — is the stack up (just dev)?" >&2
    return 1 2>/dev/null || exit 1
fi

if [ -n "${ENGRAMS_BIN:-}" ]; then
    engrams() { "$ENGRAMS_BIN" "$@"; }
else
    (cd cli && bun install --silent)
    engrams() { bun cli/src/main.ts "$@"; }
fi
