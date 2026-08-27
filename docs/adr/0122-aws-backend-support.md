# ADR 0122: AWS backend support — S3, ASG autoscaling, Secrets Manager, KMS, ECR, and per-cloud deploy templates

Status: Proposed (2026-08-27)

Terms used in this document:

- **Backend arm**: one selectable implementation behind an existing
  trait seam (`BlobStorage`, `NodePoolScaler`, `SecretStore`,
  `MasterKeyProvider`, registry auth strategy). The coordinator and
  host-agent select arms with flags and env vars at startup.
- **IRSA**: IAM Roles for Service Accounts — EKS's pod identity. A
  webhook injects `AWS_WEB_IDENTITY_TOKEN_FILE` and a projected token
  into the pod. It is file/env based, not a metadata-server intercept.
- **Quickstart**: a per-cloud Terraform root module in this repo that
  takes a fresh cloud account to a running engrams deployment in one
  apply, plus documented manual steps.
- **The crypto trap**: this workspace pins rustls to the `ring`
  provider (`engram-tls` installs it as THE process provider). A
  dependency that links `aws-lc-rs` adds a second provider; rustls
  then panics at first TLS use, and the aws-lc C build breaks the
  musl cross lane.

## Summary, in plain English

engrams deploys only on GCP today. To open-source it, an outside
operator must be able to run it on AWS, and the GCP path must not
require our private deploy repo. The code already has the right
seams: every cloud touchpoint is a trait with a GCP arm and, in two
cases, a prepared stub (`engram-storage-s3`, the reserved `s3` blob
backend value). The autoscaling operator (ADR 0044/0048) is cloud
agnostic and needs only a new actuator.

This ADR adds the AWS arms — S3 blob storage, an ASG node-pool
scaler, Secrets Manager secrets, a real KMS KEK provider, ECR
registry auth — and the deploy surface that makes both clouds simple:
promoted Terraform modules (the KVM node pool, cluster prerequisites,
database, secrets, operator IAM), a per-cloud quickstart root module,
and Helm values examples that install cleanly from public GHCR
images. Two how-to documents (`docs/deploy-gcp.md`,
`docs/deploy-aws.md`) are the acceptance artifact: this ADR flips to
Accepted only after a fresh deployment on each cloud succeeds by
following its document verbatim.

## Context

- Blob storage: `BlobStorage` (`engram-core/src/traits/storage.rs`)
  has `local` and `gcs` arms. `engram-storage-s3` is a typed stub;
  `engram_blob_client::from_env()` rejects `ENGRAM_BLOB_BACKEND=s3`.
- Autoscaling: the host-operator owns policy (ADR 0048); actuation is
  the two-method `NodePoolScaler` trait
  (`engram-core/src/traits/cloud.rs`). Only a GKE arm exists
  (`engram-cloud-gcp`). `set_size` is grow-only by invariant;
  `remove_node` names the victim and is the only shrink path.
- Secrets: `--secrets-backend env|gcp`; the host-agent egress CA
  source is `env|local-disk|gcp-secret-manager`.
- KEK: `--kek-provider env-var|gcp-kms`; `GcpKmsProvider` is a stub
  and production uses `env-var`.
- Registry auth: `static|gcp_workload_identity|anonymous`; no ECR.
- Deploy: `deploy/terraform/gcp` holds three thin modules; the
  cluster, KVM node pool, database, and secrets machinery live in the
  private deploy repo. The main Helm chart's image defaults point at
  GHCR paths that do not exist, and several documents describe the
  retired Packer/MIG topology.

## Decision

### D1. AWS SDK strategy: official SDK, ring-only, one bootstrap crate

We use the official `aws-sdk-*` v1 crates, every one with
`default-features = false`, and TLS through `aws-smithy-http-client`
with the `rustls-ring` feature. No crate in the tree may enable any
SDK's default HTTPS client (it links `aws-lc-rs` — the crypto trap).
CI enforces this mechanically: the lint job fails if `aws-lc` appears
in `Cargo.lock`.

A new crate, **`engram-aws`**, is the only place that constructs an
`SdkConfig` — the `engram-tls` pattern applied to AWS. It:

