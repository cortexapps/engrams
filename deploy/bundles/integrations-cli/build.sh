#!/usr/bin/env bash
# ADR 0058: build the `integrations-cli` RO bundle — the shared CLI toolbox for
# connected integrations. The GitHub CLI (`gh`, a static Go binary) + Datadog CI
# (`datadog-ci`, a Node SEA standalone), plus the `integrations` discovery skill
# and the `engrams-integrations` helper. The binaries run with the base image's
# own glibc (no bundled libc/loader/libstdc++ — see the build_tree note); the
# bundle targets a full glibc base, the same constraint as the binaries.
#
# Auth is brokered (ADR 0056/0057): each CLI carries only a harmless placeholder
# token in its env; the egress proxy injects the real, capability-scoped
# credential host-side. The bundle ships the *binaries* — the connector `cli`
# facet supplies the placeholder env + the per-tool docs (ADR 0058).
#
# We ship CLIs (not MCP servers): the agent drives them via bash, which is
# harness-agnostic and needs no per-harness MCP config (ADR 0027 "Why not an
# MCP server").
#
# Layout produced (ADR 0055: mounted at a dynamic reserved slot
# `/opt/engram/dyn/<i>`; agentd symlinks each bin/ entry onto PATH):
#   bin/gh                    fetched static Go binary
#   bin/datadog-ci            fetched Node SEA standalone binary (base glibc)
#   bin/engrams-integrations  the discovery helper (committed; copied in)
#   skills/integrations/      the SKILL.md (committed; copied in)
#
# Usage:
#   build.sh --stage <dir>      # produce the unpacked tree at <dir> (dev)
#   build.sh <out.squashfs>     # produce the tree, then pack to squashfs (CI/FC)
#
# Pins live in manifest.toml. Requires Docker; the pack path also needs mksquashfs.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Pinned versions — keep in lockstep with manifest.toml.
GH_VERSION="${GH_VERSION:-2.62.0}"
DATADOG_CI_VERSION="${DATADOG_CI_VERSION:-2.48.0}"

build_tree() {
    local dest="$1"
    rm -rf "$dest"
    mkdir -p "$dest"

    # Fetch + collect inside a glibc container (matches the standard bases'
    # glibc baseline), then the tree is left at $dest (bind mount). The
    # container runs as root, so it chowns /out back to the invoking uid at the
    # end — otherwise the unprivileged CI runner can't pack the root-owned tree
    # (the bug that broke publish-bundles on the PR-#55 merge).
    docker run --rm \
        -e GH_VERSION="$GH_VERSION" \
        -e DATADOG_CI_VERSION="$DATADOG_CI_VERSION" \
        -e HOST_UID="$(id -u)" \
        -e HOST_GID="$(id -g)" \
        -v "$dest:/out" \
        debian:bookworm-slim bash -euo pipefail -c '
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq
        apt-get install -y -qq --no-install-recommends curl ca-certificates tar
        rm -rf /var/lib/apt/lists/*

        ARCH="$(dpkg --print-architecture)"   # amd64 | arm64
        case "$ARCH" in
            amd64) GH_ARCH=amd64; DDCI_ARCH=x64 ;;
            arm64) GH_ARCH=arm64; DDCI_ARCH=arm64 ;;
            *) echo "unsupported arch $ARCH" >&2; exit 1 ;;
        esac

        mkdir -p /out/bin

        # The two CLIs go straight into bin/ and run with the base image s own
        # glibc — NO bundled libc / loader / libstdc++. Validated on the dev-vm:
        # gh is a static Go binary (runs anywhere); datadog-ci is a Node SEA that
        # dynamically links the base s glibc + libstdc++ and runs fine directly,
        # but SEGFAULTS under a bundled libc/loader (a pkg/SEA binary is
        # loader-sensitive) and a bundled libstdc++ built against a newer glibc
        # demands that newer glibc on the base. So we ship the binaries only;
        # the bundle targets a full glibc base (libstdc++ present) — same
        # realistic constraint as the binaries themselves (see manifest.toml).

        # 1) GitHub CLI — a static Go binary; the tarball nests it under
        #    gh_<ver>_linux_<arch>/bin/gh.
        curl -fsSL "https://github.com/cli/cli/releases/download/v${GH_VERSION}/gh_${GH_VERSION}_linux_${GH_ARCH}.tar.gz" \
            | tar -xz -C /tmp
        cp "/tmp/gh_${GH_VERSION}_linux_${GH_ARCH}/bin/gh" /out/bin/gh
        chmod 0755 /out/bin/gh

        # 2) Datadog CI — a single standalone executable (Node SEA).
        curl -fsSL -o /out/bin/datadog-ci \
            "https://github.com/DataDog/datadog-ci/releases/download/v${DATADOG_CI_VERSION}/datadog-ci_linux-${DDCI_ARCH}"
        chmod 0755 /out/bin/datadog-ci

        chown -R "$HOST_UID:$HOST_GID" /out
    '

    # Committed pieces (the discovery helper + skill) — copied on the host, not
    # fetched. mount.json is copied by the caller (stage/pack), like the other
    # bundles.
    cp "$here/bin/engrams-integrations" "$dest/bin/engrams-integrations"
    chmod 0755 "$dest/bin/engrams-integrations"
    mkdir -p "$dest/skills"
    cp -R "$here/skills/." "$dest/skills/"
}

if [[ "${1:-}" == "--stage" ]]; then
    dest="${2:?usage: build.sh --stage <dir>}"
    build_tree "$dest"
    cp "$here/mount.json" "$dest/"  # ADR 0055: activate() reads this
    echo "staged integrations-cli bundle tree -> $dest"
    exit 0
fi

out="${1:?usage: build.sh <out.squashfs> | --stage <dir>}"
command -v mksquashfs >/dev/null || {
    echo "mksquashfs not found (install squashfs-tools)" >&2
    exit 1
}
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
build_tree "$tmp"
cp "$here/mount.json" "$tmp/"  # ADR 0055: activate() reads this
rm -f "$out"
mksquashfs "$tmp" "$out" -comp zstd -all-root -noappend -no-xattrs >/dev/null
sha="$(sha256sum "$out" | cut -d' ' -f1)"
echo "built $out"
echo "sha256: $sha"
