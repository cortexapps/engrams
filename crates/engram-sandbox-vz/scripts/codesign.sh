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
# Idempotent — re-running on already-signed binaries is fast:
# we ask `codesign -d --entitlements -` whether our entitlement
# is already embedded, and skip when it is. Only freshly-rebuilt
# binaries (cargo emits unsigned bytes) hit the actual signing
# path. The ad-hoc identity (`-`) is enough for locally-built
# dev binaries on Apple Silicon; CI runs the same script.
# Distribution to other machines would need a real signing
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

# True iff `$1` is already signed with our VZ entitlement.
# `codesign -d --entitlements -` prints the embedded
# entitlements plist to stdout — XML on modern macOS. We
# redirect stderr to dodge the "Executable=..." banner that
# goes there, and grep for our entitlement key. Unsigned and
# wrong-entitlement binaries miss the grep and get re-signed.
# Cost: ~7ms per binary (reads only the signature blob from
# the Mach-O header).
has_vz_entitlement() {
    codesign -d --entitlements - "$1" 2>/dev/null \
        | grep -q "com.apple.security.virtualization"
}

sign_if_needed() {
    local bin="$1"
    if has_vz_entitlement "$bin"; then
        return 0
    fi
    echo "codesign $bin"
    codesign --force --sign - --entitlements "$ENT" "$bin"
    count=$((count + 1))
}

# `count` here tracks *signed* binaries — `examined` is the total
# (signed + skipped). Both drive the fail-fast and the summary.
count=0
examined=0

# Coordinator binary.
BIN="target/$PROFILE/engram-coordinator"
if [ -x "$BIN" ] && [ -f "$BIN" ]; then
    sign_if_needed "$BIN"
    examined=$((examined + 1))
fi

# Host-agent binary.
BIN="target/$PROFILE/engram-host-agent"
if [ -x "$BIN" ] && [ -f "$BIN" ]; then
    sign_if_needed "$BIN"
    examined=$((examined + 1))
fi

# Test binaries: target/$PROFILE/deps/engram_sandbox_vz-<16hex>
# (no extension). Cargo emits .d / .o / .rmeta siblings alongside
# the executable; filter via `file -b` to sign only the Mach-O
# executable.
shopt -s nullglob
for T in target/"$PROFILE"/deps/engram_sandbox_vz-*; do
    if [ -x "$T" ] && [ -f "$T" ]; then
        case "$(file -b "$T")" in
            "Mach-O 64-bit executable"*)
                sign_if_needed "$T"
                examined=$((examined + 1))
                ;;
        esac
    fi
done

if [ "$examined" -eq 0 ]; then
    echo "codesign.sh: no binaries found under target/$PROFILE/ — was --tests forgotten?" >&2
    exit 1
fi

skipped=$((examined - count))
echo "codesign.sh: signed $count, skipped $skipped (already up-to-date)"
