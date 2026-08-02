# Production deployment notes

Operational guidance for running Engram on GCP. Code-level setup
only.

For the **actual deployment artifacts** see:
- [`deploy/helm/engram/`](../deploy/helm/engram/) — Helm chart (coordinator + optional web frontend)
- [`deploy/packer/fc-host-gcp.pkr.hcl`](../deploy/packer/fc-host-gcp.pkr.hcl) — host image
- [`deploy/terraform/gcp/`](../deploy/terraform/gcp/) — modules + minimal example
- [`docs/chunked-storage-rollout.md`](./chunked-storage-rollout.md) — tier-laddered rollout tracker

ADR 0007's chunked-immutable storage rolled out through Phase 7;
the cold-tier flush pipeline is retired. The env vars + topology
below are accurate; the storage layout below is updated to
reflect chunks-in-`BlobStorage` rather than the legacy "hot tier
+ tar.zst sealed-blob cold tier" framing.

## Topology

- **Coordinator**: stateless service, deployed to GKE as N replicas
  behind a single load balancer. Talks to Cloud SQL Postgres for
  durable state, GCS for the chunk store (content-addressed
  manifests + chunks), and GCP Secret Manager for per-session
  secret resolution. Runs `--mode=coordinator`; never owns a local
  VMM.
- **FC host VMs**: pool of GCE instances, each running
  `engram-host-agent` with `--sandbox-backend=firecracker`
  (= `--mode=host`). They dial the coordinator over WebSocket and
  serve as worker hosts for Firecracker microVMs. The wire surface
  is `HostClient` (ADR 0011): `LocalHostClient` runs in-process on
  each host, `RemoteHostClient` is what the coord sees.
- **Egress proxy**: runs on each FC host-agent (not the
  coordinator). Three listeners on one process: tcp/443 (TLS-MITM,
  ADR 0006), udp/5353 + tcp/5353 (filtering DNS, ADR 0010). All
  hosts share a single CA cert chain so guest substrates can trust
  the proxy's MITM leaves regardless of which host they end up on.

## Required env vars

### Coordinator

| Variable                       | Purpose                                                              |
|--------------------------------|----------------------------------------------------------------------|
| `ENGRAM_DATABASE_URL`          | Cloud SQL Postgres connection string                                 |
| `ENGRAM_AUTH_TOKENS`           | Comma-separated bearer tokens accepted on the API                    |
| `ENGRAM_KEK_MASTER_KEY`        | 32-byte master KEK, base64-encoded (see below)                       |
| `ENGRAM_CLOUD_BACKEND=gcp`     | Enables GCP-specific metadata-server probes                          |
| `ENGRAM_SANDBOX_BACKEND=firecracker` | Coordinator scheduling assumes FC hosts                        |
| `ENGRAM_BLOB_BACKEND=gcs`      | Backing storage for the chunk store (chunks + manifests + traces)    |
| `ENGRAM_GCS_BUCKET`            | Bucket name for chunks + manifests                                   |
| `ENGRAM_CHUNK_GC_INTERVAL_SECS`| GC sweep cadence (default 3600). `0` disables the cron driver.       |
| `ENGRAM_CHUNK_GC_RETAIN_SECS`  | Unreferenced-chunk retention before sweep (default 86400)            |
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
| `ENGRAM_BLOB_BACKEND=gcs`      | Must match the coord's chunk-store backend                           |
| `ENGRAM_GCS_BUCKET`            | Same bucket as the coord; host materialises chunks on demand         |
| `ENGRAM_CHUNK_CACHE_BUDGET_BYTES` | NVMe-backed LRU chunk cache budget (default 200 GiB)              |
| `ENGRAM_NBD_DEVICES`           | `/dev/nbd0,/dev/nbd1,…` for the chunked-disk NBD daemon. Empty / unset falls back to materialize-to-file (correct, slower cold start). |
| `ENGRAM_EGRESS_PROXY_PORT`     | Mandatory TCP listener for the host-side TLS-MITM proxy (default `8443`). Must be non-zero; iptables REDIRECTs guest tcp/443 here. |
| `ENGRAM_EGRESS_DNS_PORT`       | Mandatory UDP+TCP listener for filtering DNS (default `5353`). Must be non-zero and match the guest DNS REDIRECT target. |
| `ENGRAM_EGRESS_METADATA_PORT`  | Mandatory TCP listener for session-scoped Google metadata ADC (default `13338`). Must be non-zero and match the guest metadata REDIRECT target. |
| `ENGRAM_EGRESS_CA_SOURCE`      | `local-disk` (default) \| `env` \| `gcp-secret-manager`. See the Egress proxy section below for the per-source vars. |
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

