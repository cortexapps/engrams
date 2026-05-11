#!/usr/bin/env bash
# Install engram-host-agent at /usr/local/bin/engram-host-agent.
# Pulled from a GCS URL the operator's CI populated before this
# Packer build kicks off. Static-musl binary so the image doesn't
# need a Rust toolchain.

set -euo pipefail

: "${HOST_AGENT_GCS_URL:?HOST_AGENT_GCS_URL must be a gs:// URL pointing at the pre-built binary}"

echo "==> downloading engram-host-agent from $HOST_AGENT_GCS_URL"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

gsutil cp "$HOST_AGENT_GCS_URL" "$tmp/engram-host-agent"
file "$tmp/engram-host-agent"

# Static-musl ELF; chmod and install.
sudo install -m 0755 "$tmp/engram-host-agent" /usr/local/bin/engram-host-agent
/usr/local/bin/engram-host-agent --version 2>&1 | head -1 || \
    /usr/local/bin/engram-host-agent --help >/dev/null

echo "==> engram-host-agent installed"
