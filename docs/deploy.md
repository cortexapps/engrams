# Production deployment

How a production engram deployment is shaped, and the knobs it runs
on. For the step-by-step bring-ups see
[`deploy-gcp.md`](./deploy-gcp.md) and [`deploy-aws.md`](./deploy-aws.md).

The **deployment artifacts**:
- [`deploy/helm/engram/`](../deploy/helm/engram/) — the control-plane
  chart (coordinator + web + orchestrator)
- [`deploy/helm/engram-host-fleet/`](../deploy/helm/engram-host-fleet/)
  — the Firecracker host fleet: DaemonSet + rollout/
  autoscaling operator + `HostFleet` CRD
- [`deploy/terraform/`](../deploy/terraform/) — per-cloud modules +
  quickstarts (GCP and AWS)

## Topology

Everything runs in one Kubernetes cluster, two Helm releases:

- **Coordinator** — a stateless Deployment (N replicas:
  Postgres is the only authority; every background task is
  lease-guarded, so replicas don't multiply work). Talks to Postgres
  (Cloud SQL / RDS), the blob store (GCS / S3) for the
  content-addressed chunk tier, and the cloud secret manager for
  per-session secret resolution. Runs `--mode=coordinator`; never
  owns a VMM.
- **Web + orchestrator** — the same release. The orchestrator
  (Bun/Hono) owns human auth (better-auth; the login wall
  is the deployment's auth door) and fronts the
  coordinator's app-gRPC. nginx serves the SPA and proxies API calls
  in-cluster.
- **FC host fleet** — a privileged DaemonSet (`hostNetwork`,
  `hostPID`) on a dedicated **Intel nested-virt node pool**, in a
  namespace enforcing the `privileged` Pod Security level. A
  node-prep DaemonSet loads the NBD module, sets the sysctls, and
  mounts the substrate tmpfs; a node-assets init container stages
  firecracker + the guest kernel + the RO session bundles.
  `updateStrategy: OnDelete` on purpose — Kubernetes never rolls
  these pods; the **operator** does, drain-gated and node-by-node. Rolls REATTACH to running VMs (the FC processes
  escape the pod cgroup); they do not evacuate sessions.
- **The operator** (same fleet release) also owns autoscaling: the coordinator's PG-derived demand signal drives
  `set_size` grow / drain-then-`remove_node` shrink through a
  per-cloud `NodePoolScaler` (`gke` | `asg`). The node pool's size is
  operator-owned — Terraform seeds it and `ignore_changes` it; never
  attach a cluster autoscaler to the KVM pool.
- **Egress proxy** — per node: tcp/443 TLS-MITM +
  filtering DNS, run as a node-local daemon that survives pod rolls.
  Every host shares ONE CA pair so migrated sessions keep trusting
  their new host.
- **Registration is HTTP**: host-agents POST register/
  heartbeat to the coordinator's in-cluster ClusterIP Service. There
  is no WebSocket dial and no internal LB in the in-cluster topology
  (`serviceInternal.enabled: false` in the cloud overlays).

### The node-pool constraints (read before "fixing" anything)

- Nested virtualization is **Intel-only** on every managed provider,
  set at node-pool creation, and incompatible with node
  auto-provisioning. GKE: the C3 family (Standard clusters only).
  EKS: the 8th-gen C8i/M8i/R8i virtual shapes (nested virtualization
  is a launch-time flag the quickstart sets), or bare metal
  (`*.metal`).
- **CPUID is a one-way door for snapshots**: images baked on a newer
  CPU platform never restore on an older one. The quickstarts default
  GCP to Sapphire Rapids (C3) and AWS to Granite Rapids (m8i); the
  AWS `m7i.metal-24xl` option gives CPUID parity when one bake must
  serve both clouds. Moving a fleet to an older platform means
  re-baking every enabled image.
- The pool's taint/label pair (`engram.io/kvm=true`), the PSA-
  privileged namespace, `auto_upgrade off`, and the operator-owned
  size are invariants, not preferences — each has an incident behind
  it (see the gke-kvm-pool / kvm-nodegroup module headers).

## Required env vars

The charts set all of these; the tables are the reference for what
lands where.

### Coordinator

| Variable | Purpose |
|---|---|
| `DATABASE_URL` | Postgres connection string (from the coordinator Secret) |
| `ENGRAM_AUTH_TOKENS` | Comma-separated machine bearer allow-list (host-agents + app-gRPC default) |
| `ENGRAM_BLOB_BACKEND` | `gcs` \| `s3` (\| `local` dev) |
| `ENGRAM_GCS_BUCKET` | gcs: the chunks bucket |
| `ENGRAM_S3_BUCKET` / `ENGRAM_S3_REGION` / `ENGRAM_S3_ENDPOINT_URL` | s3: bucket, SigV4 region, optional S3-compatible endpoint (MinIO/R2 — flips path-style) |
| `ENGRAM_KEK_PROVIDER` | `env-var` \| `gcp-kms` (stub) \| `aws-kms` (real) |
| `ENGRAM_KEK_MASTER_KEY` | env-var provider: the base64 32-byte master key |
| `ENGRAM_KEK_AWS_KEY_ID` | aws-kms provider: the KMS KeyId (ARN preferred) |
| `ENGRAM_SECRETS_BACKEND` | `gcp` \| `aws` (\| `env` dev) |
| `ENGRAM_GCP_PROJECT_ID` | gcp secrets backend: the Secret Manager project |
| `ENGRAM_CHUNK_GC_INTERVAL_SECS` / `ENGRAM_CHUNK_GC_RETAIN_SECS` | chunk-GC cadence / unreferenced retention |
| `ENGRAM_APP_GRPC_TOKENS` | The orchestrator's bearer (fail-closed when empty) |

Cloud credentials are ambient: GKE Workload Identity / EKS IRSA on
the coordinator's ServiceAccount — no key files.

### Host-agent (the fleet chart's ConfigMap/DaemonSet)

| Variable | Purpose |
|---|---|
| `ENGRAM_COORDINATOR_ENDPOINT` | `http://<coord ClusterIP Service>:8080` — registration + heartbeats are HTTP POSTs |
| `ENGRAM_COORDINATOR_TOKEN` | Must match an `ENGRAM_AUTH_TOKENS` entry |
| `ENGRAM_SANDBOX_BACKEND=firecracker` | Production backend |
| `ENGRAM_KERNEL_IMAGE_PATH` | The node-assets-staged vmlinux |
| `ENGRAM_BLOB_BACKEND` + bucket vars | Must match the coordinator's chunk store |
| `ENGRAM_NBD_MAX_SLOTS` / `ENGRAM_NBD_WARM_SLOTS` | The NBD slot allocator (module loaded by node-prep with `nbds_max`) |
| `ENGRAM_FC_UFFD_BASE_DIR` (+ the lazy-memory knobs) | The memory substrate tmpfs |
| `ENGRAM_FC_VM_CGROUP_PARENT` | VMs escape the pod cgroup so rolls don't kill them |
| `ENGRAM_EGRESS_PROXY_PORT` / `ENGRAM_EGRESS_DNS_PORT` / `ENGRAM_GUEST_GATEWAY_PORT` | Mandatory listeners (non-zero; iptables REDIRECT targets) |
| `ENGRAM_EGRESS_CA_SOURCE` | `env` \| `gcp-secret-manager` \| `aws-secrets-manager` \| `local-disk` — see below |

## KEK (key-encryption key)

The coordinator envelope-encrypts registry credentials and
per-session secret bundles under a master KEK. Provider per cloud:

- **GCP: `env-var`.** The 32-byte base64 key lives in Secret Manager
  and is relayed into the coordinator Secret's
  `ENGRAM_KEK_MASTER_KEY` by External Secrets. (`gcp-kms` is a stub.)
- **AWS: `aws-kms`.** Real KMS `Encrypt`/`Decrypt` of the DEKs under
  a customer-managed key (`ENGRAM_KEK_AWS_KEY_ID`); no key material
  in the pod.

A missing/misconfigured KEK **fails the coordinator at boot** on
purpose — running without it would write undecryptable rows. Sealed
data is provider-bound via `key_id`; there is no migration between
providers (a new-deployment decision, not a config flip).

## Secret resolution

Per-image `[secrets.*]` entries resolve at session-create time via
the configured backend:

- **`gcp`** — Secret Manager. Refs `gcp-sm://projects/.../versions/...`
  or default namespacing `<repo with / → -->--<NAME>`.
- **`aws`** — AWS Secrets Manager. Refs
  `aws-sm://<name-or-arn>[#version-stage]` or default namespacing
  `engram/<repo>/<NAME>` (Secrets Manager allows `/`, so IAM prefix
  policies scope cleanly).

Auth is ambient (WI / IRSA) on the coordinator's ServiceAccount.

## Egress proxy

The per-node proxy MITMs guest TLS so per-image
`[network].allow_hosts` gates run; filtering DNS REDIRECTs guest
udp+tcp/53. It is mandatory — a host that cannot load the
CA or bind the listeners refuses to start.

### CA distribution — the identity asymmetry that decides the source

Every host loads the SAME CA pair (a migrated session must trust its
new host). The loading path differs per cloud because of one fact:
the host-agent pod is `hostNetwork`.

| `--ca-source` | When |
|---|---|
| `env` | **GCP production.** hostNetwork pods BYPASS GKE Workload Identity (a metadata intercept) and run as the scope-limited node SA — a direct Secret Manager read 403s. External Secrets (proper WI) syncs the PEMs into a K8s Secret; the DaemonSet injects them as `ENGRAM_EGRESS_CA_{CERT,KEY}_PEM`. |
| `aws-secrets-manager` | **AWS production.** IRSA is env/file-based and WORKS in hostNetwork pods, so the host-agent reads the two SecretIds (`ENGRAM_EGRESS_CA_AWS_{CERT,KEY}_SECRET`) directly. No relay. |
| `gcp-secret-manager` | Only where WI is honoured (NOT the hostNetwork fleet). |
| `local-disk` | Dev / single host. Never for a migrating fleet — per-host CAs break teleported sessions' trust. |

CA rotation is a rolling redeploy of host-agents; the substrate
cache invalidates automatically (its filename includes the CA
fingerprint).

### DNS filtering + the DoH caveat

Guest DNS is REDIRECTed to the proxy, which checks QNAMEs against
the same allow-list as the TLS SNI peeker. The one
transport-layer bypass is **DNS-over-HTTPS**: if an operator allows
a DoH endpoint (`cloudflare-dns.com`, `dns.google`, …) in any
manifest's `allow_hosts`, guests can resolve through tcp/443.
Mitigation is operator-side: don't allow-list DoH endpoints.

## Liveness / readiness

- `/healthz` (8080, no auth) — process-up; 200 unconditionally.
- `/readyz` (8080, no auth) — 200 iff the metadata store answers
  `SELECT 1`, else 503.

Wire `livenessProbe` → `/healthz` and `readinessProbe` → `/readyz`;
a coordinator that lost its DB drops out of the LB instead of
serving 503s.