## Egress proxy

The egress proxy MITMs guest TLS so per-image `[network].allow_hosts`
gates and broker-mode placeholder substitution can run. The proxy
**runs on each FC host-agent**, not on the coordinator (ADR 0006).
iptables PREROUTING REDIRECT on each FC host hands tcp/443 to the
local proxy; no cross-machine traffic enters the request path.

### CA distribution

All host-agents in a deployment load the **same CA cert + key** so
guest substrates that trust one host's chain trust them all. The
loader is pluggable (`engram_egress_proxy::CaSource` trait); three
impls ship today, with one more impl per cloud later:

| `--ca-source`        | When to use                                                    |
|----------------------|----------------------------------------------------------------|
| `local-disk` (default) | Dev / single-host. Auto-generates `<work_dir>/egress-ca/{ca.pem,ca.key}` on first boot. |
| `env`                | Process env carries the PEMs (k8s Secret CSI mount, GCE startup script writing systemd Environment). |
| `gcp-secret-manager` | Production. Workload Identity → Secret Manager. Two named secret paths, fetched at host-agent boot. |

### Host-agent env vars

| Variable                              | Purpose                                                              |
|---------------------------------------|----------------------------------------------------------------------|
| `ENGRAM_EGRESS_PROXY_PORT`            | Mandatory TCP listener port (default `8443`; must be non-zero)       |
| `ENGRAM_EGRESS_DNS_PORT`              | Mandatory filtering DNS port (default `5353`; must be non-zero)      |
| `ENGRAM_EGRESS_METADATA_PORT`         | Mandatory Google metadata ADC port (default `13338`; must be non-zero) |
| `ENGRAM_EGRESS_CA_SOURCE`             | `env` \| `local-disk` \| `gcp-secret-manager`                        |
| `ENGRAM_EGRESS_CA_CERT_VAR`           | Env var name holding the cert PEM (`--ca-source=env`)                |
| `ENGRAM_EGRESS_CA_KEY_VAR`            | Env var name holding the key PEM (`--ca-source=env`)                 |
| `ENGRAM_EGRESS_CA_DIR`                | Directory for `--ca-source=local-disk`                               |
| `ENGRAM_EGRESS_CA_GCP_CERT_SECRET`    | `projects/<p>/secrets/<name>/versions/<v>` for the cert PEM          |
| `ENGRAM_EGRESS_CA_GCP_KEY_SECRET`     | Same shape for the key PEM                                           |

Production deploy on GCP:

1. Generate the CA pair once (`openssl req -x509 -days 3650 ...`).
2. Store both PEMs in GCP Secret Manager (e.g.
   `engram-egress-ca-cert` + `engram-egress-ca-key`).
3. Bind the FC host VM's service account to those secrets with
   `roles/secretmanager.secretAccessor`.
4. Set `ENGRAM_EGRESS_CA_SOURCE=gcp-secret-manager` plus the two
   `_SECRET` paths on each FC host's systemd unit.

The coordinator never sees the CA. CA rotation is a rolling
redeploy of host-agents (and a substrate-cache invalidation, which
is automatic: the cache filename includes the CA fingerprint).

### Per-session policy delivery

