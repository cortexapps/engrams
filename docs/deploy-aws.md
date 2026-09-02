# Deploy engrams on AWS, from zero

A fresh AWS account → a working engrams deployment on EKS. Every command is meant to run as written; if a step
needs knowledge this page doesn't give you, that's a bug in this
page — file it.

Topology background: [`deploy.md`](./deploy.md). Terraform layout:
[`deploy/terraform/aws/README.md`](../deploy/terraform/aws/README.md).
The GCP twin of this page is [`deploy-gcp.md`](./deploy-gcp.md);
the deliberate per-cloud differences are marked ⚡ below.

**Read this first — cost and quota**

- The KVM fleet defaults to **2 × `m8i.6xlarge`** (24 vCPUs each,
  nested virtualization) — the shape twin of the GCP quickstart's
  `c3-standard-22`, roughly $2.9/hr for the pair. Tear down when
  not in use.
- Check your **"Running On-Demand Standard instances" vCPU quota**
  covers the 48 fleet vCPUs plus the control-plane nodes; a fresh
  account's default may not. Request the increase (Service Quotas →
  EC2) before applying; grants can take hours to days.
- KVM needs Intel hardware: the Xeon-6 C8i/M8i/R8i virtual shapes
  (nested virtualization) or bare metal (`*.metal`). No AMD, no
  Graviton.
- ⚡ **CPUID is a one-way door.** m8i is Granite Rapids; the GCP
  quickstart's C3 is Sapphire Rapids. Images baked on the default
  AWS fleet are GNR-pinned and their snapshots never restore on a
  C3 fleet (newer silicon never restores on older). If you run
  BOTH clouds and want one bake serving them, set
  `kvm_instance_type = "m7i.metal-24xl"` (Sapphire Rapids — metal
  because m7i has no nested virt). That path is ~$10+/hr for the
  pair and needs 192 vCPUs of quota.

**What you need before starting**

- An AWS account with admin credentials configured (`aws sts
  get-caller-identity` works).
- `aws`, `terraform` ≥ 1.5, `helm` ≥ 3.10, `kubectl`, `openssl`.
- A domain you control (one validation CNAME + one final CNAME).

Throughout: `REGION`, `DOMAIN`, `ADMIN_EMAIL` are yours.

## 1. Quota check

```sh
aws service-quotas get-service-quota --region $REGION \
  --service-code ec2 --quota-code L-1216C47A \
  --query 'Quota.Value'   # Running On-Demand Standard instances (vCPUs)
```

Need ≥ 48 for the default fleet (plus the small control-plane
nodes); ≥ 192 if you chose the metal parity shape. Request more
before continuing if short.

## 2. Terraform: one apply

```sh
cd deploy/terraform/aws/quickstart
terraform init
terraform apply \
  -var region=$REGION \
  -var domain=$DOMAIN \
  -var admin_email=$ADMIN_EMAIL
```

(Optional: `-var route53_zone_id=<hosted zone>` automates the ACM
validation records; step 4 covers the manual alternative.)

This provisions the VPC (with the S3 gateway endpoint), the chunks
bucket, the EKS cluster + the self-managed KVM ASG, RDS (plus a
one-shot in-cluster Job creating the two logical databases), ⚡ the
KEK as a real KMS key (`kek.provider: aws-kms` — no KEK secret to
populate on AWS), the secret shells, every IRSA role, both
namespaces, the AWS Load Balancer Controller, the External Secrets
relay, and the ACM certificate request. Expect ~20 minutes; the
EKS control plane is the slow tail (metal instances take longer
still, if you chose that shape).

Cluster credentials:

```sh
aws eks update-kubeconfig --region $REGION \
  --name "$(terraform output -raw cluster_name)"
```

## 3. Populate the secret shells

⚡ Three shells + the CA pair — no KEK entry (the KEK is the KMS
key). **Do this before the Helm installs** — the relay leaves the
in-cluster Secrets unsynced until the shells have values, and pods
CrashLoop until the first sync.

```sh
# The machine bearer allow-list (one token to start).
aws secretsmanager put-secret-value --region $REGION \
  --secret-id engram/auth-tokens \
  --secret-string "$(openssl rand -hex 32)"

# The orchestrator's session-signing secret.
aws secretsmanager put-secret-value --region $REGION \
  --secret-id engram/better-auth-secret \
  --secret-string "$(openssl rand -base64 48)"

# The egress-proxy CA pair (fleet-wide, ten-year cert). ⚡ The
# host-agent reads these DIRECTLY from Secrets Manager over IRSA
# (caSource=aws-secrets-manager) — no in-cluster relay for the CA.
openssl req -x509 -newkey rsa:4096 -nodes \
  -keyout /tmp/ca.key -out /tmp/ca.pem -days 3650 \
  -subj "/CN=Engram Egress Proxy CA"
aws secretsmanager put-secret-value --region $REGION \
  --secret-id engram/egress-ca-cert --secret-string file:///tmp/ca.pem
aws secretsmanager put-secret-value --region $REGION \
  --secret-id engram/egress-ca-key --secret-string file:///tmp/ca.key
rm /tmp/ca.key /tmp/ca.pem
```