- calls `engram_tls::install_provider()` first;
- builds the smithy HTTP client with
  `tls::Provider::Rustls(CryptoMode::Ring)`;
- sets a 5 s connect timeout and no operation timeout (blob bodies
  are GB-scale; the `BlobClient` layer owns per-attempt deadlines);
- sets `RetryConfig::disabled()` — one retry layer per path:
  `BlobClient` retries blobs, the operator's reconcile re-asserts
  scaling, and secret resolution fails the request to its caller;
- honors region and endpoint overrides from the caller and env;
- restricts the connection to HTTP/1.1 if the builder exposes ALPN
  control. The GCS arm measured h2 at 4-5x worse bulk throughput
  (2026-07-31 blobbench); we A/B the same on S3 during Phase G and
  record the numbers here.

Per-service `aws-sdk-*` dependencies live in their consumer crates;
the shared `aws-config`/`aws-smithy-*` pins live in the workspace
root. One dependabot group moves the family together.

Credential resolution is the SDK default chain: env → IRSA
web-identity → profile → ECS → IMDSv2. Tests always inject static
credentials so the chain never probes the instance metadata address
in CI.

### D2. S3 blob storage: sized single PUT, bounded multipart for streams

`S3BlobStorage` replaces the stub (clean break, no stub remnants):

- `put` (body in hand, the chunk hot path): one sized `PutObject`.
- `put_streaming`: the trait's `ByteStream` carries no length, and S3
  requires sized bodies. We buffer into 16 MiB parts. A stream that
  ends inside the first part becomes a single `PutObject`; a longer
  stream becomes a multipart upload (create → sequential part puts →
  complete), with best-effort `AbortMultipartUpload` on any error so
  no half-written destination remains. This also covers objects over
  the 5 GB single-PUT limit. Peak memory is one to two parts per
  in-flight upload.
- `head` maps both `NoSuchKey` and HeadObject's code-less 404 to
  `BlobError::NotFound` (S3 returns the latter without an error
  code; missing either breaks `exists()`).
- `delete` is natively idempotent; `list_prefix` walks
  `ListObjectsV2` continuation tokens.
- `ENGRAM_S3_ENDPOINT_URL` switches on `force_path_style` (MinIO and
  other emulators); `ENGRAM_S3_REGION` overrides the chain.

Etag semantics: S3 multipart etags are not MD5 and carry a `-N`
suffix. The trait already declares etags opaque; PR review audits the
few `etag` consumers for cross-backend or content-hash assumptions.

A shared conformance suite (`engram_testkit::blob_conformance`) is
extracted from the GCS emulator tests and runs against local, GCS
(fake-gcs-server), and S3 (MinIO) — the same scenarios everywhere, so
a backend cannot silently diverge.

### D3. ASG node-pool scaler: trait unchanged, names resolved inside the arm

`engram-cloud-aws` implements `NodePoolScaler` over an EC2 Auto
Scaling group; the pool identifier is the ASG name.

- `set_size` → `SetDesiredCapacity` (honor_cooldown false). Grow-only
  per the ADR 0048 invariant; clamp errors surface as protocol
  errors. No LRO polling — reconcile re-asserts, the GKE posture.
- `remove_node` → resolve the K8s node name (the EC2 private DNS name
  on default EKS) to an instance id with `ec2:DescribeInstances`,
  verify ASG membership, then
  `TerminateInstanceInAutoScalingGroup{ShouldDecrementDesiredCapacity: true}`.
  A missing or already-terminating node returns `Ok(())`
  (idempotent, per the trait contract).

We keep the trait unchanged rather than threading
`Node.spec.providerID` through it: that alternative touches the GKE
arm and the operator for no GKE benefit. The documented v1
constraint: EKS-default node naming only (Karpenter or
`--hostname-override` fleets are unsupported).

Selection string: `ENGRAM_NODE_POOL_SCALER=asg` (chart value
`operator.autoscaling.scaler: asg`), with the detect-or-noop fallback
the GKE arm uses.

Terraform-side coupling (the AWS `host-operator-iam` module must
match): the ASG suspends `AZRebalance` (it would pick its own
victims), sets `max_size` above the operator's ceiling, leaves
scale-in protection off, and `ignore_changes` on desired capacity —
the operator owns size, exactly like the GKE pool's
`ignore_changes = [node_count]`.

