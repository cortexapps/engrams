---
title: "Helm chart: engram"
description: The control-plane chart, what it deploys and does not, and the values that matter.
sidebar:
  order: 5
---

`deploy/helm/engram` deploys the control plane to any Kubernetes cluster: the coordinator,
the orchestrator, and nginx serving the dashboard. The chart is cloud-agnostic; the bindings
that differ per cloud, such as Workload Identity, KMS, and load-balancer annotations, live in
the `values-gcp.yaml.example` and `values-aws.yaml.example` overlays, and the Terraform
quickstarts render a second overlay with every value they know.

## What it deploys

**The coordinator.** A stateless Deployment with `replicaCount` replicas, a ClusterIP Service
for in-cluster traffic, a ServiceAccount annotated for the cloud's identity binding, and a
ConfigMap of non-secret settings. Optionally a HorizontalPodAutoscaler, a
PodDisruptionBudget, and a NetworkPolicy. The coordinator's Ingress is off by default and
should stay off: the coordinator has no authentication of its own for people.

**The web frontend.** With `web.enabled`, an nginx Deployment that serves the dashboard and
reverse-proxies API calls, event streams, and WebSockets to the orchestrator, plus the
Service the public Ingress targets. The web Ingress is the only public surface of a
deployment.

**The orchestrator.** With `orchestrator.enabled`, the Deployment that owns sign-in, tasks,
profiles, integrations, and the API. Its settings, including the admin allow-list
(`orchestrator.auth.adminEmails`), live under the `orchestrator` block.

## What it does not deploy

- **Postgres.** Bring your own: Cloud SQL, RDS, or an in-cluster operator. The connection
  string comes from a Secret named by `database.existingSecret`.
- **The blob bucket.** Provisioned outside the chart, by Terraform, and named in `blob`.
- **The host fleet.** That is the [`engram-host-fleet` chart](../helm-engram-host-fleet/).
- **The master key.** A KMS key on AWS, or a base64 key placed in the coordinator's Secret on
  GCP.
- **The managed certificate and load balancer.** Terraform creates them; the chart references
  them by name in annotations.

## Prerequisites

A cluster (GKE 1.28 or later with Workload Identity, EKS 1.28 or later with IRSA, or any
conformant cluster with a Secret-based master key for development), a Postgres the pods can
reach, and a Secret named by `database.existingSecret` (default
`engram-coordinator-secrets`) that carries `DATABASE_URL`, `ENGRAM_AUTH_TOKENS`, and, with
the `env-var` master-key provider, `ENGRAM_KEK_MASTER_KEY`. The coordinator runs migrations
at boot.

## The values that matter

`values.yaml` is annotated line by line; this is the map of its sections.

| Section | What it sets |
|---|---|
| `replicaCount`, `image`, `imagePullSecrets` | Coordinator replicas and image. The defaults point at the published images on GHCR. |
| `mode` | `coordinator` for a real deployment. `all` runs a host agent inside the coordinator pod and is for single-binary development only. |
| `blob` | `backend` (`gcs`, `s3`, `local`), and the bucket, region, and endpoint for it. The key names match the fleet chart so the two agree. |
| `kek` | The master-key provider and its key: `env-var` with `envVarName`, or `aws-kms` with `awsKeyId`. |
| `secrets` | The secret backend, `gcp` with `gcpProjectId` or `aws`. |
| `database` | The Secret and key that hold `DATABASE_URL`. |
| `auth` | The Secret and key that hold the machine bearer tokens. |
| `appGrpc` | The coordinator's private RPC port for the orchestrator, and the Secret with its bearer. Required in production; an empty bearer refuses every call. |
| `forge` | An optional GitHub App so agents can fetch short-lived git credentials and open pull requests without a durable token in the sandbox. |
| `service`, `serviceInternal`, `ingress` | The coordinator's Services and the Ingress that should stay disabled. |
| `hpa`, `pdb`, `networkPolicy`, `resources`, `nodeSelector`, `tolerations`, `affinity` | Scheduling and availability. |
| `otel` | An OTLP endpoint for the coordinator's own traces. |
| `probes`, `drainSeconds`, `terminationGracePeriodSeconds` | Liveness on `/healthz`, readiness on `/readyz`, and how long a replica drains before it stops. |
| `web` | The dashboard: image, Ingress, and the annotations that attach a managed certificate or IAP. |
| `orchestrator` | The orchestrator: image, its Secret with the session-signing key, `auth.adminEmails`, the public URL, and integration settings. |

## Upgrades

The coordinator is stateless, so `helm upgrade` is a rolling restart of whichever
Deployment's template changed. Sessions live on the hosts and are untouched by a control-plane
roll; dashboard event streams reconnect with `Last-Event-ID`, and hosts re-register when the
coordinator comes back. Roll the two charts together when you upgrade engrams: the
coordinator and the host agents share a wire version and refuse to talk across a mismatch.
