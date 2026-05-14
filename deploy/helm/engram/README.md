# engram Helm chart

Deploys the Engram coordinator (always) plus an optional nginx web
frontend (default on) to any K8s cluster. The chart is cloud-agnostic
— cloud-specific bindings (Workload Identity, KMS, LB annotations,
IAP) live in the `values-*.yaml.example` overlays.

## What this chart deploys

**Coordinator (always):**

- A stateless `Deployment` (`<release>-coordinator`).
- A `ClusterIP` `Service` (`<release>-coordinator`) for in-cluster
  traffic — the web nginx pod proxies browser API calls here.
- A `LoadBalancer` `Service` (`<release>-coordinator-internal`) with
  cloud internal-LB annotations, for the FC host MIG dialing in over
  WS from a sibling MIG. Pin `serviceInternal.loadBalancerIP` to a
  Terraform-reserved VPC address so the MIG's `coordinator_endpoint`
  can be plumbed before Helm installs.
- A `ServiceAccount` (`<release>-coordinator`) with annotations for
  cloud-native identity (Workload Identity on GKE, IRSA on EKS).
- A `ConfigMap` (`<release>-coordinator-config`) carrying non-secret
  env vars.
- Optional `Ingress` (off by default — see invariant below),
  `HorizontalPodAutoscaler`, `PodDisruptionBudget`, `NetworkPolicy`.

**Web (gated by `web.enabled`, default true):**

- A `Deployment` of nginx (`<release>-web`) serving the React SPA
  bundled into the web image and reverse-proxying `/v1/*`, `/api/*`,
  `/events` to the coord ClusterIP. WS upgrade + SSE both pass
  through.
- A `ClusterIP` `Service` (`<release>-web`) — the target of the
  public Ingress.
- Optional `Ingress` (`web.ingress.enabled`) — the **only** public
  surface of the deployment. On GCP, fronted by a Google HTTPS LB
  with IAP attached via a `BackendConfig` annotation on the web
  Service.
- A `ConfigMap` (`<release>-web-config`) carrying the nginx server
  block (SPA routing + coord reverse proxy).

**Public-surface invariant.** The coord has no public IP. Browsers
reach the SPA via the public IAP'd HTTPS LB → nginx → in-cluster
ClusterIP. Host-agents reach the coord at a VPC-only internal LB.
Set `web.enabled=false` only if you're running a CLI-only deploy and
have an alternative auth-gated path to the coord; never enable the
coord `ingress` without an auth proxy in front.

## What this chart does NOT deploy

- **Postgres** — bring your own (Cloud SQL, RDS, in-cluster operator).
  Pass the connection URL via a Secret.
- **The blob backend** — GCS bucket / S3 bucket is provisioned outside
  the chart (Terraform). Pass the bucket name via values.
- **FC host fleet** — `engram-host-agent` runs on FC-capable VMs
  outside the K8s cluster. See `deploy/packer/` + `deploy/terraform/`
  for the fleet provisioning.
- **The KEK** — bring your own KMS key (GCP KMS, AWS KMS) or stash a
  base64 master key in the DATABASE_URL secret (dev only).
- **The managed cert + IAP brand/client** — provisioned in Terraform;
  the chart only references them by name via annotations.

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

helm install engram ./deploy/helm/engram \
  --set blob.backend=local \
  --set blob.gcs.bucket="" \
  --set kek.provider=env-var \
  --set secrets.backend=env \
  --set image.repository=ghcr.io/cortexapps/engram-coordinator \
  --set image.tag=0.1.0 \
  --set web.enabled=false

kubectl port-forward svc/engram-coordinator 8080:8080
curl localhost:8080/healthz
```

## Quickstart — GKE

```sh
# 1. Provision the supporting infrastructure with Terraform (GCS
#    bucket, KMS key, Cloud SQL, GSA + WI binding, reserved internal
#    IP for the coord LB, IAP BackendConfig, managed cert). See
#    `deploy/terraform/gcp/`.

