#!/usr/bin/env bash
# Ad-hoc codesign engram-coordinator + engram-sandbox-vz test
# binaries with the com.apple.security.virtualization entitlement.
#
# Without this, every VZ API call fails with NSError 7 "process
# doesn't have the com.apple.security.virtualization entitlement".
# See crates/engram-sandbox-vz/src/vm.rs for the smoke test that
# detects unsigned binaries.
#
# Usage: codesign.sh <profile>      (e.g. `codesign.sh debug`)
#
# Idempotent — re-running on already-signed binaries replaces the
# signature in place. The ad-hoc identity (`-`) is enough for
# locally-built dev binaries on Apple Silicon; CI runs the same
# script. Distribution to other machines would need a real signing
# identity + notarization, which is out of scope.

set -euo pipefail

PROFILE="${1:?usage: $0 <profile>}"
ENT="crates/engram-sandbox-vz/entitlements.plist"

if [ "$(uname -s)" != "Darwin" ]; then
    echo "codesign.sh: macOS-only, skipping on $(uname -s)" >&2
    exit 0
fi

if [ ! -f "$ENT" ]; then
    echo "codesign.sh: entitlements file not found at $ENT" >&2
    exit 1
fi

count=0

# Coordinator binary.
BIN="target/$PROFILE/engram-coordinator"
if [ -x "$BIN" ] && [ -f "$BIN" ]; then
    echo "codesign $BIN"
    codesign --force --sign - --entitlements "$ENT" "$BIN"
    count=$((count + 1))
fi

# Test binaries: target/$PROFILE/deps/engram_sandbox_vz-<16hex>
# (no extension). We skip .d / .o / .rmeta / etc. by checking the
# magic bytes via `file`, which is reliable across cargo's varied
# intermediate file naming.
shopt -s nullglob
for T in target/"$PROFILE"/deps/engram_sandbox_vz-*; do
    if [ -x "$T" ] && [ -f "$T" ]; then
        case "$(file -b "$T")" in
            "Mach-O 64-bit executable"*)
                echo "codesign $T"
                codesign --force --sign - --entitlements "$ENT" "$T"
                count=$((count + 1))
                ;;
        esac
    fi
done

if [ "$count" -eq 0 ]; then
    echo "codesign.sh: no binaries found under target/$PROFILE/ — was --tests forgotten?" >&2
    exit 1
fi

echo "codesign.sh: signed $count binary/binaries with $ENT"
