#!/usr/bin/env bash
set -euo pipefail
umask 077
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
bun="$(command -v bun || true)"
if [[ -z "$bun" || "$bun" != /* ]]; then
  echo 'NEEDS_CONTEXT:bun' >&2
  exit 2
fi
exec env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin "$bun" "$root/tests/fixtures/mcp-stack/platform/main.ts" --backend firecracker "$@"
