# debian-slim is enough now that the harness's runtime dependencies
# (Anthropic's bundled-Bun `claude` binary) ship host-side via the
# harness pack, mounted into every sandbox at
# `/run/engram/harnesses/claude/`. The image only needs the tools
# the *workspace* uses — git for the agent's edits, ca-certificates
# so https calls work.
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        git ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# ttyd: PTY-over-WebSocket server. Used by the dashboard's `SHELL`
# tab — coordinator proxies a WebSocket from the browser to ttyd
# inside the guest, ghostty-web on the browser side renders it.
# Static binary, no runtime deps; ~1.3 MB. Architecture-aware so the
# same Dockerfile baked for arm64 (VZ) or amd64 (FC) picks the right
# release.
ARG TTYD_VERSION=1.7.7
RUN ARCH=$(dpkg --print-architecture) \
    && case "$ARCH" in \
        arm64) TTYD_ARCH=aarch64 ;; \
        amd64) TTYD_ARCH=x86_64 ;; \
        *) echo "unsupported arch $ARCH for ttyd"; exit 1 ;; \
    esac \
    && curl -fsSL -o /usr/local/bin/ttyd \
        "https://github.com/tsl0922/ttyd/releases/download/${TTYD_VERSION}/ttyd.${TTYD_ARCH}" \
    && chmod +x /usr/local/bin/ttyd

RUN mkdir -p /workspace/.engram

# Minimal interactive-shell ergonomics for the dashboard's SHELL tab.
# `ls`/`grep` color via the standard --color=auto alias; a two-tone
# prompt (cyan user@host, blue path) so the eye can pick the cwd out
# at a glance. xterm-256color is what ttyd already advertises; we
# pin it here in case bash decides otherwise. Lands at /root/.bashrc
# because ttyd starts bash as root in /workspace.
RUN printf '%s\n' \
    'export TERM=xterm-256color' \
    'alias ls="ls --color=auto"' \
    'alias ll="ls -lah --color=auto"' \
    'alias grep="grep --color=auto"' \
    "PS1='\\[\\e[36m\\]\\u@\\h\\[\\e[0m\\]:\\[\\e[34m\\]\\w\\[\\e[0m\\]\\\$ '" \
    > /root/.bashrc