The coordinator ships `SessionEgressPolicy` (network allow-hosts
+ per-secret placeholder→real_value keyring + `SecretMode`) over
the existing coordinator↔host WS as a notify frame. The host-agent
registers it with its local proxy registry (which the DNS filter
and the TCP/443 SNI peeker both read). WS-frame ordering guarantees
the policy lands before the subsequent `start_agent` request, so
the harness can't make egress calls before the proxy knows the
policy. Cold resume re-issues the policy when a session moves hosts.

Egress filtering cannot be disabled per host. Both listener ports must be
non-zero because Firecracker iptables rules redirect guest `:443` and `:53`
to those literal ports. The host-agent rejects `0` and fails startup if either
listener cannot bind, keeping an unusable host out of the schedulable fleet.
Tilt uses `18443`/`15353` for local VZ so it does not collide with retained
Colima SSH forwards or macOS mDNS; Firecracker and production retain the binary
defaults above.

### DNS filtering

When the egress proxy is enabled, all guest DNS (both `udp/53` and
`tcp/53`, regardless of which upstream IP the guest's resolv.conf
points at) is iptables-REDIRECTed to the proxy. The proxy parses
the query, checks the QNAME against the same `network.allow_hosts`
list the TCP/443 SNI peeker uses, and either forwards to 1.1.1.1
or responds with `NXDOMAIN`. The default DNS port is `5353` so
the host's own `systemd-resolved` (on `127.0.0.53:53`) keeps
working for the host's own name resolution. See
[ADR 0010](./adr/0010-dns-filtering.md) for the architecture and
the DNS-over-HTTPS residual gap.

The one out-of-band exfil channel that the proxy can't close at
the transport layer is **DNS-over-HTTPS**. If an operator allows
a DoH endpoint (`cloudflare-dns.com`, `dns.google`,
`doh.opendns.com`, …) in any manifest's `allow_hosts`, the guest
can drive resolution via tcp/443 and bypass the DNS filter
entirely. Mitigation is operator-side: don't include known DoH
endpoints in `allow_hosts`.

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

## Operational notes

### Idle auto-eviction in `--mode=coordinator`

The host-agent now runs its own `HarnessHub` (ADR 0011 splits the
coord↔host boundary; the hub lives wherever its connections terminate),
so the routing path that fed the idle evictor in `--mode=all` is now
present in split-mode too. The evictor itself still polls
`state.harness_hub.idle_sandboxes(...)` on the coord side — the small
shim of "ask each connected host for its idle candidates over WS" is
the only remaining piece; tracked in `docs/known-issues.md` #8 with a
clearly-scoped fix. For now, in `--mode=coordinator` the evictor runs
and produces no candidates (harmless); sessions linger until their hard
TTL or explicit `DELETE /sessions/:id`. The ADR 0005 cold-tier flush
endpoints retired in Phase 7 when chunked-immutable storage became the
single durability primitive.

### Chunk-store GC — REMOVED 2026-05-23

The chunk-store GC and its admin endpoint were deleted after a
prod incident reaped freshly-enabled-image chunks before any host
could prefetch them. BlobStorage cost grows unbounded until a
redesigned sweep ships; see ADR 0015 M5 "Known regression —
chunk-store GC deleted" for the post-mortem and design
constraints for any re-implementation.

### Materialize-dir reap

Per-host `<work_dir>/chunked-rootfs/<manifest_id>-vN.ext4`
files are reaped by
`POST /api/admin/reap-materialize-dir?min_age_secs=<N>`. In
`--mode=coordinator` the endpoint fans out across every
connected host via WS-RPC (WIRE v3); each host sweeps its
own local dir and returns per-host stats.

## Liveness / readiness

- `/healthz` (port 8080, no auth) — process-up check.
  Returns 200 with version JSON unconditionally.
- `/readyz` (port 8080, no auth) — readiness check.
  Returns 200 if the metadata store responds to `SELECT 1`, else 503.

Configure the K8s `livenessProbe` against `/healthz` and the
`readinessProbe` against `/readyz`. The readiness probe gates LB
traffic; a coordinator that lost its DB connection drops out
instead of returning 503s to clients.
