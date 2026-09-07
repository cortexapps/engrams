# Deploy engrams on AWS, from zero

A fresh AWS account → a working engrams deployment on EKS. Every command is meant to run as written; if a step
needs knowledge this page doesn't give you, that's a bug in this
page — file it.

Topology background: [`deploy.md`](./deploy.md). Terraform layout:
[`deploy/terraform/aws/README.md`](../deploy/terraform/aws/README.md).
The GCP twin of this page is [`deploy-gcp.md`](./deploy-gcp.md);
the deliberate per-cloud differences are marked ⚡ below.

**Read this first — cost and quota**

- The KVM fleet defaults to **2 × `m8i.8xlarge`** (32 vCPUs each,
  nested virtualization) — the nearest shape above the GCP
  quickstart's `c3-standard-22`, roughly $3.4/hr for the pair
  (us-west-2 on-demand). Tear down when not in use.
- Check your **"Running On-Demand Standard instances" vCPU quota**
  covers the 64 fleet vCPUs plus the control-plane nodes; a fresh
  account's default may not. Request the increase (Service Quotas →
  EC2) before applying; grants can take hours to days.
- KVM needs Intel hardware: an 8th-gen Xeon-6 virtual shape (C8i,
  M8i, R8i or their -flex variants — the only families EC2 enables
  nested virtualization on, and only as a launch-time flag the
  quickstart sets) or bare metal (`*.metal`). No AMD, no Graviton,
  no 7th-gen virtual shapes.
- ⚡ **CPUID is a one-way door.** m8i is Granite Rapids; the GCP
  quickstart's C3 is Sapphire Rapids. Images baked on the default
  AWS fleet are GNR-pinned and their snapshots never restore on a
  C3 fleet (newer silicon never restores on older). If you run
  BOTH clouds and want one bake serving them, set
  `kvm_instance_type = "m7i.metal-24xl"` (Sapphire Rapids — metal,
  because EC2 does not enable nested virtualization on 7th-gen
  virtual shapes). That path is ~$10/hr for the pair and needs a
  metal quota ticket (192 vCPUs).

**What you need before starting**

- An AWS account with admin credentials configured (`aws sts
  get-caller-identity` works).
- `aws`, `terraform` ≥ 1.5.7, `helm` ≥ 3.10, `kubectl`, `openssl`, `jq`, `dig`.
- A domain you control (one validation CNAME + one final CNAME).

Throughout: `REGION`, `DOMAIN`, `ADMIN_EMAIL` are yours.

## 1. Quota check

```sh
aws service-quotas get-service-quota --region $REGION \
  --service-code ec2 --quota-code L-1216C47A \
  --query 'Quota.Value'   # Running On-Demand Standard instances (vCPUs)
```

Need ≥ 64 for the default fleet (plus the small control-plane
nodes); ≥ 192 if you chose the metal parity shape. Request more
before continuing if short.

## 2. Terraform: one apply

Write the inputs to `terraform.tfvars` once (gitignored; every later
`terraform` command reads it, so a fresh shell with `$REGION` unset
can never apply an empty region — the inputs are also validated
non-empty):

```sh
cd deploy/terraform/aws/quickstart
cat > terraform.tfvars <<EOF
region      = "$REGION"
domain      = "$DOMAIN"
admin_email = "$ADMIN_EMAIL"
EOF
terraform init
terraform apply
```

(Optional: `route53_zone_id = "<hosted zone>"` in the file automates
the ACM validation records; step 4 covers the manual alternative.)

This provisions the VPC (with the S3 gateway endpoint), the chunks
bucket, the EKS cluster + the self-managed KVM ASG, RDS (plus a
one-shot in-cluster Job creating the two logical databases), ⚡ the
coordinator's KEK as a real KMS key (`kek.provider: aws-kms`), the
secret shells, every IRSA role, both
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

⚡ Four shells + the CA pair. The coordinator's KEK is the KMS key,
so there is no coordinator KEK entry; the orchestrator still needs a
raw key because it seals its own tables in-process and has no KMS
path. **Do this before the Helm installs** — the relay leaves the
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

# The orchestrator's 32-byte sealing key (raw; the coordinator uses KMS).
aws secretsmanager put-secret-value --region $REGION \
  --secret-id engram/kek-master \
  --secret-string "$(openssl rand -base64 32)"

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

Step 2 *requested* an ACM certificate for `$DOMAIN`; ACM issues it
only after you prove control of the name with a DNS record. The ALB
(step 5) carries this certificate, so nothing serves HTTPS until it
is **Issued**.

If you passed `route53_zone_id`, Terraform created the validation
record and waited for issuance — skip to step 5. Otherwise print
the record and create it in your DNS zone:

```sh
terraform output acm_validation_records
```

The `name` is the fully-qualified record name. Most DNS consoles
(Cloud DNS, Cloudflare, Route53) take the name *relative to the
zone* and append the zone themselves: for zone `example.com` and
name `_abc.engrams.example.com.`, enter `_abc.engrams`. Keep every
label — a dropped label is the usual reason validation never
completes. Confirm the record from the zone's authoritative server,
then wait for ACM:

```sh
NAME=$(terraform output -json acm_validation_records | jq -r '.[0].name')
dig +short CNAME "$NAME" @"$(dig +short NS "${DOMAIN#*.}" | head -1)"
#   → the acm-validations.aws. target; empty means the record is missing or misnamed
aws acm wait certificate-validated --region $REGION \
  --certificate-arn "$(terraform output -raw acm_certificate_arn)"
#   returns once Status is ISSUED (usually within minutes of the record resolving)
```

You can run step 5 while the certificate is pending: the Helm
installs succeed, but the Load Balancer Controller cannot create
the HTTPS listener until issuance, so the Ingress has no hostname
and step 6 cannot start. The controller retries on its own once
the certificate is Issued.

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
the matching `*_ksa` values in `terraform.tfvars`.)

Watch it come up:

```sh
kubectl get nodes -l engram.io/kvm=true   # the KVM instances, 2 Ready
kubectl get pods -n engrams
kubectl get pods -n engrams-hosts          # hf-operator + one hf-host-agent per KVM node
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
cd deploy/terraform/aws/quickstart && terraform destroy   # reads terraform.tfvars
```

The bucket refuses destroy unless emptied
(`aws s3 rm s3://<bucket> --recursive` first).

## When something doesn't come up

| Symptom | Likely cause |
|---|---|
| KVM ASG stuck at 0/2 healthy | vCPU quota (step 1) or the chosen shape isn't offered in a chosen AZ (m8i and metal availability varies by AZ) — check the ASG activity history. |
| ASG instances InService but `kubectl get nodes -l engram.io/kvm=true` is empty and the DaemonSet shows 0 desired | The kubelets can't authenticate: the node role needs an EKS access entry (the kvm-nodegroup module creates it — `aws eks list-access-entries --cluster-name <cluster>` must list `<name>-node`). Nodes join on their own once it exists; no relaunch needed. |
| `terraform apply` fails on a `helm_release` with `no endpoints available for service "aws-load-balancer-webhook-service"` | The AWS Load Balancer Controller registers a fail-closed webhook on every Service before its pods are ready; the quickstart serializes ESO behind it, so this means the controller itself is unhealthy (`kubectl get pods -n kube-system`). Fix that, then re-run `apply`. |
| Pods CrashLoop on missing Secrets | Step 3 skipped — `kubectl get externalsecret -A` shows the sync state. |
| Orchestrator `CreateContainerConfigError`: `couldn't find key ENGRAM_KEK_MASTER_KEY` | The `engram/kek-master` shell is empty, or the relay has not re-synced since you filled it (`kubectl annotate externalsecret -n engrams engram-orchestrator-secrets force-sync=$(date +%s)`). |
| Host-agent CrashLoops on the egress CA | The CA shells are empty, or the fleet IRSA role can't read them (it is scoped to exactly those two ARNs). |
| Ingress has no ALB hostname, or `kubectl delete ingress` hangs on its finalizer | The AWS Load Balancer Controller isn't healthy (`kubectl get pods -n kube-system`), or the ACM cert isn't Issued yet. A controller CrashLooping on `failed to fetch VPC ID from instance metadata` is missing its explicit `vpcId`/`region` (the quickstart sets both; node IMDS hop limit 1 blocks metadata for pods). |
| ACM cert stays `PENDING_VALIDATION` | The validation CNAME is missing or misnamed (a dropped label when the console appended the zone). `dig` it against the zone's authoritative nameserver (step 4); fix the name — ACM re-checks on its own. |
| `no capacity` / sessions queued forever | Hosts never registered — check host-agent logs for the coordinator endpoint + bearer. |
| Enabling an image fails with `Error creating KVM object: No such file or directory` | `/dev/kvm` is missing on the host: the instance was launched without nested virtualization (a launch template from before the flag, or a 7th-gen shape). `kubectl exec -n engrams-hosts <host-agent pod> -- grep -c vmx /proc/cpuinfo` prints 0. The flag is launch-time only: `terraform apply`, then replace each instance with `aws autoscaling terminate-instance-in-auto-scaling-group --instance-id <id> --no-should-decrement-desired-capacity`. |
