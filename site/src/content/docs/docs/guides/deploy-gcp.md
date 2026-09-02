---
title: Deploy on GCP
description: A fresh GCP project to a working engrams deployment on GKE, command by command.
sidebar:
  order: 2
---

This page takes a fresh GCP project to a working deployment. Every command is meant to run
as written. If a step needs something this page does not tell you, that is a bug in this
page; please file it.

**Before you start**

- A GCP project with billing, and Owner on it.
- `gcloud`, `terraform` 1.5 or later, `helm` 3.10 or later, `kubectl`, and `openssl`.
- A domain you control. You will create one A record.
- C3 quota. The KVM pool is two `c3-standard-22` nodes by default, which is 44 vCPUs. Check
  `gcloud compute regions describe <region>` for `C3_CPUS` headroom, and that the region
  offers C3 at all, before you apply. A quota request can take a day.

Throughout, `PROJECT`, `REGION`, `DOMAIN` (for example `engrams.example.com`), and
`ADMIN_EMAIL` are yours.

## 1. Enable the APIs

```sh
gcloud services enable --project=$PROJECT \
  container.googleapis.com compute.googleapis.com \
  sqladmin.googleapis.com servicenetworking.googleapis.com \
  secretmanager.googleapis.com storage.googleapis.com \
  iam.googleapis.com dns.googleapis.com
```

## 2. Terraform, one apply

```sh
cd deploy/terraform/gcp/quickstart
terraform init
terraform apply \
  -var project_id=$PROJECT \
  -var region=$REGION \
  -var domain=$DOMAIN \
  -var admin_email=$ADMIN_EMAIL
```

If you manage the domain in Cloud DNS, add `-var dns_zone_name=<zone>` and Terraform creates
the A record too; otherwise step 4 does it by hand.

This creates the VPC, the chunks bucket, the GKE cluster with its KVM pool, Cloud SQL with
both databases, the secret shells, the four identities with their Workload Identity
bindings, both namespaces, the External Secrets relay, and the web static IP with a managed
certificate. Expect about 20 minutes; the cluster and the database are most of it.

Fetch cluster credentials for the later steps:

```sh
gcloud container clusters get-credentials \
  "$(terraform output -raw cluster_name)" \
  --region $REGION --project $PROJECT
```

## 3. Populate the secret shells

Secret material never enters Terraform state, so four shells are created empty and filled
here. **Do this before the Helm installs.** Until the shells have versions, the External
Secrets relay leaves the in-cluster Secrets unsynced and the pods crash-loop waiting for them.

```sh
# The machine bearer allow-list (one token to start).
echo -n "$(openssl rand -hex 32)" | \
  gcloud secrets versions add engram-auth-tokens --data-file=- --project=$PROJECT

# The 32-byte master key.
openssl rand -base64 32 | \
  gcloud secrets versions add engram-kek-master --data-file=- --project=$PROJECT

# The orchestrator's session-signing secret.
openssl rand -base64 48 | \
  gcloud secrets versions add engram-better-auth-secret --data-file=- --project=$PROJECT

# The egress-proxy CA pair (fleet-wide, ten-year cert).
openssl req -x509 -newkey rsa:4096 -nodes \
  -keyout /tmp/ca.key -out /tmp/ca.pem -days 3650 \
  -subj "/CN=Engram Egress Proxy CA"
gcloud secrets versions add engram-egress-ca-cert --data-file=/tmp/ca.pem --project=$PROJECT
gcloud secrets versions add engram-egress-ca-key  --data-file=/tmp/ca.key --project=$PROJECT
rm /tmp/ca.key /tmp/ca.pem
```

Check that the relay synced. Give it up to a minute, or delete the `external-secrets` pods to
force a resync:

```sh
kubectl get externalsecret -A
# every row: STATUS SecretSynced, READY True
```

## 4. DNS

If you passed `dns_zone_name`, skip this. Otherwise create one A record for `$DOMAIN`
pointing at:

```sh
terraform output -raw web_static_ip
```

The managed certificate provisions only after both the DNS record resolves and the Ingress
exists, and the Ingress is created by the Helm install in the next step. Until then the
certificate sits in `Provisioning` however long you wait; that is expected, not stuck. After
step 5, expect 15 to 40 minutes. `kubectl describe managedcertificate -n engrams
engram-web-cert` shows `Active` when it is done.

