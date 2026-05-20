#!/usr/bin/env bash
#
# Installs Google Cloud Ops Agent on the FC host image so the
# `engram-host-agent.service` systemd journal output is shipped
# to Cloud Logging under `resource.type="gce_instance"` with
# `jsonPayload._SYSTEMD_UNIT="engram-host-agent.service"`.
#
# Background: pre-Ops-Agent, `journalctl -u engram-host-agent` on
# the host was the only way to read host-agent logs. That required
# sudo SSH access to every FC host VM. Cloud Logging (where coord
# + web pod logs already live) was missing the host-agent stream,
# which made cross-host post-mortems painful and made any
# investigation that needed sudo on the VM gated on having the
# right org-side IAM.
#
# After this lands and host VMs roll, host-agent logs are queryable
# via:
#
#   gcloud logging read 'resource.type="gce_instance" AND \
#       jsonPayload._SYSTEMD_UNIT="engram-host-agent.service" AND \
#       textPayload=~"<your search>"' \
#       --project=cortex-internal-tooling \
#       --freshness=1h
#
# Or via the prod-ops skill once `logs-host.sh` is updated to
# prefer Cloud Logging over `gcloud compute ssh ... journalctl`.

set -euo pipefail

# Official install script. Pinned to the `add-google-cloud-ops-agent-repo`
# bootstrap that adds the GCP package repo + installs the
# `google-cloud-ops-agent` package. `--also-install` triggers the
# apt install in one go.
#
# We don't pin a specific Ops Agent version: Google patches this
# frequently for security + bug fixes, the default config we ship
# is stable across versions, and the runtime config gets
# re-applied on every restart so package upgrades don't reset
# anything operator-side.
curl -fsSL \
    https://dl.google.com/cloudagents/add-google-cloud-ops-agent-repo.sh \
    -o /tmp/add-ops-agent-repo.sh
sudo bash /tmp/add-ops-agent-repo.sh --also-install
rm -f /tmp/add-ops-agent-repo.sh

# Explicit logging config. The default Ops Agent config already
# tails the systemd journal as `default_pipeline`, but we override
# with a named receiver so:
#   1. The `engram_host_agent_journal` label is searchable in
#      Cloud Logging under `logName`, which makes a "show me all
#      host-agent output across the fleet" query a one-liner.
#   2. Operators reading this config a year from now don't have
#      to guess whether the host-agent stream is going somewhere.
#
# `_SYSTEMD_UNIT` filtering at receiver time (vs. query time)
# would cut log volume — but the FC host VM is single-purpose
# (only engram-host-agent + system daemons), so dragging in the
# kernel + systemd journal too is cheap and useful for booting /
# OOM / TAP / iptables forensics. We pay maybe a few MB/day per
# host in Cloud Logging ingestion.
sudo mkdir -p /etc/google-cloud-ops-agent
sudo tee /etc/google-cloud-ops-agent/config.yaml >/dev/null <<'YAML'
# Engram FC host Ops Agent config — baked into the GCE image by
# deploy/packer/provisioners/install-ops-agent.sh.
#
# Ships the full systemd journal (host-agent + system daemons +
# kernel) to Cloud Logging. Query host-agent specifically via:
#   jsonPayload._SYSTEMD_UNIT="engram-host-agent.service"
logging:
  receivers:
    engram_host_journal:
      type: systemd_journald
  service:
    pipelines:
      engram_host_pipeline:
        receivers:
          - engram_host_journal

# Metrics receivers stay on Ops Agent defaults (host metrics +
# process metrics). Add custom receivers here if/when we want
# host-agent-specific Prometheus scrape into Cloud Monitoring.
metrics:
  service:
    pipelines:
      default_pipeline:
        receivers:
          - hostmetrics
YAML

# Restart so the new config takes effect immediately on the next
# boot (the agent picks the config up from the file on its
# startup, but a service restart at bake-time is cheap and
# catches typos before the image gets baked).
sudo systemctl enable google-cloud-ops-agent.service
sudo systemctl restart google-cloud-ops-agent.service

# Sanity: print the agent's status so the Packer log captures
# whether the agent is healthy in the baked image. `--no-pager`
# keeps the output flat; `|| true` so an active-but-degraded
# state (e.g. test instance has no WIF for live ingestion)
# doesn't fail the bake.
sudo systemctl status google-cloud-ops-agent.service --no-pager --lines=10 || true

echo "Ops Agent installed and configured for engram-host-agent journal."