# 2. Drop the images into Artifact Registry (private CI; the OSS
#    repo doesn't publish images):
#    docker buildx build -f docker/coordinator.Dockerfile --push \
#      -t us-docker.pkg.dev/$PROJECT/engram/coordinator:0.1.0 .
#    docker buildx build -f docker/web.Dockerfile --push \
#      -t us-docker.pkg.dev/$PROJECT/engram/web:0.1.0 .

# 3. Stash secrets (use Secret Manager + External Secrets Operator
#    in prod):
kubectl create secret generic engram-coordinator-secrets \
  --from-literal=DATABASE_URL='postgres://...' \
  --from-literal=ENGRAM_AUTH_TOKENS='tok1,tok2'

# 4. Install:
cp deploy/helm/engram/values-gcp.yaml.example my-values.yaml
# edit my-values.yaml: REPLACE_PROJECT, REPLACE_BUCKET,
#   REPLACE_KMS_RESOURCE, REPLACE_RESERVED_INTERNAL_IP, REPLACE_DOMAIN
helm install engram ./deploy/helm/engram -f my-values.yaml
```

## Upgrades

The coord is stateless. `helm upgrade` triggers a rolling restart of
whichever component's pod template actually changed (Helm doesn't
roll Deployments whose hash is unchanged). Active sessions live on
FC hosts; their state is untouched by coord recycles. SSE clients
reconnect via `Last-Event-ID` automatically; WS dialer connections
from host-agents reconnect on the existing backoff schedule.

Bump web (e.g. SPA tweaks) without rolling coord: `helm upgrade
--set web.image.tag=NEW`. Bump coord without rolling web:
`helm upgrade --set image.tag=NEW`.

WS-protocol changes: the chart's `appVersion` should bump in lockstep
with `engram_protocol::WIRE_VERSION`. A mixed-version deploy is
caught by the hello-frame handshake (loud error + connection drop);
the host-agents reconnect once both sides are at the same version.

## Values reference

See [`values.yaml`](./values.yaml) for the full schema. The most
common knobs:

### Coordinator

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
| `service.type` | `ClusterIP` | Keep as ClusterIP — in-cluster path only. Out-of-cluster reach is `serviceInternal`. |
| `serviceInternal.loadBalancerIP` | `""` | VPC-internal IP for FC host dial-in. Pin to a `google_compute_address`/equivalent reservation. |
| `serviceInternal.annotations` | `{}` | Cloud-specific internal-LB annotations (see `values-gcp.yaml.example`). |
| `ingress.enabled` | `false` | Leave off — coord must not be public. |
| `hpa.enabled` | `true` | Stateless coord scales on CPU. |
| `pdb.minAvailable` | `1` | Lift to `2` in prod. |
| `networkPolicy.enabled` | `false` | Lock down egress in prod once CIDRs are known. |
| `resources` | 0.5 cpu / 512Mi req | Bump for high-throughput. |

### Web

| Key | Default | Notes |
|---|---|---|
| `web.enabled` | `true` | Set false for CLI-only deploys. |
| `web.image.repository` | `ghcr.io/cortexapps/engram-web` | Set to your registry. |
| `web.image.tag` | `""` → `Chart.AppVersion` | Pin a specific version. |
| `web.replicaCount` | 2 | No HPA on the web — it's cheap. |
| `web.containerPort` | 8080 | nginx listens here; lets the pod run as non-root UID 101. |
| `web.service.type` | `ClusterIP` | Always ClusterIP — public reach is via Ingress only. |
| `web.service.port` | 80 | Ingress targets this. |
| `web.service.annotations` | `{}` | IAP BackendConfig annotation lands here (see `values-iap.yaml.example`). |
| `web.ingress.enabled` | `false` | Flip on once you have managed cert + DNS. |
| `web.ingress.annotations` | `{}` | GKE managed-cert + static-IP refs. |
| `web.resources` | 50m / 64Mi req | nginx is light. |
