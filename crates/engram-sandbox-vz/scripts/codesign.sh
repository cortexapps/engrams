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
#
# Hot-path optimization: each signed binary gets a sibling
# `<binary>.signed` marker file containing the combined sha256 of
# the binary + the entitlements plist at sign time. Re-runs hash
# the current binary + ENT and short-circuit when the marker
# matches. Steady-state Tilt rebuilds with no source changes drop
# from "sign N binaries, ~Ns total" to "shasum N binaries,
# ~50ms total." Markers live in `target/` (gitignored).

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

# Combined sha of one binary + ENT. Used as the marker contents
# so that any change to either invalidates the cache and forces a
# re-sign. Stripping paths via awk keeps the value stable across
# absolute/relative `bin` invocations (Tilt runs us from repo root,
# but a defensive measure).
sha_of_pair() {
    local bin="$1"
    shasum -a 256 "$bin" "$ENT" | awk '{print $1}'
}

# Sign `$1` only if its (binary, ENT) sha pair differs from the
# marker file beside it. Increments `count` only when we actually
# signed — so the "no binaries found" fail-fast at the bottom of
# the script still works.
sign_if_needed() {
    local bin="$1"
    local marker="${bin}.signed"
    local cur
    cur="$(sha_of_pair "$bin")"
    if [ -f "$marker" ] && [ "$(cat "$marker")" = "$cur" ]; then
        # Bytes haven't changed since the last sign and the
        # entitlements file hasn't either. Trust the existing
        # signature — codesign --verify would do more work for
        # the same information.
        return 0
    fi
    echo "codesign $bin"
    codesign --force --sign - --entitlements "$ENT" "$bin"
    # Re-compute sha *after* signing — `codesign --force` mutates
    # the Mach-O's load commands to embed the signature, so the
    # post-sign hash is what we need to compare against next time.
    sha_of_pair "$bin" > "$marker"
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

# Test binaries: target/$PROFILE/deps/engram_sandbox_vz-<16hex>
# (no extension). We skip .d / .o / .rmeta / etc. by checking the
# magic bytes via `file`, which is reliable across cargo's varied
# intermediate file naming. Also skip our own `.signed` markers.
shopt -s nullglob
for T in target/"$PROFILE"/deps/engram_sandbox_vz-*; do
    case "$T" in
        *.signed) continue ;;  # our marker, not a binary
    esac
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
