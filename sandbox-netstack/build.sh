#!/usr/bin/env bash
set -euo pipefail
mode=${1:-test}
root=$(cd "$(dirname "$0")/.." && pwd -P)
out=${2:-"$root/var/mcp-netstack-build"}
case "$mode" in build|test|race) ;; *) echo 'usage: build.sh [build|test|race] [owned-output-directory]' >&2; exit 2;; esac
# Cache/output ownership is explicit. Never follow a cache symlink.
[[ ! -L "$out" ]] || exit 1
if [[ ! -e "$out" ]]; then
  mkdir -m 700 "$out"
  printf '%s\n' 'sandbox-netstack build outputs and Go caches' > "$out/ownership.txt"
fi
[[ -f "$out/ownership.txt" && ! -L "$out/ownership.txt" ]] || exit 1
out=$(cd "$out" && pwd -P)
for dir in go-path go-mod go-cache tmp; do
  [[ ! -L "$out/$dir" ]] || exit 1
  mkdir -p "$out/$dir"
done
export GOTOOLCHAIN=local GOMAXPROCS=2 CGO_ENABLED=0
export GOPATH="$out/go-path" GOMODCACHE="$out/go-mod" GOCACHE="$out/go-cache" TMPDIR="$out/tmp"
[[ $(go env GOVERSION) == go1.26.2 ]] || { echo 'Go 1.26.2 is required' >&2; exit 1; }
cd "$root/sandbox-netstack"
case "$mode" in
 build) go build -mod=readonly -trimpath -o "$out/engram-sandbox-netstack" . ;;
 test) go test -mod=readonly -timeout 30s ./...; go vet ./...; go mod verify ;;
 race) CGO_ENABLED=1 go test -mod=readonly -race -timeout 45s ./... ;;
esac
