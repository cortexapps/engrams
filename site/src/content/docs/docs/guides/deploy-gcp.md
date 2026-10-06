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
- An OAuth client for sign-in. People sign in with Google, and only accounts of your
  Google Workspace domain get in. In the Google Cloud console, open APIs & Services →
  Credentials, create an OAuth client ID of type "Web application", and add the authorized
  redirect URI `https://$DOMAIN/api/auth/oauth2/callback/sso`. If the console asks for a
  consent screen first, user type "Internal" limits the client to your organization. Keep
  the client ID and the client secret.
- C3 quota. The KVM pool is two `c3-standard-22` nodes by default, which is 44 vCPUs. Check
  `gcloud compute regions describe <region>` for `C3_CPUS` headroom, and that the region
  offers C3 at all, before you apply. A quota request can take a day.

Throughout, `PROJECT`, `REGION`, `DOMAIN` (for example `engrams.example.com`),
`ADMIN_EMAIL`, `EMAIL_DOMAIN` (the Workspace domain whose accounts may sign in, for example
`example.com`), `OAUTH_CLIENT_ID`, and `OAUTH_CLIENT_SECRET` are yours.

Run every command from a checkout of the newest release. The charts in a release install
the images of that release; a checkout of `main` can name a version that is not released.

```sh
git clone https://github.com/cortexapps/engrams.git && cd engrams
git checkout "$(git tag --list 'v*' --sort=-v:refname | head -n 1)"
```

To move to a later release afterwards, see [Upgrade engrams](../upgrade/).

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
  -var admin_email=$ADMIN_EMAIL \
  -var oauth_client_id=$OAUTH_CLIENT_ID \
  -var "oauth_allowed_domains=[\"$EMAIL_DOMAIN\"]"
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

Secret material never enters Terraform state, so the shells are created empty and filled
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

# The OAuth client secret.
echo -n "$OAUTH_CLIENT_SECRET" | \
  gcloud secrets versions add engram-oauth-client-secret --data-file=- --project=$PROJECT

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

The engrams images are public, so the install needs no pull secret. If you mirror them
into a private registry, create a pull secret in both namespaces first and name it under
`imagePullSecrets` in each values file.

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

Open `https://$DOMAIN` once the certificate is Active and choose **Continue with Google**.
Sign in as `$ADMIN_EMAIL`; the bootstrap allow-list promotes that account to admin on its
first sign-in. Every other account of `$EMAIL_DOMAIN` can sign in as a member, and no other
account can. There is no password sign-up.

There is no identity proxy in this posture: the orchestrator's login wall is the door. The
`orchestrator.auth` block of the chart holds every sign-in setting in one place:

- **Another identity provider.** Any provider with an OIDC discovery document works, such as
  Okta, Auth0, Keycloak, or Cognito. Pass `-var oauth_issuer=<issuer URL>` in step 2. The
  provider must return a verified email.
- **Who may sign in.** `oauth.allowedDomains` and `oauth.allowedEmails`. The check runs on
  every sign-in, so an account that leaves the list loses access at its next sign-in.
- **IAP instead.** `mode: iap`, with `deploy/helm/engram/values-iap.yaml.example`. It splits
  the app host from the machine surface (RPC, SSE, WebSockets) onto a second hostname, so
  plan on one more A record at the same IP and a second managed certificate.
- **Email and password.** `mode: password`. An address is not verified at sign-up, so anyone
  who can reach the page can register any address. **Do not** use it on a deployment the
  internet can reach.

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
| Google shows `redirect_uri_mismatch` | The OAuth client's authorized redirect URI is not exactly `https://$DOMAIN/api/auth/oauth2/callback/sso`. |
| The login page says the account does not have access | The account is not of an allowed domain. For Google the domain is the Workspace organization of the account, so a personal Google account with an address at your domain is refused. |
| Orchestrator `CreateContainerConfigError`, `secret "engram-oauth-client" not found` | The `engram-oauth-client-secret` shell is empty. Fill it (step 3) and wait for the relay. |
| `no capacity`, sessions queued forever | Hosts never registered. Check the host-agent logs for the coordinator endpoint and the bearer token, which must be an `engram-auth-tokens` entry. |
