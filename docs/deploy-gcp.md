# Deploy engrams on GCP, from zero

A fresh GCP project → a working engrams deployment. Every command is
meant to run as written; if a step needs knowledge this page doesn't
give you, that's a bug in this page — file it.

Topology background: [`deploy.md`](./deploy.md). Terraform layout:
[`deploy/terraform/gcp/README.md`](../deploy/terraform/gcp/README.md).

**What you need before starting**

- A GCP project with billing, and `Owner` (or equivalent) on it.
- `gcloud`, `terraform` ≥ 1.5, `helm` ≥ 3.10, `kubectl`, `openssl`.
- A domain you control (you will create one A record).
- **An OAuth client for sign-in.** People sign in with Google, and
  only accounts of your Google Workspace domain get in. In the
  Google Cloud console: APIs & Services → Credentials → OAuth client
  ID → "Web application", with the authorized redirect URI
  `https://$DOMAIN/api/auth/oauth2/callback/sso`. If the console
  asks for a consent screen first, user type "Internal" limits the
  client to your organization. Keep the client ID and the client
  secret.
- **C3 quota**: the KVM pool is 2 × `c3-standard-22` (44 vCPUs) by
  default. Check `gcloud compute regions describe <region>` for
  `C3_CPUS` headroom, and that your region offers C3 at all, before
  applying — a quota request can take a day.

Throughout: `PROJECT`, `REGION`, `DOMAIN` (e.g.
`engrams.example.com`), `ADMIN_EMAIL`, `EMAIL_DOMAIN` (the Workspace
domain whose accounts may sign in, e.g. `example.com`),
`OAUTH_CLIENT_ID`, `OAUTH_CLIENT_SECRET` are yours.

Run every command from a checkout of the newest release. The charts in
a release install the images of that release; a checkout of `main` can
name a version that is not released.

```sh
git clone https://github.com/cortexapps/engrams.git && cd engrams
git checkout "$(git tag --list 'v*' --sort=-v:refname | head -n 1)"
```

