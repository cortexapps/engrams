---
title: "Terraform: GCP"
description: The composable modules and the one-apply quickstart for GKE.
sidebar:
  order: 7
---

`deploy/terraform/gcp` is the GCP deployment surface: composable modules plus a quickstart
root that wires all of them. Use the quickstart for a fresh project, and compose the modules
yourself when you bring your own cluster or database. The step-by-step bring-up, including
the manual steps Terraform leaves to you on purpose, is [Deploy on GCP](../../guides/deploy-gcp/).

## What every cloud must provide

engrams asks five things of a cloud.

1. **Postgres 14 or later** with `LISTEN/NOTIFY`: Cloud SQL, RDS, or your own.
2. **Object storage** for chunks: GCS, S3, or an S3-compatible store through the endpoint
   override.
3. **A secret store** for session secrets and the egress CA: Secret Manager or AWS Secrets
   Manager, or the environment-variable arms plus a relay that lands Kubernetes Secrets.
4. **Linux nodes with KVM** for the hosts. This is the binding constraint: nested
   virtualization is Intel-only on every managed provider, set when the pool is created, and
   the CPU platform is a one-way door for snapshots.
5. **Kubernetes** for the control plane and the host DaemonSet.

A new cloud implements the contract by mirroring this layout: its own network and storage
modules, a KVM node-pool module that honors the invariants above, a scaler for the
autoscaling operator, and a quickstart that wires them. The Helm charts are already
cloud-agnostic; only a values overlay is per-cloud.

## Layout

```
modules/
  network/            VPC, subnet, Cloud NAT, firewall
  storage/            the GCS chunks bucket
  fc-host-gsa/        the hosts' Google service account (identity only)
  gke-cluster/        a regional cluster: Workload Identity, Managed Prometheus,
                      VPC-native, plus the small control-plane pool
  gke-kvm-pool/       THE nested-virt host pool: Intel-only, operator-owned size,
                      the CPU one-way door. Read its header before touching it.
  cloudsql/           Postgres 16 on a private IP, both databases, connection
                      strings in Secret Manager
  secret-shells/      empty Secret Manager shells for the operator-populated secrets:
                      master key, auth tokens, session signing, egress CA.
                      Material never enters Terraform state.
  host-operator-iam/  the autoscaling operator's least-privilege role and account
quickstart/           ONE apply from a fresh project to an engrams-ready cluster:
                      all of the above plus namespaces, the External Secrets relay,
                      identity bindings, the web static IP and managed certificate,
                      and rendered Helm values as outputs
examples/
  minimal/            the bring-your-own-cluster shape: VPC, bucket, KMS, and
                      identities only
```

## Quickstart

```sh
cd deploy/terraform/gcp/quickstart
terraform init
terraform apply \
  -var project_id=my-project \
  -var region=us-west2 \
  -var domain=engrams.example.com \
  -var admin_email=you@example.com
```

Pass `-var dns_zone_name=<zone>` to have the A record created in Cloud DNS. The outputs you
use next are `secret_shell_ids` (the shells to populate), `web_static_ip` (the A record if you
create it by hand), `cluster_name`, and the two rendered values overlays:

```sh
terraform output -raw engram_values      > engram.tfvalues.yaml
terraform output -raw host_fleet_values  > host-fleet.tfvalues.yaml
```

Mind the order: the External Secrets relay syncs only after the shells are populated, and a
Helm release installed before that crash-loops on missing Secrets until the first sync.

## Bring your own cluster or database

Compose the modules directly; each stands alone. `examples/minimal/` is the smallest useful
set. Add `gke-kvm-pool` against your existing cluster and `cloudsql`, or any Postgres 14 or
later, as needed. The published Google modules for GKE and Cloud SQL compose fine with these;
the engrams modules carry the engrams-specific invariants, not a re-rolled cloud.

`modules/network`, `modules/storage`, and `modules/fc-host-gsa` are consumed by git reference
from other deployments, so their paths and variables are frozen. New modules are additive.
