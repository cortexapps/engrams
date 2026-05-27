#!/usr/bin/env bash
#
# Installs the OpenTelemetry Collector (contrib) on the FC host image as a
# local trace collector for ADR 0019. host-agent exports OTLP to
# localhost:4317; in-guest agentd reaches it at the per-sandbox TAP gateway
# (<gateway>:4317). The collector forwards traces to Google Cloud Trace via
# the `googlecloud` exporter (contrib-only), authing through the FC-host
# instance SA (roles/cloudtrace.agent, granted by the fc-host-mig TF module).
#
# We install otelcol-CONTRIB (not core) because the `googlecloud` exporter
# only ships in contrib. Config lives at /etc/otelcol/config.yaml (baked
# from deploy/otel/collector-gcp.yaml); the systemd unit is
# provisioners/systemd/otelcol.service.
#
# Sibling of install-ops-agent.sh — the Ops Agent ships the journal to
# Cloud Logging; this ships boot/restore/evac traces to Cloud Trace.

set -euo pipefail

# Pin a VERIFIED-GOOD contrib release. NB: some releases (e.g. 0.116.0)
# ship a dynamically-linked binary whose distroless container image lacks
# the glibc loader — that breaks the k8s sidecar with `exec: no such file
# or directory`. The .deb installs onto Ubuntu (which has the loader) so the
# host path is less exposed, but we pin the same known-good version
# everywhere. 0.111.0 is verified to run (`otelcol-contrib --version`).
# Bump deliberately and re-verify both the .deb and the container image.
OTELCOL_VERSION="${OTELCOL_VERSION:-0.111.0}"
arch="$(dpkg --print-architecture)" # amd64 | arm64
deb="otelcol-contrib_${OTELCOL_VERSION}_linux_${arch}.deb"
url="https://github.com/open-telemetry/opentelemetry-collector-releases/releases/download/v${OTELCOL_VERSION}/${deb}"

echo "Installing otelcol-contrib ${OTELCOL_VERSION} (${arch})"
curl -fsSL "$url" -o "/tmp/${deb}"
# The .deb installs /usr/bin/otelcol-contrib + its own
# otelcol-contrib.service. We disable that default unit and ship our own
# (otelcol.service) pointed at our config, so the image has exactly one
# collector with our pipeline.
sudo apt-get install -y "/tmp/${deb}"
rm -f "/tmp/${deb}"
sudo systemctl disable --now otelcol-contrib.service 2>/dev/null || true

# Our config (copied to /tmp by the file provisioner before this runs).
sudo mkdir -p /etc/otelcol
sudo install -m 0644 /tmp/collector-gcp.yaml /etc/otelcol/config.yaml

# Our systemd unit (also staged to /tmp by a file provisioner).
sudo install -m 0644 /tmp/otelcol.service /etc/systemd/system/otelcol.service
sudo systemctl daemon-reload
sudo systemctl enable otelcol.service

# Don't start at bake-time: the bake instance's SA may lack
# roles/cloudtrace.agent, and the collector would log export 403s. It
# starts cleanly on first boot of a real FC host. Validate the config
# instead so a typo fails the bake.
sudo otelcol-contrib validate --config /etc/otelcol/config.yaml

echo "otelcol-contrib installed; otelcol.service enabled (starts on boot)."
