# ttyd: PTY-over-WebSocket server. Pulled from Docker Hub because
# tsl0922's GitHub Releases page is still pinned at 1.7.7 (March
# 2024) but the 1.7.7 *release binary* segfaults at `0x403296`
# (NULL deref in the WS receive handler) on every inbound text or
# binary frame — observed in prod 2026-05-21: every SHELL-tab
# connection turned the receiving ttyd into a zombie within
# milliseconds of the first frame. 1.7.8 is master HEAD as of
# 2026-03-19 and fixes the crash (verified by running an AuthToken
# JSON + input frame through the 1.7.8 image: it returns the prompt
# instead of crashing). Multi-stage so we get a static binary from
# tsl0922's image without inheriting Alpine as the runtime base.
FROM tsl0922/ttyd:1.7.8 AS ttyd-source

# debian-slim is enough for the workspace image — ADR 0062: NO harness
# binary lives in this rootfs. The harness is a per-session selection that
# rides the fleet `current_bundles` stamp and mounts on dyn_0, so the
# Dockerfile carries only the tools the *workspace* uses: git for source
# edits, ca-certificates so https calls work, curl for ad-hoc fetches.
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        git ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=ttyd-source /usr/bin/ttyd /usr/local/bin/ttyd
RUN chmod +x /usr/local/bin/ttyd

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