Verify the relay synced:

```sh
kubectl get externalsecret -A
# every row: STATUS SecretSynced / READY True
```

## 4. Certificate validation

If you passed `route53_zone_id`, the validation records were created
— wait for `terraform output` / the ACM console to show **Issued**.
Otherwise create the CNAMEs from:

```sh
terraform output acm_validation_records
```

Issuance follows within minutes of the records resolving.

## 5. Helm: the two releases

```sh
cd deploy/terraform/aws/quickstart
terraform output -raw engram_values     > /tmp/engram.tfvalues.yaml
terraform output -raw host_fleet_values > /tmp/host-fleet.tfvalues.yaml
cd ../../../..

cp deploy/helm/engram/values-aws.yaml.example /tmp/engram-values.yaml
cp deploy/helm/engram-host-fleet/values-aws.yaml.example /tmp/fleet-values.yaml
# The tfvalues overlays override every REPLACE_* the TF layer knows.

# While the engrams repository is private, its GHCR images need a
# pull secret in BOTH namespaces (a GitHub PAT with read:packages) —
# the values examples reference the name `ghcr-pull`:
kubectl create secret docker-registry ghcr-pull -n engrams \
  --docker-server=ghcr.io --docker-username=<gh-user> --docker-password=<PAT>
kubectl create secret docker-registry ghcr-pull -n engrams-hosts \
  --docker-server=ghcr.io --docker-username=<gh-user> --docker-password=<PAT>

helm install engram deploy/helm/engram \
  -n engrams -f /tmp/engram-values.yaml -f /tmp/engram.tfvalues.yaml

helm install hf deploy/helm/engram-host-fleet \
  -n engrams-hosts -f /tmp/fleet-values.yaml -f /tmp/host-fleet.tfvalues.yaml
```

(Release names matter — same coupling as the GCP page: the fleet
dials `engram-coordinator.engrams`, and the IRSA trust policies name
the `hf-*` ServiceAccounts. Different names → re-apply step 2 with
the matching `-var *_ksa` values.)

Watch it come up:

```sh
kubectl get pods -n engrams
kubectl get pods -n engrams-hosts
kubectl logs -n engrams deploy/engram-coordinator | grep -i "host registered"
```

## 6. The final CNAME

⚡ The ALB hostname exists only after the web Ingress reconciles —
the one output Terraform cannot render up front:

```sh
kubectl get ingress -n engrams engram-web \
  -o jsonpath='{.status.loadBalancer.ingress[0].hostname}'
```

CNAME `$DOMAIN` to that hostname. Then `https://$DOMAIN` serves the
app (the ALB carries the ACM cert and the 3600 s idle timeout the
overlay sets — the 60 s default would cut every quiet SSE/WebSocket
leg).

## 7. First login + first image

Sign up with `$ADMIN_EMAIL` (bootstrap-promoted to admin — the
login wall; ⚡ do NOT put ALB OIDC/Cognito auth in front of
the app: it breaks CORS preflights and WebSockets the same way IAP
does). Then enable a first image — `eclipse-temurin:21-jre` is a
good first pick; **use a glibc-based image** (Alpine/musl images are
not yet supported by the built-in harnesses), and add your model
credentials (Settings → Harness environment, or connect OpenRouter
under Settings → Model routers) — and create a session against it:
the agent's first reply is the end-to-end proof.

⚡ If you are reusing images baked on a GCP fleet: the default
platforms do NOT match (AWS m8i = Granite Rapids, GCP C3 =
Sapphire Rapids) — bake fresh on this fleet. Cross-cloud reuse
works only on the `m7i.metal-24xl` parity shape (Sapphire Rapids
both sides).

## Teardown

The fleet bills while it idles — tear down promptly:

```sh
helm uninstall engram -n engrams; helm uninstall hf -n engrams-hosts
# Delete any ALBs the controller created before destroying the VPC:
kubectl delete ingress -n engrams --all
cd deploy/terraform/aws/quickstart && terraform destroy -var ... # same vars as apply
```

The bucket refuses destroy unless emptied
(`aws s3 rm s3://<bucket> --recursive` first).

## When something doesn't come up

| Symptom | Likely cause |
|---|---|
| KVM ASG stuck at 0/2 healthy | vCPU quota (step 1) or the chosen shape isn't offered in a chosen AZ (m8i and metal availability varies by AZ) — check the ASG activity history. |
| Pods CrashLoop on missing Secrets | Step 3 skipped — `kubectl get externalsecret -A` shows the sync state. |
| Host-agent CrashLoops on the egress CA | The CA shells are empty, or the fleet IRSA role can't read them (it is scoped to exactly those two ARNs). |
| Ingress has no ALB hostname | The AWS Load Balancer Controller isn't healthy (`kubectl get pods -n kube-system`), or the ACM cert isn't Issued yet. |
| `no capacity` / sessions queued forever | Hosts never registered — check host-agent logs for the coordinator endpoint + bearer. |
