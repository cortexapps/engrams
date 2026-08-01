#!/usr/bin/env bash
# Fingerprint cache for the dev bundle builds (`just bundles-squashfs`).
#
# Every bundle is content-addressed (`<sha256>.squashfs`, ADR 0027/0035), so a
# rebuild from unchanged inputs writes back the byte-identical file it already
# had. The build itself is not cheap though: `browser`, `ide` and
# `integrations-cli` each start a container, run `apt-get update`, and download
# node/playwright/code-server/gh/pup/glab/stripe from the network. On a `just
# dev` loop that cost is paid on every Tilt trigger for no change at all.
#
# So: hash the INPUTS, remember which output sha they produced, and skip the
# build when the inputs match and that output is still on disk.
#
# What the fingerprint covers is per-bundle and the caller's choice — see
# `bundle_fingerprint`. What it deliberately does NOT cover: floating remote
# state that the build pulls but does not pin, i.e. the `apt-get install` set in
# the Docker bundles and the base image behind a mutable tag. Pinned downloads
# (ttyd, code-server, gh/pup/glab/stripe, the claude CLI) are pinned in the
# build scripts, so a version bump edits a tracked file and moves the
# fingerprint. Set ENGRAM_BUNDLES_FORCE=1 to ignore the cache and rebuild
# everything when you want the unpinned layers refreshed.
#
# Cache layout: <shared_dir>/.fingerprints/<name> holding "<fingerprint> <sha>".
# A hit still verifies <shared_dir>/<sha>.squashfs exists, so `just
# reap-bundles` (which prunes any squashfs the stamp does not name) degrades to
# a rebuild rather than a dangling stamp entry.

_bundle_sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

_bundle_sha256_stdin() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum | cut -d' ' -f1
    else
        shasum -a 256 | cut -d' ' -f1
    fi
}

# bundle_fingerprint <input>... -> sha256 hex on stdout
#
# Each input is one of:
#   - a directory: every regular file under it contributes its path, its
#     executable bit, and its content hash (the exec bit matters — the skills
#     bundle chmods its wrappers, and losing +x silently breaks the guest);
#   - a regular file: same, for one file (used for a built binary, e.g. the
#     cross-compiled engram-agentd);
#   - anything else: mixed in verbatim as an opaque literal (used for the host
#     arch, which selects a different download in several build scripts).
#
# Paths that do not exist are literals, so a missing input never silently
# collides with an empty one.
bundle_fingerprint() {
    local input path
    {
        for input in "$@"; do
            if [ -d "$input" ]; then
                # `sort` under a fixed locale so the digest does not depend on
                # the caller's LC_COLLATE.
                find "$input" -type f -print0 | LC_ALL=C sort -z |
                    while IFS= read -r -d '' path; do
                        _bundle_fingerprint_file "$path"
                    done
            elif [ -f "$input" ]; then
                _bundle_fingerprint_file "$input"
            else
                printf 'literal %s\n' "$input"
            fi
        done
    } | _bundle_sha256_stdin
}

_bundle_fingerprint_file() {
    local path="$1" mode='-'
    if [ -x "$path" ]; then mode='x'; fi
    printf 'file %s %s %s\n' "$path" "$mode" "$(_bundle_sha256_file "$path")"
}

# bundle_cache_get <shared_dir> <name> <fingerprint>
# Prints the cached output sha and returns 0 on a hit; returns 1 on a miss.
bundle_cache_get() {
    local shared="$1" name="$2" fingerprint="$3" file cached_fp cached_sha
    if [ -n "${ENGRAM_BUNDLES_FORCE:-}" ]; then return 1; fi
    file="$shared/.fingerprints/$name"
    [ -f "$file" ] || return 1
    read -r cached_fp cached_sha < "$file" || return 1
    [ "$cached_fp" = "$fingerprint" ] || return 1
    [ -n "$cached_sha" ] && [ -f "$shared/$cached_sha.squashfs" ] || return 1
    printf '%s\n' "$cached_sha"
}

# bundle_cache_put <shared_dir> <name> <fingerprint> <sha>
bundle_cache_put() {
    local shared="$1" name="$2" fingerprint="$3" sha="$4"
    mkdir -p "$shared/.fingerprints"
    printf '%s %s\n' "$fingerprint" "$sha" > "$shared/.fingerprints/$name"
}
