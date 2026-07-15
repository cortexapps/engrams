# ADR 0080: a session image is a plain OCI image. The image contract is
# just "any linux image with /bin/sh" — engrams supplies everything else
# host-side:
#   - engram-agentd + the stage-1 init: the `bundle-agentd` slot (ADR 0080 §A)
#   - the harness (built-in `claude`): the `harness-claude` bundle slot (ADR 0062)
#   - ttyd (the dashboard SHELL tab): the `guest-tools` bundle slot (ADR 0080 §D)
#   - the SHELL-tab .bashrc ergonomics: written by the stage-1 init if absent
# So the Dockerfile carries only the tools the *workspace* uses: git for
# source edits, ca-certificates so https works, curl for ad-hoc fetches,
# socat + iproute2 for the in-guest port relay.
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        git ca-certificates curl socat iproute2 \
    && rm -rf /var/lib/apt/lists/*

RUN mkdir -p /workspace/.engrams
