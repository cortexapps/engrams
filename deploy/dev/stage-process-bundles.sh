#!/usr/bin/env bash
# Stage host-native bundle trees for the dev-only ProcessBackend.
#
# A real host reports squashfs generations from /var/lib/engram/shared. Process
# mode runs the harness as a host subprocess, so it needs native executables in
# unpacked trees instead. `current.json` gives the synthetic in-process host the
# same logical-name -> content-id catalog contract as a real host.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

if [[ "$(uname -s)" != Linux ]]; then
    echo "bundles-process supports Linux only (macOS dev uses the VZ backend)" >&2
    exit 1
fi

case "$(uname -m)" in
    x86_64 | amd64) host_arch=x86_64; claude_arch=linux-x64 ;;
    aarch64 | arm64) host_arch=aarch64; claude_arch=linux-arm64 ;;
    *) echo "bundles-process: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

out="$PWD/var/bundles"
cache="$out/.cache"
mkdir -p "$out" "$cache"

deploy/bundles/skills/build.sh --stage "$out/skills"

# The dev-engrams image pre-stages these wrappers and then removes target/.
# Reuse the staged copies while the source tree is unchanged. Any product-plane
# source edit changes this fingerprint, which makes Tilt rebuild both native
# wrappers before it updates the catalog stamp.
wrapper_source_sha="$({
    find Cargo.toml Cargo.lock workspace-hack \
        crates/engram-core \
        crates/engram-harness-claude \
        crates/engram-harness-codex \
        crates/engram-harness-proto \
        crates/engram-harness-sdk \
        crates/engram-transport \
        -type f -print0 \
        | LC_ALL=C sort -z \
        | xargs -0 sha256sum
} | sha256sum | cut -d' ' -f1)"
wrapper_stamp="$out/.process-harness-source-sha"
build_wrappers=1
if [[ -x "$out/harness-claude/harness" ]] \
    && [[ -x "$out/harness-codex/harness" ]] \
    && [[ -f "$wrapper_stamp" ]] \
    && [[ "$(<"$wrapper_stamp")" == "$wrapper_source_sha" ]]; then
    build_wrappers=0
fi
if (( build_wrappers )); then
    cargo build -p engram-harness-claude
    cargo build -p engram-harness-codex
fi

claude_version=2.1.212
claude_bin="$cache/claude-$claude_version-$claude_arch"
if [[ ! -x "$claude_bin" ]]; then
    curl -fsSL --retry 3 \
        "https://downloads.claude.ai/claude-code-releases/$claude_version/$claude_arch/claude" \
        -o "$claude_bin"
    chmod 0755 "$claude_bin"
fi
mkdir -p "$out/harness-claude"
if (( build_wrappers )); then
    cp -p target/debug/engram-harness-claude "$out/harness-claude/harness"
fi
cp -p "$claude_bin" "$out/harness-claude/claude"
cp -p deploy/harness-claude/harness.toml "$out/harness-claude/harness.toml"

codex_version="$(sed -n 's/^version=//p' deploy/harness-codex/fetch-codex.sh)"
codex_stamp="$out/harness-codex/.engram-codex-version"
if [[ ! -x "$out/harness-codex/codex" ]] \
    || [[ ! -f "$codex_stamp" ]] \
    || [[ "$(<"$codex_stamp")" != "$codex_version-$host_arch" ]]; then
    # A missing or stale CLI needs stage.sh's schema check and therefore a
    # freshly built wrapper path, even when the source fingerprint matched.
    if (( ! build_wrappers )); then
        cargo build -p engram-harness-codex
        build_wrappers=1
    fi
    deploy/harness-codex/stage.sh \
        "$host_arch" target/debug/engram-harness-codex "$out/harness-codex"
    printf '%s\n' "$codex_version-$host_arch" > "$codex_stamp"
elif (( build_wrappers )); then
    cp -p target/debug/engram-harness-codex "$out/harness-codex/harness"
    cp -p deploy/harness-codex/harness.toml "$out/harness-codex/harness.toml"
fi

printf '%s\n' "$wrapper_source_sha" > "$wrapper_stamp"

tree_sha() {
    local dir="$1"
    (
        cd "$dir"
        find . -type f -print0 \
            | LC_ALL=C sort -z \
            | xargs -0 sha256sum \
            | sha256sum \
            | cut -d' ' -f1
    )
}

skills_sha="$(tree_sha "$out/skills")"
claude_sha="$(tree_sha "$out/harness-claude")"
codex_sha="$(tree_sha "$out/harness-codex")"
stamp_tmp="$out/.current.json.tmp"
printf '{"skills":"%s","harness-claude":"%s","harness-codex":"%s"}\n' \
    "$skills_sha" "$claude_sha" "$codex_sha" > "$stamp_tmp"
mv "$stamp_tmp" "$out/current.json"

echo "staged ProcessBackend bundles:"
cat "$out/current.json"
