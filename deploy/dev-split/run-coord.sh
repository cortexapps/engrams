#!/usr/bin/env bash
# Launch the coordinator in --mode=coordinator. Bearer auth on; the
# host-agent dials in with the matching token. No sandbox backend
# constructed here — the host owns the FC backend.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
exec /nix/var/nix/profiles/default/bin/nix develop --command bash -lc '
  set -a
  source .env
  set +a
  export DATABASE_URL=postgres://engram:engram@localhost:5435/engram
  export ENGRAM_BIND_ADDR=127.0.0.1:8090
  export ENGRAM_MODE=coordinator
  export ENGRAM_AUTH_TOKENS=dev-split-token
  export ENGRAM_LOCAL_PATH=./var/engram
  export ENGRAM_BLOB_BACKEND=local
  export RUST_LOG=info,engram=debug
  exec ./target/debug/engram-coordinator
'
