# engram-coordinator Helm chart

Deploy the Engram coordinator to any K8s cluster. The chart is
cloud-agnostic — cloud-specific bindings (Workload Identity, KMS,
LB annotations) live in the `values-*.yaml.example` overlays.

## What this chart deploys

- A stateless `Deployment` of the coordinator
- A `Service` exposing the HTTP+WS API
- Optional `Ingress`, `HorizontalPodAutoscaler`, `PodDisruptionBudget`,
  `NetworkPolicy`
- A `ServiceAccount` with annotations for cloud-native identity
  (Workload Identity on GKE, IRSA on EKS)
- A `ConfigMap` carrying non-secret env vars

## What this chart does NOT deploy

- **Postgres** — bring your own (Cloud SQL, RDS, in-cluster operator).
  Pass the connection URL via a Secret.
- **The blob backend** — GCS bucket / S3 bucket is provisioned outside
  the chart (Terraform). Pass the bucket name via values.
- **FC host fleet** — `engram-host-agent` runs on FC-capable VMs
  outside the K8s cluster (or inside via KubeVirt-style nesting,
  but that's not the supported topology). See
  `deploy/packer/` + `deploy/terraform/` for the fleet provisioning.
- **The KEK** — bring your own KMS key (GCP KMS, AWS KMS) or stash a
  base64 master key in the DATABASE_URL secret (dev only).

## Prerequisites

1. `kubectl` + `helm` ≥ 3.10.
2. A K8s cluster:
   - GKE 1.28+ with Workload Identity, **or**
   - EKS 1.28+ with IRSA, **or**
   - any conformant cluster + a Secret-based KEK (dev).
3. A Postgres instance reachable from the pods. Schema migrations
   from `deploy/migrations/` applied.
4. A Secret named per `database.existingSecret` (default
   `engram-coordinator-secrets`) carrying:
   - `DATABASE_URL` — the Postgres connection string.
   - (env-var KEK only) `ENGRAM_KEK_MASTER_KEY` — base64-encoded
     32-byte key.
   - (auth-enabled) `ENGRAM_AUTH_TOKENS` — comma-separated bearer
     allow-list.
5. (GCP / AWS) The bound ServiceAccount has the right roles on the
   blob bucket + KEK key + secret resources.

## Quickstart — cluster smoke (kind)

```sh
kind create cluster
helm install pg bitnami/postgresql --set auth.postgresPassword=test
kubectl create secret generic engram-coordinator-secrets \
  --from-literal=DATABASE_URL='postgres://postgres:test@pg-postgresql.default:5432/postgres' \
  --from-literal=ENGRAM_KEK_MASTER_KEY="$(head -c 32 /dev/urandom | base64)"

helm install engram ./deploy/helm/engram-coordinator \
  --set blob.backend=local \
  --set blob.gcs.bucket="" \
  --set kek.provider=env-var \
  --set secrets.backend=env \
  --set image.repository=ghcr.io/cortexapps/engram-coordinator \
  --set image.tag=0.1.0

kubectl port-forward svc/engram-coordinator 8080:8080
curl localhost:8080/healthz
```

## Quickstart — GKE

```sh
# 1. Provision the supporting infrastructure with Terraform (GCS
#    bucket, KMS key, Cloud SQL, GSA + WI binding). See
#    `deploy/terraform/gcp/`.

# 2. Drop the coord image into Artifact Registry:
#    docker buildx build --push -t us-docker.pkg.dev/$PROJECT/engram/coordinator:0.1.0 .

# 3. Stash secrets (use Secret Manager for the source of truth and
#    sync into K8s via External Secrets Operator if you don't want
#    raw kubectl create secret):
kubectl create secret generic engram-coordinator-secrets \
  --from-literal=DATABASE_URL='postgres://...' \
  --from-literal=ENGRAM_AUTH_TOKENS='tok1,tok2'

# 4. Install:
cp deploy/helm/engram-coordinator/values-gcp.yaml.example my-values.yaml
# edit my-values.yaml: REPLACE_PROJECT, REPLACE_BUCKET, REPLACE_KMS_RESOURCE
helm install engram ./deploy/helm/engram-coordinator -f my-values.yaml
```

## Upgrades

The coord is stateless. `helm upgrade` triggers a rolling restart.
Active sessions live on FC hosts; their state is untouched by coord
recycles. SSE clients reconnect via `Last-Event-ID` automatically;
WS dialer connections from host-agents reconnect on the existing
backoff schedule.

WS-protocol changes: the chart's `appVersion` should bump in lockstep
with `engram_protocol::WIRE_VERSION`. A mixed-version deploy is
caught by the hello-frame handshake (loud error + connection drop);
the host-agents reconnect once both sides are at the same version.

## Values reference

See [`values.yaml`](./values.yaml) for the full schema. The most
common knobs:

| Key | Default | Notes |
|---|---|---|
| `replicaCount` | 2 | Ignored when `hpa.enabled=true`; bump `hpa.minReplicas` instead. |
| `image.repository` | `ghcr.io/cortexapps/engram-coordinator` | Set to your registry. |
| `image.tag` | `""` → `Chart.AppVersion` | Pin a specific version. |
| `mode` | `coordinator` | `all` is dev-only. |
| `blob.backend` | `gcs` | `local` for dev; `s3` reserved. |
| `blob.gcs.bucket` | `""` | Required for `gcs`. |
| `kek.provider` | `gcp-kms` | `env-var` for dev. |
| `kek.gcpResource` | `""` | Required for `gcp-kms`. |
| `secrets.backend` | `gcp` | `env` for dev. |
| `secrets.gcpProjectId` | `""` | Required for `gcp`. |
| `database.existingSecret` | `engram-coordinator-secrets` | |
| `auth.existingSecret` | `""` | Empty disables bearer auth (dev). |
| `serviceAccount.annotations` | `{}` | Per-cloud identity binding. |
| `service.type` | `ClusterIP` | `LoadBalancer` for direct exposure. |
| `ingress.enabled` | `false` | Flip on with TLS for public hosts. |
| `hpa.enabled` | `true` | Stateless coord scales on CPU. |
| `pdb.minAvailable` | `1` | Lift to `2` in prod. |
| `networkPolicy.enabled` | `false` | Lock down egress in prod once CIDRs are known. |
| `resources` | 0.5 cpu / 512Mi req | Bump for high-throughput. |
