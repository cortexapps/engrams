# Engram Terraform — pick your cloud

- [`gcp/`](gcp/README.md) — GKE. The production-validated deployment.
- `aws/` — EKS (ADR 0122; lands with the AWS quickstart).

Each cloud ships the same shape: composable `modules/` + a
`quickstart/` root that takes a fresh account to an engram-ready
cluster in one apply, with rendered Helm values as outputs. The
step-by-step bring-ups live in `docs/deploy-gcp.md` and
`docs/deploy-aws.md`.

## The cloud-agnostic contract

Per ADR 0007/0122, engram's only hard requirements on a cloud are:

1. **Postgres ≥ 14** with `LISTEN/NOTIFY` (Cloud SQL, RDS, your own).
2. **Object storage** satisfying the `BlobStorage` trait (GCS, S3,
   MinIO/R2 via the S3 endpoint override).
3. **A secret store** satisfying `SecretStore` + `CaSource` (GCP
   Secret Manager, AWS Secrets Manager) — or the `env` arms plus any
   relay that lands K8s Secrets (External Secrets Operator).
4. **Linux + KVM-capable nodes** for the Firecracker hosts. This is
   the binding constraint: nested virtualization is **Intel-only** on
   every managed provider (GKE C3 family; EKS bare-metal `*.metal` or
   Xeon-6 C8i/M8i/R8i), it is set at node-pool creation, and the CPU
   platform is a **one-way door for snapshots** — images baked on a
   newer platform never restore on an older one. The two quickstarts
   default to Sapphire Rapids on both clouds (GCP C3 / AWS m7i) so
   images bake once.
5. **Kubernetes** for the control plane and the host-fleet DaemonSet
   (ADR 0044).

A new cloud implements the contract by mirroring the layout: its own
`modules/network`, `modules/storage`, a KVM node-pool module honoring
the invariants above, a `NodePoolScaler` actuator crate for the
autoscaling operator (ADR 0048), and a quickstart wiring them. The
Helm charts are already cloud-agnostic — only a values overlay is
per-cloud.