### D4. Identity: IRSA everywhere, including hostNetwork pods

GKE Workload Identity is a metadata-server intercept, and the
host-agent's `hostNetwork: true` pods bypass it — which forced the
`egress.caSource: env` + External Secrets relay on GCP. IRSA has no
such hole: the injected token file works in hostNetwork pods, and the
IMDS fallback yields the node instance role, which can also be
granted the needed reads.

Consequence: on AWS every component uses native arms directly —

- coordinator: `--secrets-backend aws`, `--kek-provider aws-kms`;
- host-agent: `--ca-source aws-secrets-manager`;
- both: S3 via IRSA role annotations on their service accounts.

External Secrets still delivers the bootstrap env that must exist
before the process can talk to anything (`DATABASE_URL`,
`ENGRAM_AUTH_TOKENS`, the better-auth secret, and the
`CONTROL_PLANE_BEARER` = first element of the auth-token list
template) — the same shape GCP production uses.

### D5. Secrets Manager arm

`engram-secrets-aws` mirrors `engram-secrets-gcp`'s shape
(`SecretStore` + a `ca.rs` egress-CA source) but on
`aws-sdk-secretsmanager` — SigV4 makes the GCP crate's hand-rolled
REST approach a bad trade here.

Ref scheme: `aws-sm://<name-or-arn>[#version-stage]`. Default
namespacing is AWS-idiomatic slash paths, `engram/<repo>/<name>`,
because Secrets Manager names allow `/` and IAM prefix policies are
path-based. `ResourceNotFoundException` → `Ok(None)`; access denied →
a typed Unauthorized with an IRSA/node-role hint (the GCP 403 hint's
twin); binary-only secrets are a typed error.

The host-agent's gcp-branded cert/key secret-path flags are
generalized rather than duplicated (clean break; zero users).

### D6. KMS KEK provider — real, and asymmetric with GCP

`engram-kms-aws` implements `MasterKeyProvider` with KMS
`Encrypt`/`Decrypt` of the 32-byte DEK (far under the 4096-byte
limit; the ciphertext self-describes its key). Selection:
`--kek-provider aws-kms --aws-kms-key-id <arn>`.

Recorded asymmetry: `GcpKmsProvider` stays a stub and GCP production
keeps `env-var`. AWS gets the real provider because it ships in the
same effort; nothing migrates between KEK providers — sealed data is
provider-bound via `key_id()`, a new-deployment concern only.

It is a separate crate, not code inside `engram-crypto`:
`engram-crypto` is a dependency of musl guest binaries and every
sealing consumer, and must not pull an AWS SDK.

### D7. ECR registry auth

A new `RegistryAuthSpec::AwsEcr { assume_role_arn }` kind
(`kind() = "aws_ecr"`), a migration extending the auth-kind CHECK
constraint, and `engram-oci-auth/src/aws.rs`: parse the region from
`<acct>.dkr.ecr.<region>.amazonaws.com`, call
`GetAuthorizationToken`, decode the `AWS:<password>` pair, and cache
per strategy until ~15 minutes before the ~12 h expiry (the GCP token
cache's twin). `assume_role_arn` is schema'd but returns a clear
not-implemented error, mirroring the GCP arm's deferred
impersonation. The proto, web settings panel, and the engram-sim
conformance suite (ADR 0098 D4) extend in the same PR.

### D8. Test strategy: MinIO in the existing lane, wiremock for the rest

- S3: MinIO joins fake-gcs-server in the `test-linux` CI job (a
  `docker run` + job env; the existing workspace nextest step picks
  up the env-gated conformance tests). Dev gets a `local-s3` compose
  profile and Tiltfile wiring. No new CI lane, no `CI Gate` change.
- ASG / Secrets Manager / KMS / ECR: wiremock against the endpoint
  override with static credentials — fixture-pinned protocol tests
  (including the idempotent missing-node path and error mapping).
  LocalStack is rejected for CI: a heavy service for marginal value
  over wiremock plus the live phase.
- Live: Phase G exercises every arm against real AWS before this ADR
  flips to Accepted; the ASG arm carries an "UNVALIDATED ON REAL
  INFRA" header until then.

### D9. Deploy surface: promoted modules, per-cloud quickstarts, docs as the gate

- The generic Terraform machinery moves from the private repo into
  `deploy/terraform/gcp/modules/` (`gke-cluster`, `gke-kvm-pool`,
  `cloudsql`, `secrets`, `host-operator-iam`); the existing three
  modules' paths and variables are frozen (the private repo consumes
  them by git ref). `deploy/terraform/aws/modules/` mirrors the
  layout (`network`, `storage`, `eks-cluster` wrapping the pinned
  community EKS module, `kvm-nodegroup` as a self-managed ASG of
  `m7i.metal-24xl` by default — Sapphire Rapids, deliberate CPUID
  parity with GCP C3 so snapshots restore across clouds — `rds`,
  `secrets`, `irsa`, `host-operator-iam`).
