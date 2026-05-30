# Dogfooding rootfs (ADR 0023). debian-slim + git + the Rust toolchain
# to build/test engrams' product plane + the built-in create-pull-request
# skill. The baker reads `[harness] builtin = "claude"` in engram.toml and
# injects the published harness-claude artifact into /opt/engram/harness/;
# the FC baker injects `engram-agentd` (carrying the ADR 0023 `forge-*`
# subcommands the helpers below call). Nothing engram-specific leaks into
# this Dockerfile beyond the skill + forge helper scripts.
FROM tsl0922/ttyd:1.7.8 AS ttyd-source

FROM debian:bookworm-slim

# git for clone/push; build-essential + pkg-config + protobuf-compiler so
# the agent can `cargo build`/`cargo test` engrams' product-plane crates
# (the FC substrate crates need KVM headers and aren't the dogfood target).
RUN apt-get update && apt-get install -y --no-install-recommends \
        git ca-certificates curl build-essential pkg-config \
        protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

COPY --from=ttyd-source /usr/bin/ttyd /usr/local/bin/ttyd
RUN chmod +x /usr/local/bin/ttyd

# Pinned Rust toolchain via rustup (matches the workspace rust-version
# floor; the agent runs `cargo test -p engram-coordinator` etc.).
ENV RUSTUP_HOME=/opt/rust CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain 1.80.0 --profile minimal \
    && rustup component add clippy rustfmt

# ADR 0023 forge helpers. `git-askpass` feeds git a fresh credential per
# operation (no token in the guest); `engram-pr` opens a change request.
# Both shell out to the baked `engram-agentd forge-*` subcommands, which
# dial the host's forge vsock port.
COPY git-askpass /opt/engram/git-askpass
COPY engram-pr /usr/local/bin/engram-pr
RUN chmod +x /opt/engram/git-askpass /usr/local/bin/engram-pr

# Git uses the askpass for the password and the configured username for
# the user half, so the askpass is only consulted for the secret.
RUN git config --global core.askPass /opt/engram/git-askpass \
    && git config --global 'credential.https://github.com.username' x-access-token \
    && git config --global credential.helper ''

# Built-in skill at the harness-agnostic path; symlink the Claude Code
# loader dir at it so the resident harness discovers it (ADR 0023).
COPY skills /root/.agents/skills
RUN mkdir -p /root/.claude && ln -sfn /root/.agents/skills /root/.claude/skills

RUN mkdir -p /workspace/.engram

RUN printf '%s\n' \
    'export TERM=xterm-256color' \
    'export PATH=/opt/cargo/bin:$PATH' \
    'alias ls="ls --color=auto"' \
    'alias ll="ls -lah --color=auto"' \
    "PS1='\\[\\e[36m\\]\\u@\\h\\[\\e[0m\\]:\\[\\e[34m\\]\\w\\[\\e[0m\\]\\\$ '" \
    > /root/.bashrc
