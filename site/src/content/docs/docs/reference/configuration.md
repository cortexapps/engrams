---
title: Configuration
description: The environment variables the coordinator and the host agent run on, and how secrets, the master key, and the egress CA reach them.
sidebar:
  order: 4
---

The Helm charts set every variable on this page. The tables are the reference for what lands
where, for anyone reading a manifest or running a component outside the charts.

## Coordinator

| Variable | Purpose |
|---|---|
| `DATABASE_URL` | Postgres connection string, from the coordinator's Secret. |
| `ENGRAM_AUTH_TOKENS` | Comma-separated bearer tokens that hosts and the orchestrator present. |
| `ENGRAM_APP_GRPC_TOKENS` | The orchestrator's bearer for the coordinator's private RPC port. Empty means every call is refused. |
| `ENGRAM_BLOB_BACKEND` | `gcs`, `s3`, or `local` for development. |
| `ENGRAM_GCS_BUCKET` | With `gcs`: the chunks bucket. |
| `ENGRAM_S3_BUCKET`, `ENGRAM_S3_REGION`, `ENGRAM_S3_ENDPOINT_URL` | With `s3`: the bucket, the signing region, and an optional endpoint for S3-compatible stores such as MinIO or R2, which switches the client to path-style addressing. |
| `ENGRAM_KEK_PROVIDER` | `env-var` or `aws-kms`. |
| `ENGRAM_KEK_MASTER_KEY` | With `env-var`: the base64 32-byte master key. |
| `ENGRAM_KEK_AWS_KEY_ID` | With `aws-kms`: the KMS key id, ideally the ARN. |
| `ENGRAM_SECRETS_BACKEND` | `gcp`, `aws`, or `env` for development. |
| `ENGRAM_GCP_PROJECT_ID` | With `gcp`: the Secret Manager project. |
| `ENGRAM_CHUNK_GC_INTERVAL_SECS`, `ENGRAM_CHUNK_GC_RETAIN_SECS` | How often chunk garbage collection runs, and how long an unreferenced chunk is kept before deletion. |

Cloud credentials are ambient: Workload Identity on GKE, IRSA on EKS, bound to the
coordinator's ServiceAccount. There are no key files.

## Host agent

| Variable | Purpose |
|---|---|
| `ENGRAM_COORDINATOR_ENDPOINT` | `http://<coordinator Service>:8080`. Registration and heartbeats are HTTP POSTs. |
| `ENGRAM_COORDINATOR_TOKEN` | Must be one of the coordinator's `ENGRAM_AUTH_TOKENS`. |
| `ENGRAM_SANDBOX_BACKEND` | `firecracker` in production. |
| `ENGRAM_KERNEL_IMAGE_PATH` | The guest kernel staged on the node. |
| `ENGRAM_BLOB_BACKEND` and the bucket variables | Must match the coordinator's. |
| `ENGRAM_NBD_MAX_SLOTS`, `ENGRAM_NBD_WARM_SLOTS` | The NBD device pool. The node-prep step loads the module with `nbds_max` to match. |
| `ENGRAM_FC_UFFD_BASE_DIR` | The tmpfs that holds each image's canonical memory. |
| `ENGRAM_FC_VM_CGROUP_PARENT` | A node-level cgroup the VMs move into, so a host-agent pod restart does not kill them. |
| `ENGRAM_EGRESS_PROXY_PORT`, `ENGRAM_EGRESS_DNS_PORT`, `ENGRAM_GUEST_GATEWAY_PORT` | The proxy's listeners. Mandatory and non-zero; guest traffic is redirected to them. |
| `ENGRAM_EGRESS_CA_SOURCE` | Where the proxy's certificate authority comes from. See below. |
| `ENGRAM_WARM_STALL_SECS` | The stall budget for warm hooks during capture; default 120. |

## The master key

The coordinator envelope-encrypts registry credentials and per-session secret bundles under
a master key. On GCP the provider is `env-var`: a 32-byte key lives in Secret Manager and the
External Secrets relay places it in the coordinator's Secret. On AWS the provider is
`aws-kms`: the data keys are encrypted and decrypted by KMS under a customer-managed key, and
no key material reaches a pod.

A missing or wrong master key fails the coordinator at boot, on purpose. Encrypted rows are
bound to the provider that wrote them; there is no migration between providers, so the
choice is made when a deployment is created.

## Secret references

Secrets a session needs are resolved when the session is created, through the configured
backend, with the coordinator's ambient identity.

| Backend | Reference syntax | Default name when no reference is given |
|---|---|---|
| `gcp` | `gcp-sm://projects/<p>/secrets/<name>/versions/<v>` | `<repo with / replaced by ->--<NAME>` |
| `aws` | `aws-sm://<name-or-arn>[#version-stage]` | `engram/<repo>/<NAME>`, so an IAM policy can scope by prefix |

## The egress proxy's certificate authority

Every host intercepts guest TLS on port 443 to enforce the allow-list, so every host needs the
proxy's CA certificate and key, and every host in a fleet needs the same pair so a migrated
session keeps trusting its new host. How the pair reaches a host depends on the cloud, for one
reason: the host-agent pod shares the node's network namespace.

| `ENGRAM_EGRESS_CA_SOURCE` | When |
|---|---|
| `env` | GCP production. A pod on the node's network bypasses Workload Identity and runs as the node's scope-limited identity, so a direct Secret Manager read is refused. The External Secrets relay, which does have Workload Identity, syncs the PEMs into a Kubernetes Secret and the DaemonSet injects them as `ENGRAM_EGRESS_CA_CERT_PEM` and `ENGRAM_EGRESS_CA_KEY_PEM`. |
| `aws-secrets-manager` | AWS production. IRSA is file-based and works on the node's network, so the host agent reads the two secrets named in `ENGRAM_EGRESS_CA_AWS_CERT_SECRET` and `ENGRAM_EGRESS_CA_AWS_KEY_SECRET` itself. |
| `gcp-secret-manager` | Only where Workload Identity is honored, which the fleet's pods are not. |
| `local-disk` | Development on a single host. Never for a fleet: a session that migrates would not trust the new host's CA. |

Rotating the CA is a rolling redeploy of the host agents. The memory substrate's cache
invalidates on its own, because the CA's fingerprint is part of its file names.

## DNS and the one bypass

Guest DNS on port 53 is redirected to the proxy, which checks names against the same
allow-list as the TLS interception. The one transport-layer bypass is DNS over HTTPS: if a
profile allows a DoH endpoint such as `cloudflare-dns.com` or `dns.google`, guests can resolve
any name through it on port 443. The mitigation is operator-side. Do not allow-list one.

## Health endpoints

The coordinator serves two unauthenticated endpoints on port 8080. `/healthz` returns 200
whenever the process is up. `/readyz` returns 200 only when Postgres answers a `SELECT 1`,
and 503 otherwise. Wire the liveness probe to the first and the readiness probe to the
second.