- Each cloud gets a `quickstart/` root module: one apply from a fresh
  account to a running cluster — namespaces (fleet namespace
  PSA-privileged), External Secrets install and stores, optional
  IAP/DNS on GCP, ACM on AWS — plus `templatefile()`-rendered Helm
  values as Terraform outputs, layered under the static
  `values-<cloud>.yaml` via `helm -f`. This retires the sed-REPLACE
  pattern for OSS users.
- The identity proxy is optional and the quickstarts default to
  none: the orchestrator login wall (ADR 0118) is the auth door, so
  a single `"/" → web` ingress rule is a complete deployment. IAP
  stays an opt-in GCP overlay carrying the ingress path table; ALB
  OIDC/Cognito is documented as unsupported (the ADR 0118
  CORS-preflight class of breakage).
- `docs/deploy-gcp.md` and `docs/deploy-aws.md` are the acceptance
  artifact: Phase G executes each verbatim on a fresh environment,
  and any step that needs knowledge outside the document is a
  documentation bug to fix and re-walk.

## What cannot "just work" (documented manual steps)

Secret-shell population (values must never enter Terraform state),
DNS records and certificate waits, C3/metal vCPU quota tickets, the
CPUID one-way door (moving to an older CPU platform requires
re-baking every enabled image), the nested-virt pool invariants, and
first-image enable plus OIDC client creation.

## Alternatives considered

- **`object_store`/`opendal` instead of per-backend SDK crates**: the
  in-house trait is five required methods and already has three
  implementations, a retry layer, and a fault-injection decorator
  built against it. A generic storage crate would replace working,
  tuned code (the GCS h2 rejection, the streaming hot path) with a
  new abstraction for no new capability.
- **Karpenter for AWS autoscaling**: the operator's contract is
  "resize this pool; remove this named node". ASGs implement it
  directly; Karpenter's NodeClaim model would put a second
  autoscaler's policy under ours. Rejected for v1, same reasoning as
  ADR 0048's rejection of cluster-autoscaler.
- **LocalStack in CI**: heavier than MinIO+wiremock, and the live
  Phase G covers the real-protocol risk.
- **Managed EKS node group**: reconciles scaling config and fights
  the operator's external actuation; a launch template + ASG
  expresses the labels/taints and leaves the operator in control.

## Phases and PR chain

- A: this ADR (Proposed).
- B: Rust arms — B1 blob conformance suite; B2 `engram-aws` +
  workspace pins + aws-lc CI guard; B3 S3 + MinIO; B4 ASG scaler;
  B5 Secrets Manager; B6 KMS; B7 ECR (+ migration, proto, web, sim
  conformance).
- C: Helm — deploy CI lane; image-default fix; main-chart S3 +
  `serviceInternal` toggle; GCP values examples; AWS values examples.
- D/E: Terraform — GCP module promotion + quickstart; AWS modules +
  quickstart.
- F: docs — `docs/deploy.md` rewrite (Packer/MIG purge),
  `docs/deploy-gcp.md`, `docs/deploy-aws.md`.
- G: fresh-deployment validation on both clouds from the docs,
  blobbench numbers recorded here, private-repo migration onto the
  promoted modules, and the Accepted flip with the commit chain.