To move to a later release afterwards, see
[Releases and upgrades](./deploy.md#releases-and-upgrades).

## 1. Enable the APIs

```sh
gcloud services enable --project=$PROJECT \
  container.googleapis.com compute.googleapis.com \
  sqladmin.googleapis.com servicenetworking.googleapis.com \
  secretmanager.googleapis.com storage.googleapis.com \
  iam.googleapis.com dns.googleapis.com
```

## 2. Terraform: one apply

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

(Optional: `-var dns_zone_name=<your Cloud DNS zone>` creates the A
record too; step 4 covers the manual alternative.)

This provisions the VPC, the chunks bucket, the GKE cluster + the
nested-virt KVM pool, Cloud SQL (both logical databases), the secret
shells, all four identities with Workload Identity bindings, both
namespaces, the External Secrets relay, and the web static IP +
managed certificate. Expect ~20 minutes, dominated by the cluster
and the database.

Grab cluster credentials for the later steps:

```sh
gcloud container clusters get-credentials \
  "$(terraform output -raw cluster_name)" \
  --region $REGION --project $PROJECT
```

## 3. Populate the secret shells

Secret material never enters Terraform state, so four shells are
created empty and populated here. **Do this before the Helm installs**
— the External Secrets relay leaves the in-cluster Secrets unsynced
until the shells have versions, and pods CrashLoop on the missing
Secrets until the first sync.

```sh
# The machine bearer allow-list (one token to start).
echo -n "$(openssl rand -hex 32)" | \
  gcloud secrets versions add engram-auth-tokens --data-file=- --project=$PROJECT

# The 32-byte master KEK (env-var provider).
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

Verify the relay synced (give it up to a minute, or delete the
`external-secrets` pods to force a resync):

```sh
kubectl get externalsecret -A
# every row should show STATUS SecretSynced / READY True
```

## 4. DNS

If you passed `dns_zone_name`, skip this. Otherwise create one A
record for `$DOMAIN` pointing at:

```sh
terraform output -raw web_static_ip
```

The Google-managed certificate provisions only after BOTH the DNS
record resolves AND the Ingress exists — and the Ingress is created
by the helm install in step 5. Until then the cert sits in
`Provisioning` no matter how long you wait; that is expected, not
stuck. After step 5, expect ~15–40 minutes. Check with
`kubectl describe managedcertificate -n engrams engram-web-cert`
(status `Active` when done).

## 5. Helm: the two releases

Render the TF-derived values overlays, copy the static examples, and
install:

```sh
cd deploy/terraform/gcp/quickstart
terraform output -raw engram_values     > /tmp/engram.tfvalues.yaml
terraform output -raw host_fleet_values > /tmp/host-fleet.tfvalues.yaml
cd ../../../..

cp deploy/helm/engram/values-gcp.yaml.example /tmp/engram-values.yaml
cp deploy/helm/engram-host-fleet/values-gcp.yaml.example /tmp/fleet-values.yaml
# The tfvalues overlays override every REPLACE_* the TF layer knows;
# edit the /tmp copies only for taste (replica counts, resources).

helm install engram deploy/helm/engram \
  -n engrams -f /tmp/engram-values.yaml -f /tmp/engram.tfvalues.yaml

helm install hf deploy/helm/engram-host-fleet \
  -n engrams-hosts -f /tmp/fleet-values.yaml -f /tmp/host-fleet.tfvalues.yaml
```

(The release names matter: the fleet values dial
`engram-coordinator.engrams`, and the quickstart's Workload Identity
bindings expect the `hf-*` ServiceAccount names. Different names →
pass the matching `-var coordinator_ksa/host_fleet_ksa/operator_ksa`
back in step 2.)

Watch it come up:

```sh
kubectl get pods -n engrams        # coordinator, web, orchestrator Running
kubectl get pods -n engrams-hosts  # node-prep + host-agent per KVM node, operator
kubectl logs -n engrams deploy/engram-coordinator | grep -i "host registered"
```

## 6. First login

Open `https://$DOMAIN` (after the cert is Active) and choose
**Continue with Google**. Sign in as `$ADMIN_EMAIL` — the bootstrap
allow-list promotes it to admin on first sign-in. Every other
account of `$EMAIL_DOMAIN` can sign in as a member, and no other
account can. There is no password sign-up.

There is no identity proxy in this posture: the orchestrator's login
wall is the auth door. Every sign-in setting is in the chart's
`orchestrator.auth` block (`deploy/helm/engram/values.yaml`):

- **Another identity provider**: any provider with an OIDC discovery
  document (Okta, Auth0, Keycloak, Cognito). Pass
  `-var oauth_issuer=<issuer URL>` in step 2. The provider must
  return a verified email.
- **Who may sign in**: `oauth.allowedDomains` / `oauth.allowedEmails`.
  The check runs on every sign-in, so an account that leaves the
  list loses access at its next sign-in.
- **IAP instead**: `mode: iap`, with
  `deploy/helm/engram/values-iap.yaml.example` — the split-host
  layout: IAP guards the app host while the machine surface (RPC,
  SSE, WebSockets) moves to a second hostname, so plan on one extra
  A record (`api.<domain>`, same IP) and a second managed cert.
- **Email + password**: `mode: password`. An address is not verified
  at sign-up, so anyone who can reach the page can register any
  address. **Do not** use it on a deployment the internet can reach.

## 7. Enable a first image

Sessions need an enabled image (a base snapshot baked on this
fleet). From Settings → Images (or the `engrams` CLI), register your
registry if it's private and enable an image — `eclipse-temurin:21-jre`
is a good first pick. **Use a glibc-based image**: Alpine/musl images
are not yet supported by the built-in harnesses (the bundled agent
CLI is a glibc binary; on a musl guest it exits immediately and the
session hangs at "delivering…"). The enable pipeline materializes
the image, boots a capture VM on the KVM pool, and snapshots.

The harness also needs model credentials: add your Claude
credentials under Settings → Harness environment (e.g.
`CLAUDE_CODE_OAUTH_TOKEN`) before the first session, or connect
OpenRouter under Settings → Model routers to use routed models.

When the image shows **ready**, create a session against it — the
agent's first reply is the end-to-end proof.

## Teardown

```sh
helm uninstall engram -n engrams; helm uninstall hf -n engrams-hosts
cd deploy/terraform/gcp/quickstart && terraform destroy -var ... # same vars as apply
```

`terraform destroy` refuses on the bucket unless it's empty
(`force_destroy` is off by design); `gsutil -m rm -r gs://<bucket>/*`
first if you mean it.

## When something doesn't come up

| Symptom | Likely cause |
|---|---|
| Pods CrashLoop on missing Secrets | Step 3 skipped or shells empty — `kubectl get externalsecret -A` shows the sync state. |
| Host-agent pods Pending | The KVM pool isn't up (C3 quota?) or the namespace lost its `pod-security.kubernetes.io/enforce=privileged` label. |
| Host-agent CrashLoops on the egress CA | The CA shells are empty (step 3), or the `engram-host-egress-ca` Secret hasn't synced into `engrams-hosts`. |
| Cert stuck `Provisioning` | DNS doesn't resolve to the static IP yet; managed certs wait for it. |
| Google shows `redirect_uri_mismatch` | The OAuth client's authorized redirect URI is not exactly `https://$DOMAIN/api/auth/oauth2/callback/sso`. |
| The login page says the account does not have access | The account is not of an allowed domain. For Google the domain is the Workspace organization of the account, so a personal Google account with an address at your domain is refused. |
| Orchestrator `CreateContainerConfigError`: `secret "engram-oauth-client" not found` | The `engram-oauth-client-secret` shell is empty — fill it (step 3) and wait for the relay. |
| `no capacity` / sessions queued forever | Hosts never registered — check the host-agent logs for the coordinator endpoint + bearer (must be an `engram-auth-tokens` entry). |