## 5. Helm, two releases

Render the values overlays Terraform derived, copy the static examples, and install:

```sh
cd deploy/terraform/gcp/quickstart
terraform output -raw engram_values     > /tmp/engram.tfvalues.yaml
terraform output -raw host_fleet_values > /tmp/host-fleet.tfvalues.yaml
cd ../../../..

cp deploy/helm/engram/values-gcp.yaml.example /tmp/engram-values.yaml
cp deploy/helm/engram-host-fleet/values-gcp.yaml.example /tmp/fleet-values.yaml
# The tfvalues overlays fill in every REPLACE_* the Terraform layer knows.
# Edit the /tmp copies only for taste: replica counts, resources.

helm install engram deploy/helm/engram \
  -n engrams -f /tmp/engram-values.yaml -f /tmp/engram.tfvalues.yaml

helm install hf deploy/helm/engram-host-fleet \
  -n engrams-hosts -f /tmp/fleet-values.yaml -f /tmp/host-fleet.tfvalues.yaml
```

If you pull the engrams images from a private registry, create a pull secret named
`ghcr-pull` in both namespaces first; the values examples reference that name.

The release names matter. The fleet values dial `engram-coordinator.engrams`, and the
quickstart's Workload Identity bindings expect the `hf-*` ServiceAccount names. If you use
different names, pass the matching `-var coordinator_ksa`, `host_fleet_ksa`, and
`operator_ksa` back in step 2.

Watch it come up:

```sh
kubectl get pods -n engrams        # coordinator, web, orchestrator Running
kubectl get pods -n engrams-hosts  # node-prep and host-agent per KVM node, plus the operator
kubectl logs -n engrams deploy/engram-coordinator | grep -i "host registered"
```

## 6. First login

Open `https://$DOMAIN` once the certificate is Active. Sign up with `$ADMIN_EMAIL`; the
bootstrap allow-list promotes that account to admin on its first sign-in. There is no
identity proxy in this posture: the orchestrator's login wall is the door. To put IAP in
front later, see `deploy/helm/engram/values-iap.yaml.example`; it splits the app host from
the machine surface (RPC, SSE, WebSockets) onto a second hostname, so plan on one more A
record at the same IP and a second managed certificate.

## 7. Enable a first image

Sessions need an enabled image. From Settings → Images, or with the CLI, register your
registry if it is private and enable an image. `eclipse-temurin:21-jre` is a good first pick.
Use a glibc-based image; the built-in harnesses do not run on Alpine, and a session on a musl
image hangs at "delivering". The enable pipeline materializes the image, boots a capture VM
on the KVM pool, and takes the base snapshot.

The harness also needs model credentials. Add your Claude Code token under Settings before
the first session, or connect OpenRouter under Settings → Model routers.

When the image shows **ready**, create a session against it. The agent's first reply is the
end-to-end proof.

## Teardown

```sh
helm uninstall engram -n engrams; helm uninstall hf -n engrams-hosts
cd deploy/terraform/gcp/quickstart && terraform destroy -var ... # the same vars as apply
```

`terraform destroy` refuses to remove the bucket unless it is empty; `force_destroy` is off
on purpose. `gsutil -m rm -r gs://<bucket>/*` first if you mean it.

## When something does not come up

| Symptom | Likely cause |
|---|---|
| Pods crash-loop on missing Secrets | Step 3 was skipped or the shells are empty. `kubectl get externalsecret -A` shows the sync state. |
| Host-agent pods Pending | The KVM pool is not up (C3 quota?), or the namespace lost its `pod-security.kubernetes.io/enforce=privileged` label. |
| Host-agent crash-loops on the egress CA | The CA shells are empty, or the `engram-host-egress-ca` Secret has not synced into `engrams-hosts`. |
| Certificate stuck in `Provisioning` | DNS does not resolve to the static IP yet; managed certificates wait for it. |
| `no capacity`, sessions queued forever | Hosts never registered. Check the host-agent logs for the coordinator endpoint and the bearer token, which must be an `engram-auth-tokens` entry. |
