#!/usr/bin/env bash
# ADR 0058: build the `integrations-cli` RO bundle — the shared CLI toolbox for
# connected integrations. It includes the provider CLIs, the `integrations`
# discovery skill, and the `engrams-integrations` helper. Most binaries run with
# the base image's own glibc. The `gcloud` archive also brings its pinned Python
# runtime and required shared libraries.
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
#   bin/glab                  fetched static Go binary (GitLab CLI)
#   bin/stripe                fetched static Go binary (Stripe CLI)
#   bin/pup                   fetched glibc Rust binary (Datadog CLI for agents)
#   bin/gcloud                wrapper for the pinned Google Cloud CLI archive
#   bin/<provider>            committed POSIX-sh + curl connector wrappers (linear,
#                             jira, sentry, pd, … — brokered auth, copied in)
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
PUP_VERSION="${PUP_VERSION:-1.4.0}"
GLAB_VERSION="${GLAB_VERSION:-1.105.0}"
STRIPE_VERSION="${STRIPE_VERSION:-1.43.2}"
GCLOUD_VERSION="${GCLOUD_VERSION:-577.0.0}"
# Build-container base, pinned by digest (multi-arch index, amd64+arm64).
# The bundle COPIES this image's Python runtime and shared libraries into the
# tree, so a floating tag changed the bundle content between builds and broke
# the content-addressed reproducibility promise. Bump deliberately, with the
# other pins.
BOOKWORM_IMAGE="debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818"

