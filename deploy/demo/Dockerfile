# ADR 0080: a session image is a plain OCI image. The image contract is
# just "any linux image with /bin/sh" — engrams supplies everything else
# host-side:
#   - engram-agentd + the stage-1 init: the `bundle-agentd` slot (ADR 0080 §A)
#   - the harness (built-in `claude`): the `harness-claude` bundle slot (ADR 0062)
#   - ttyd (the dashboard SHELL tab): the `guest-tools` bundle slot (ADR 0080 §D)
#   - the SHELL-tab .bashrc ergonomics: written by the stage-1 init if absent
# So the Dockerfile carries only the tools the *workspace* uses: git for
# source edits, ca-certificates so https works, curl for ad-hoc fetches,
# socat + iproute2 for the in-guest port relay, and the code-search trio the
# PR reviewer leans on — jq, ripgrep, and ast-grep (ADR 0100). unzip is a
# build-only helper for the ast-grep release archive.
# Pinned by digest so a re-bake of this commit can't float to a new base image
# (ADR 0100 reviewer-image reproducibility). To bump: pull debian:bookworm-slim
# and copy the new `RepoDigests` index digest here.
FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818

# apt is PINNED to a snapshot.debian.org timestamp — never floated from bookworm.
# Same rationale as the browser bundle (deploy/bundles/browser/build.sh): an
# unpinned `apt-get install` silently rides every Debian push into the next
# reviewer image with no detection, so a re-bake of the same commit could produce
# a materially different reviewer. Freezing the repo to a snapshot makes the whole
# dependency closure (including ca-certificates itself) reproducible.
#
# The stock slim base ships no CA bundle, so https to the snapshot can't be
# verified yet — bootstrap ca-certificates from the stock (http) repo first, then
# repoint to the https snapshot and install everything (which re-pins
# ca-certificates from the snapshot too). To BUMP: pick a newer timestamp from
# https://snapshot.debian.org/ and update SNAPSHOT_TS.
ARG SNAPSHOT_TS=20260630T000000Z
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates; \
    rm -f /etc/apt/sources.list.d/debian.sources; \
    { \
        echo "deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/${SNAPSHOT_TS} bookworm main"; \
        echo "deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/${SNAPSHOT_TS} bookworm-updates main"; \
        echo "deb [check-valid-until=no] https://snapshot.debian.org/archive/debian-security/${SNAPSHOT_TS} bookworm-security main"; \
    } > /etc/apt/sources.list; \
    apt-get -o Acquire::Check-Valid-Until=false update; \
    apt-get install -y --no-install-recommends \
        git ca-certificates curl socat iproute2 jq ripgrep unzip; \
    rm -rf /var/lib/apt/lists/*

# ast-grep (ADR 0100): a prebuilt release binary — not in apt. Arch-mapped from
# the build's own architecture so the same Dockerfile bakes on arm64 (the guest
# kernel arch) and amd64. The release archive is verified against a pinned
# sha256 (per arch) so a moved or tampered upstream asset fails the build instead
# of installing silently. We install only `ast-grep`, NOT its `sg` alias, which
# would shadow Debian's built-in `sg` (setgroups) from the `passwd` package. To
# bump ASTGREP_VERSION, update both checksums (shasum -a 256 the release zips).
ARG ASTGREP_VERSION=0.44.1
ARG ASTGREP_SHA256_AARCH64=077a4ab0c628154ef3cb79fecaf11dabca7f8a41f2c7260c022f263a52c1b021
ARG ASTGREP_SHA256_X86_64=611f9e5e76f2611ecea1a35dd3468ceedf600641a11224b80341d79c6ee7b9dd
RUN set -eux; \
    case "$(dpkg --print-architecture)" in \
        arm64) sg_arch=aarch64; sg_sha="$ASTGREP_SHA256_AARCH64" ;; \
        amd64) sg_arch=x86_64; sg_sha="$ASTGREP_SHA256_X86_64" ;; \
        *) echo "ast-grep: unsupported arch $(dpkg --print-architecture)" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/ast-grep.zip \
        "https://github.com/ast-grep/ast-grep/releases/download/${ASTGREP_VERSION}/app-${sg_arch}-unknown-linux-gnu.zip"; \
    echo "${sg_sha}  /tmp/ast-grep.zip" | sha256sum -c -; \
    unzip -o /tmp/ast-grep.zip -d /tmp/ast-grep; \
    install -m 0755 /tmp/ast-grep/ast-grep /usr/local/bin/ast-grep; \
    rm -rf /tmp/ast-grep.zip /tmp/ast-grep; \
    ast-grep --version

RUN mkdir -p /workspace/.engrams
