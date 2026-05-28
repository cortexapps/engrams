# Identical OS + tooling to deploy/demo/'s Dockerfile — the
# difference between this image and the harness-less demo is *not*
# in the rootfs you describe here. The baker reads
# `[harness] builtin = "claude"` in engram.toml and pulls the
# published harness-claude OCI artifact, extracting it into
# /opt/engram/harness/ during the bake. The Dockerfile stays
# Docker-native; nothing engram-specific leaks into it.
#
# ttyd: PTY-over-WebSocket. See deploy/demo/Dockerfile for the
# 1.7.7-segfault history; we pin 1.7.8 from tsl0922's Docker Hub
# image to avoid inheriting Alpine as the runtime base.
FROM tsl0922/ttyd:1.7.8 AS ttyd-source

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        git ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=ttyd-source /usr/bin/ttyd /usr/local/bin/ttyd
RUN chmod +x /usr/local/bin/ttyd

RUN mkdir -p /workspace/.engram

# Same shell ergonomics as the harness-less demo — the SHELL tab in
# the dashboard still works for an Agent session of this image (the
# agent runs alongside, doesn't pre-empt the PTY).
RUN printf '%s\n' \
    'export TERM=xterm-256color' \
    'alias ls="ls --color=auto"' \
    'alias ll="ls -lah --color=auto"' \
    'alias grep="grep --color=auto"' \
    "PS1='\\[\\e[36m\\]\\u@\\h\\[\\e[0m\\]:\\[\\e[34m\\]\\w\\[\\e[0m\\]\\\$ '" \
    > /root/.bashrc
