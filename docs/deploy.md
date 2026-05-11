# Production deployment notes

Operational guidance for running Engram on GCP. Code-level setup
only — Terraform, GKE manifests, IAM bindings, and image-build CI
are out of band.

## Topology

- **Coordinator**: stateless service, deployed to GKE as N replicas
  behind a single load balancer. Talks to Cloud SQL Postgres for
  durable state, GCS for cold-tier blobs, and GCP Secret Manager
  for per-session secret resolution.
- **FC host VMs**: pool of GCE instances, each running
  `engram-host-agent` with `--sandbox-backend=firecracker`. They
  dial the coordinator over WebSocket and serve as worker hosts
  for Firecracker microVMs.
- **Egress proxy**: runs on each FC host-agent (not the
  coordinator). All hosts share a single CA cert chain so guest
  substrates can trust the proxy's MITM leaves regardless of which
  host they end up on.

## Required env vars

### Coordinator

| Variable                       | Purpose                                                              |
|--------------------------------|----------------------------------------------------------------------|
| `ENGRAM_DATABASE_URL`          | Cloud SQL Postgres connection string                                 |
| `ENGRAM_AUTH_TOKENS`           | Comma-separated bearer tokens accepted on the API                    |
| `ENGRAM_KEK_MASTER_KEY`        | 32-byte master KEK, base64-encoded (see below)                       |
| `ENGRAM_CLOUD_BACKEND=gcp`     | Enables GCP-specific metadata-server probes                          |
| `ENGRAM_SANDBOX_BACKEND=firecracker` | Coordinator scheduling assumes FC hosts                        |
| `ENGRAM_BLOB_BACKEND=gcs`      | Cold-tier flush target                                               |
| `ENGRAM_GCS_BUCKET`            | Bucket name for snapshots / logs                                     |
| `ENGRAM_SECRETS_BACKEND=gcp`   | Selects `GcpSecretManager` (default project from metadata server)    |
| `ENGRAM_LOG_FORMAT=json`       | Structured logging for Cloud Logging ingestion                       |

The coordinator is otherwise stateless. The `local_path` directory
holds only the in-process OCI cache for `--mode=all` dev runs; in
production the cache lives on each host-agent.

### Host-agent

| Variable                       | Purpose                                                              |
|--------------------------------|----------------------------------------------------------------------|
| `ENGRAM_COORDINATOR_ENDPOINT`  | `https://<coordinator-lb>` — host-agent dials in over WS             |
| `ENGRAM_COORDINATOR_TOKEN`     | Must match an entry in the coordinator's `ENGRAM_AUTH_TOKENS`        |
| `ENGRAM_SANDBOX_BACKEND=firecracker` | Production backend on Linux                                    |
| `ENGRAM_KERNEL_IMAGE_PATH`     | Path to the vmlinux image baked into the FC host image               |
| `ENGRAM_LOG_FORMAT=json`       | Same as coordinator                                                  |

## KEK (key-encryption key)

The coordinator uses a master KEK to envelope-encrypt registry
credentials (`registry_credentials` table) and per-session secret
bundles (`session_secrets` table). Sourcing path:

1. The 32-byte master key is generated once and stored in GCP Secret
   Manager (e.g., `secret_id = engram-kek-master-v1`). Generate with
   `openssl rand -base64 32`.
2. A k8s Secret in the coordinator's namespace pulls the value via
   the Secret Store CSI driver or External Secrets Operator.
3. The k8s Secret is projected into the coordinator pod as the env
   var `ENGRAM_KEK_MASTER_KEY` (base64-encoded).
4. On boot the coordinator reads the env var. **If it's missing,
   the coordinator fails to start** (`crates/engram-coordinator/src/main.rs:227-232`)
   — this is intentional; running with a missing KEK would silently
   produce undecryptable rows on the next write.

### Rotation (out of v1 scope)

The current `key_id()` format hardcodes `:v1`
(`crates/engram-crypto/src/providers.rs`). Rotating the master key
would require redeploying with a new env var value **and**
re-encrypting any stored ciphertexts under the new key. Neither
the rewrap loop nor the multi-version key lookup exists yet — flag
for follow-up before any rotation is attempted in production.

### Why not GCP KMS?

The `GcpKmsProvider` is a stub (see `crates/engram-crypto/src/providers.rs:159-175`).
For v1 we chose env-var KEK over implementing KMS wrap/unwrap
because:

- The wrapped data is small (registry passwords, OAuth tokens) and
  unwrap happens at session-create time — KMS round-trip latency
  would add a non-trivial fraction to that.
- Secret Manager + Workload Identity provides equivalent at-rest
  protection for the master key itself; KMS would only add HSM
  ceremony, not a stronger security boundary against the threats
  in scope (cluster compromise, snapshot leak).
- Implementing it later is a self-contained drop-in via the
  `MasterKeyProvider` trait.

If you eventually want HSM-backed unwrap (audit logs per decrypt,
no plaintext key in pod memory), implement `wrap` / `unwrap` in
`providers.rs` against `google-cloud-kms` and flip
`--kek-provider gcp-kms`.

## Egress proxy CA

The egress proxy (run by each host-agent) MITMs guest TLS so
per-secret `allow_hosts` policies can be enforced and broker-mode
placeholder substitution can run. For this to work, every guest
substrate must trust the proxy's CA, which means all host-agents
must MITM with the same chain.

| Variable                          | Purpose                                                |
|-----------------------------------|--------------------------------------------------------|
| `ENGRAM_EGRESS_CA_CERT_PEM`       | Base64-encoded CA cert PEM, sourced from Secret Manager |
| `ENGRAM_EGRESS_CA_KEY_PEM`        | Base64-encoded CA private key PEM, ditto                |
| `ENGRAM_EGRESS_PROXY_PORT`        | Listener port on the FC host's tap interface           |

Sourcing path is the same shape as the KEK: generate once, store
in Secret Manager, inject through a k8s Secret (or GCE metadata
secret) at boot.

## Secret resolution

Per-image `[secrets.*]` entries resolve via `engram-secrets-gcp`,
which calls `secretmanager.googleapis.com/v1/{path}:access` per
secret at session-create time. Authentication is via Workload
Identity (the coordinator's k8s SA mapped to a GCP SA with
`roles/secretmanager.secretAccessor`).

The resolver supports two reference styles
(`crates/engram-secrets-gcp/src/lib.rs:7-19`):

- Explicit `ref = "gcp-sm://projects/.../secrets/.../versions/..."`
- Implicit namespacing: a manifest for repo `cortex/api` needing
  `GITHUB_TOKEN` resolves to `cortex--api--GITHUB_TOKEN`
  (`/` → `--` because Secret Manager keys disallow `/`).

For v1, image manifests should declare `secret_mode = "literal"`.
The broker mode (placeholder substitution via the egress proxy) is
deferred — substitution logic is complete in
`crates/engram-egress-proxy/src/substitute.rs`, but the
coordinator-to-proxy registration step is not yet wired.

## Liveness / readiness

- `/healthz` (port 8080, no auth) — process-up check.
  Returns 200 with version JSON unconditionally.
- `/readyz` (port 8080, no auth) — readiness check.
  Returns 200 if the metadata store responds to `SELECT 1`, else 503.

Configure the K8s `livenessProbe` against `/healthz` and the
`readinessProbe` against `/readyz`. The readiness probe gates LB
traffic; a coordinator that lost its DB connection drops out
instead of returning 503s to clients.
