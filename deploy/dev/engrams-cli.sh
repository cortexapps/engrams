# Shared `engrams` CLI resolution for the deploy/dev scripts. Source me:
#
#   source deploy/dev/engrams-cli.sh   # from the repo root
#
# then call `engrams …`. The CLI drives the ORCHESTRATOR (:8787) — the
# coordinator's app-gRPC is internal (orchestrator↔coord only).
#
#   ENGRAMS_BIN      — path to a compiled binary (CI builds one); unset →
#                      `cargo build -p engram-cli` and target/debug/engrams.
#   ENGRAMS_URL      — orchestrator base URL (default the `just dev` stack).
#   ENGRAMS_API_KEY  — admin credential; defaults to var/dev-api-key, which
#                      the Tilt `dev-api-key` resource seeds at stack-up.

export ENGRAMS_URL="${ENGRAMS_URL:-http://127.0.0.1:8787}"
export ENGRAMS_API_KEY="${ENGRAMS_API_KEY:-$(cat var/dev-api-key 2>/dev/null || true)}"
if [ -z "$ENGRAMS_API_KEY" ]; then
    echo "engrams-cli.sh: no ENGRAMS_API_KEY and no var/dev-api-key — is the stack up (just dev)?" >&2
    return 1 2>/dev/null || exit 1
fi

if [ -z "${ENGRAMS_BIN:-}" ]; then
    cargo build -q -p engram-cli >&2
    ENGRAMS_BIN="$(pwd)/target/debug/engrams"
fi
engrams() { "$ENGRAMS_BIN" "$@"; }
