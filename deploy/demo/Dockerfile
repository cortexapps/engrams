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
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        git ca-certificates curl socat iproute2 jq ripgrep unzip \
    && rm -rf /var/lib/apt/lists/*

# ast-grep (ADR 0100): a prebuilt release binary — not in apt. Arch-mapped from
# the build's own architecture so the same Dockerfile bakes on arm64 (the guest
# kernel arch) and amd64. We install only `ast-grep`, NOT its `sg` alias, which
# would shadow Debian's built-in `sg` (setgroups) from the `passwd` package.
ARG ASTGREP_VERSION=0.44.1
RUN set -eux; \
    case "$(dpkg --print-architecture)" in \
        arm64) sg_arch=aarch64 ;; \
        amd64) sg_arch=x86_64 ;; \
        *) echo "ast-grep: unsupported arch $(dpkg --print-architecture)" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/ast-grep.zip \
        "https://github.com/ast-grep/ast-grep/releases/download/${ASTGREP_VERSION}/app-${sg_arch}-unknown-linux-gnu.zip"; \
    unzip -o /tmp/ast-grep.zip -d /tmp/ast-grep; \
    install -m 0755 /tmp/ast-grep/ast-grep /usr/local/bin/ast-grep; \
    rm -rf /tmp/ast-grep.zip /tmp/ast-grep; \
    ast-grep --version

RUN mkdir -p /workspace/.engrams