build_tree() {
    local dest="$1"
    rm -rf "$dest"
    mkdir -p "$dest"
    # DO NOT bind-mount the OUTPUT dir. The Docker daemon may run in a Lima/Colima
    # VM reached over reverse-sshfs (macOS dev), where a bind-mounted /out is
    # unreliable: a freshly created host dir is not yet visible in the VM when
    # `docker run -v` fires (mkdir /out/* -> "No such file or directory"). Instead
    # the container builds the tree in its OWN overlayfs /out, then we `docker cp`
    # the finished tree to the host — docker cp writes host-side through the CLI,
    # with no sshfs in the path. Works identically on a native host.
    local cname=engram-bundle-build-integrations-cli
    docker rm -f "$cname" >/dev/null 2>&1 || true

    # Fetch + collect inside a glibc container (matches the standard bases'
    # glibc baseline), building the tree at /out. The container runs as root, so
    # it chowns /out back to the invoking uid at the end (kept harmless under
    # docker cp) — the bug that broke publish-bundles on the PR-#55 merge.
    docker run --name "$cname" \
        -e GH_VERSION="$GH_VERSION" \
        -e PUP_VERSION="$PUP_VERSION" \
        -e GLAB_VERSION="$GLAB_VERSION" \
        -e STRIPE_VERSION="$STRIPE_VERSION" \
        -e GCLOUD_VERSION="$GCLOUD_VERSION" \
        -e HOST_UID="$(id -u)" \
        -e HOST_GID="$(id -g)" \
        "$BOOKWORM_IMAGE" bash -euo pipefail -c '
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq
        apt-get install -y -qq --no-install-recommends curl ca-certificates tar python3
        rm -rf /var/lib/apt/lists/*

        ARCH="$(dpkg --print-architecture)"   # amd64 | arm64
        case "$ARCH" in
            amd64) GH_ARCH=amd64; PUP_ARCH=x86_64; GLAB_ARCH=amd64; STRIPE_ARCH=x86_64; GCLOUD_ARCH=x86_64; GCLOUD_SHA256=0b32d330446ce7b0f57f253e7efab4636c18fb1f87a3ac31c6c3f2a2a697525e ;;
            arm64) GH_ARCH=arm64; PUP_ARCH=arm64;  GLAB_ARCH=arm64; STRIPE_ARCH=arm64; GCLOUD_ARCH=arm; GCLOUD_SHA256=dbac26bdf80d72b5d13538e3a215dcbfe2781edfd2d69723effbeef3839cffb8 ;;
            *) echo "unsupported arch $ARCH" >&2; exit 1 ;;
        esac

        mkdir -p /out/bin

        # Provider CLIs go straight into bin/ and run with the base image s own
        # glibc — NO bundled libc / loader / libstdc++. gh is a static Go binary
        # (runs anywhere); pup is a glibc Rust binary that dynamically links the
        # base s libc (+ libgcc_s) and runs directly on any full glibc base. We
        # ship the binaries only — bundling a libc/loader is fragile (validated on
        # the dev-vm: a bundled glibc segfaults / version-mismatches). The bundle
        # targets a full glibc base, the same constraint as the binaries (manifest.toml).

        # 1) GitHub CLI — a static Go binary; the tarball nests it under
        #    gh_<ver>_linux_<arch>/bin/gh.
        curl -fsSL "https://github.com/cli/cli/releases/download/v${GH_VERSION}/gh_${GH_VERSION}_linux_${GH_ARCH}.tar.gz" \
            | tar -xz -C /tmp
        cp "/tmp/gh_${GH_VERSION}_linux_${GH_ARCH}/bin/gh" /out/bin/gh
        chmod 0755 /out/bin/gh

        # 2) Datadog pup — the official Datadog CLI for agents (a glibc Rust
        #    binary). The release tarball contains the `pup` executable.
        mkdir -p /tmp/pup
        curl -fsSL "https://github.com/DataDog/pup/releases/download/v${PUP_VERSION}/pup_${PUP_VERSION}_Linux_${PUP_ARCH}.tar.gz" \
            | tar -xz -C /tmp/pup
        cp "$(find /tmp/pup -type f -name pup | head -1)" /out/bin/pup
        chmod 0755 /out/bin/pup

        # 3) GitLab CLI (glab) — a static Go binary published on GitLab releases
        #    (mirrored on GitHub). The tarball nests it under bin/glab.
        mkdir -p /tmp/glab
        curl -fsSL "https://gitlab.com/gitlab-org/cli/-/releases/v${GLAB_VERSION}/downloads/glab_${GLAB_VERSION}_linux_${GLAB_ARCH}.tar.gz" \
            | tar -xz -C /tmp/glab
        cp "$(find /tmp/glab -type f -name glab | head -1)" /out/bin/glab
        chmod 0755 /out/bin/glab

        # 4) Stripe CLI — a static Go binary on GitHub releases; the tarball
        #    contains a single `stripe` executable at its root (ARCH = x86_64 | arm64).
        mkdir -p /tmp/stripe
        curl -fsSL "https://github.com/stripe/stripe-cli/releases/download/v${STRIPE_VERSION}/stripe_${STRIPE_VERSION}_linux_${STRIPE_ARCH}.tar.gz" \
            | tar -xz -C /tmp/stripe
        cp "$(find /tmp/stripe -type f -name stripe | head -1)" /out/bin/stripe
        chmod 0755 /out/bin/stripe

        # 5) Google Cloud CLI. The versioned archive is a
        # self-contained SDK tree, so the wrapper can locate it relative to the
        # read-only bundle mount without a login or credential file.
        curl -fsSL "https://storage.googleapis.com/cloud-sdk-release/google-cloud-cli-${GCLOUD_VERSION}-linux-${GCLOUD_ARCH}.tar.gz" \
            -o /tmp/google-cloud-cli.tar.gz
        echo "$GCLOUD_SHA256  /tmp/google-cloud-cli.tar.gz" | sha256sum -c -
        tar -xzf /tmp/google-cloud-cli.tar.gz -C /out
        # ARM archives do not include Python. Ship one runtime on both arches
        # so the bundle has the same contract on every host architecture.
        mkdir -p /out/python/bin /out/python/lib
        cp /usr/bin/python3.11 /out/python/bin/
        ln -s python3.11 /out/python/bin/python3
        cp -a /usr/lib/python3.11 /out/python/lib/

        # Ship the Python runtime non-glibc shared libraries too. Never put
        # glibc libraries in this directory: the wrapper adds it to the dynamic
        # library search path, and a bundled libc can make even the base image s
        # /bin/sh fail before gcloud starts.
        mkdir -p /out/lib
        {
            ldd /usr/bin/python3.11
            find /usr/lib/python3.11/lib-dynload -type f -name "*.so" -exec ldd {} \;
        } | awk '\''/=> \/.* \(/{print $3}'\'' | sort -u | while read -r library; do
            case "${library##*/}" in
                libc.so.*|libm.so.*|libpthread.so.*|libdl.so.*|librt.so.*|\
                libresolv.so.*|libutil.so.*|libanl.so.*|libnss_*.so.*|ld-linux*.so.*)
                    continue
                    ;;
            esac
            cp -L "$library" /out/lib/
        done

        # Fail the bundle build if the pinned SDK cannot start with only the
        # Python runtime and shared libraries that the bundle will contain.
        CLOUDSDK_PYTHON=/out/python/bin/python3 \
            LD_LIBRARY_PATH=/out/lib \
            /out/google-cloud-sdk/bin/gcloud --version >/dev/null

        chown -R "$HOST_UID:$HOST_GID" /out
    '

    # Copy the fetched tree out of the container onto the host (host-side via the
    # CLI — no sshfs), then drop the container.
    docker cp "$cname:/out/." "$dest/"
    docker rm -f "$cname" >/dev/null 2>&1 || true

    # Committed pieces (the discovery helper + skill) — copied on the host into
    # the extracted tree, not fetched. mount.json is copied by the caller
    # (stage/pack), like the other bundles.
    cp "$here/bin/engrams-integrations" "$dest/bin/engrams-integrations"
    chmod 0755 "$dest/bin/engrams-integrations"
    cp "$here/bin/gcloud" "$dest/bin/gcloud"
    chmod 0755 "$dest/bin/gcloud"
    # The Slack CLI is a committed POSIX-sh + curl wrapper (no fetched binary):
    # auth is brokered, so it just calls the Slack Web API and the proxy injects
    # the bot token host-side.
    cp "$here/bin/slack" "$dest/bin/slack"
    chmod 0755 "$dest/bin/slack"
    # The per-provider connector CLIs are committed POSIX-sh + curl wrappers (no
    # fetched binary): auth is brokered, so each just calls the provider's REST
    # API and the egress proxy injects the real credential host-side (ADR 0056/0057).
    for bin in linear jira sentry pd cloudflare vercel netlify circle newrelic \
               notion asana twilio sendgrid hubspot airtable figma discord shopify; do
        cp "$here/bin/$bin" "$dest/bin/$bin"
        chmod 0755 "$dest/bin/$bin"
    done
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
# Reproducible content-addressed pack (ADR 0027/0035): identical content MUST
# yield an identical sha across builds — see deploy/bundles/_pack.sh.
# shellcheck source=../_pack.sh
. "$(dirname "${BASH_SOURCE[0]}")/../_pack.sh"
pack_squashfs "$tmp" "$out"
sha="$(sha256sum "$out" | cut -d' ' -f1)"
echo "built $out"
echo "sha256: $sha"
