#!/usr/bin/env bash
# The release version lives in three files. This script writes it to all of
# them, or checks that all of them agree with it.
#
#   scripts/release-version.sh set 0.10.0     # the version-bump PR
#   scripts/release-version.sh check 0.10.0   # release.yml, before it publishes
#
# The chart `appVersion` is the default image tag of both charts, so a
# checkout of the tag `v0.10.0` installs the `0.10.0` images. The public site
# reads the same `appVersion` at build time (site/src/version.ts).
set -euo pipefail

usage() {
  echo "usage: $0 <set|check> <major.minor.patch>" >&2
  exit 2
}

[[ $# -eq 2 ]] || usage
MODE="$1"
VERSION="$2"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo "error: '$VERSION' is not major.minor.patch" >&2
  exit 2
}

cd "$(git rev-parse --show-toplevel)"

# Each entry is `file|extended regex with one capture before and one after the
# version`. `set` replaces the text between the captures; `check` reads it.
SITES=(
  'deploy/helm/engram/Chart.yaml|^(version: )[^[:space:]]+()$'
  'deploy/helm/engram/Chart.yaml|^(appVersion: ")[^"]+(")$'
  'deploy/helm/engram-host-fleet/Chart.yaml|^(version: )[^[:space:]]+()$'
  'deploy/helm/engram-host-fleet/Chart.yaml|^(appVersion: ")[^"]+(")$'
  'cli/package.json|^(  "version": ")[^"]+(",)$'
  'cli/src/main.ts|^(  \.version\(")[^"]+("\))$'
)

fail=0
for site in "${SITES[@]}"; do
  file="${site%%|*}"
  pattern="${site#*|}"
  # A pattern that matches no line, or more than one, means the file changed
  # shape. Stop: a silent no-op here would release a wrong version.
  hits="$(grep -cE "$pattern" "$file" || true)"
  if [[ "$hits" != "1" ]]; then
    echo "error: $file: expected one line that matches $pattern, found $hits" >&2
    exit 1
  fi
  case "$MODE" in
    set)
      tmp="$(mktemp)"
      sed -E "s/$pattern/\1$VERSION\2/" "$file" > "$tmp"
      cat "$tmp" > "$file"
      rm -f "$tmp"
      ;;
    check)
      have="$(grep -E "$pattern" "$file" | sed -E "s/$pattern/\1$VERSION\2/")"
      want="$(grep -E "$pattern" "$file")"
      if [[ "$have" != "$want" ]]; then
        echo "error: $file is not at $VERSION: $want" >&2
        fail=1
      fi
      ;;
    *) usage ;;
  esac
done

if [[ "$fail" != "0" ]]; then
  echo "Run 'scripts/release-version.sh set $VERSION' and merge the result before the release." >&2
  exit 1
fi
echo "$MODE $VERSION: ok"
