# Engram on GCP — Terraform reference

This directory is the reference deployment for Engram on Google
Cloud. The shape is **opinionated modules + a minimal example**:
each module owns one concern (network, storage, FC host fleet);
the example wires them together. Operators copy the example and
swap in their environment-specific values.

## Layout

```
modules/
  network/         VPC + subnet + Cloud NAT + firewall
  storage/         GCS chunks bucket + bucket-scoped SA
  fc-host-mig/     Regional MIG of Firecracker hosts (Packer image)
examples/
  minimal/         End-to-end: VPC + bucket + KMS key + MIG +
                   coordinator SA. Bring your own GKE + Cloud SQL.
```

## What this gives you

After `terraform apply`:

- A VPC isolated from the rest of your project, with Cloud NAT
  so FC guests reach the public internet without per-host IPs.
- A GCS bucket holding ADR 0007 chunked storage (disk + memory
  snapshots), with a dedicated GSA that owns read+write on it.
- A KMS key the coord uses for envelope-encrypting registry
  credentials + session secrets.
- A regional MIG of FC-capable VMs running the
  `engram-host-agent` systemd unit. Auto-healing, autoscaling on
  CPU, rolling updates with the per-host drain hook so sessions
  migrate gracefully on replacement.
- Per-host instance SA with the IAM grants needed to read+write
  chunks + decrypt registry creds via Workload Identity.
- A coordinator SA + IAM scaffolding so the Helm chart's
  Workload Identity binding has something to point at.

## What this doesn't give you

| Concern | Where to get it |
|---|---|
| GKE cluster | `terraform-google-modules/kubernetes-engine/google` |
| Cloud SQL Postgres | `terraform-google-modules/sql-db/google//modules/postgresql` |
| Artifact Registry repo | One `google_artifact_registry_repository` per env; inline it |
| Secret Manager entries | Operator-specific contents (DATABASE_URL, auth tokens, etc.); create the secrets with TF, populate the values out-of-band |
| Public ingress | `gke-managed-certs` + a `google_compute_managed_ssl_certificate`; the Helm chart's `ingress` block handles the K8s side |

This is deliberate. The published Google modules already do the
clusters / databases better than we'd re-roll; we focus on the
pieces that are *specific to Engram*.

## Apply order

The Helm chart for the coord needs the GKE cluster + Cloud SQL +
Secret Manager entries in place first, so the recommended order:

1. Build the host image with Packer (`deploy/packer/`) — produces
   the GCE image family the MIG consumes.
2. Provision the supporting infra (GKE, Cloud SQL, Secret
   Manager) — out of scope for this reference.
3. `terraform apply` in `examples/minimal/` — VPC, bucket, KMS,
   MIG, coordinator SA. Outputs the values the Helm chart needs.
4. `helm install` the coordinator (`deploy/helm/engram-coordinator/`)
   using those outputs.
5. Once the coord's internal LB is up, capture its URL and
   `terraform apply` again with `coordinator_endpoint` set — the
   MIG rolling-updates with the new env file, host-agents dial
   home.

## Cloud-agnostic contract

Per ADR 0007, Engram's only hard requirements on the cloud are:

1. **Postgres ≥14** with `LISTEN/NOTIFY`.
2. **Object storage** satisfying the `BlobStorage` trait (GCS,
   S3, MinIO). The chunks bucket lives here.
3. **Secret store** satisfying `SecretStore` + `CaSource` (GCP
   Secret Manager, AWS Secrets Manager, HashiCorp Vault).
4. **Linux + KVM-capable VMs** for FC hosts (any provisioning
   mechanism — MIG, ASG, on-prem hand-rolled).
5. **Kubernetes** for the coordinator (optional — the coord runs
   as a single-process VM too, but K8s is the supported
   topology).

Anyone implementing this contract on AWS/on-prem/Azure can mirror
the module layout: their own `modules/network/`, `modules/storage/`
(S3 bucket + IAM), `modules/fc-host-asg/` (ASG of FC hosts), and
the same `examples/minimal/` shape wiring them together. The Helm
chart is already cloud-agnostic — only `values-aws.yaml.example`
needs filling in.
