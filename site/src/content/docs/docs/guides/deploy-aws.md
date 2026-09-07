---
title: Deploy on AWS
description: A fresh AWS account to a working engrams deployment on EKS, command by command.
sidebar:
  order: 3
---

This page takes a fresh AWS account to a working deployment on EKS. Every command is meant
to run as written. If a step needs something this page does not tell you, that is a bug in
this page; please file it. The GCP twin is [Deploy on GCP](../deploy-gcp/); the places where
AWS differs on purpose are marked ⚡.

**Read this first: cost and quota**

- The KVM fleet defaults to two `m8i.8xlarge` instances (32 vCPUs each, nested
  virtualization), the nearest shape above the GCP quickstart's `c3-standard-22`, at roughly
  $3.40 an hour for the pair (us-west-2 on-demand). Tear it down when you are not using it.
- Check that your "Running On-Demand Standard instances" vCPU quota covers the 64 fleet
  vCPUs plus the control-plane nodes; a fresh account's default may not. Request the increase
  under Service Quotas → EC2 before you apply. Grants can take hours to days.
- KVM needs Intel hardware: an 8th-generation Xeon 6 virtual shape (C8i, M8i, R8i, or their
  flex variants; the only families EC2 enables nested virtualization on, and only as a
  launch-time flag the quickstart sets) or bare metal (`*.metal`). No AMD, no Graviton, and
  no 7th-generation virtual shapes.
- ⚡ The CPU platform is a one-way door. m8i is Granite Rapids; the GCP quickstart's C3 is
  Sapphire Rapids. Images enabled on the default AWS fleet never restore on a C3 fleet, because
  newer silicon never restores on older. If you run both clouds and want one enable to serve
  them, set `kvm_instance_type = "m7i.metal-24xl"` (Sapphire Rapids; metal, because EC2 does
  not enable nested virtualization on 7th-generation virtual shapes). That path is about $10
  an hour for the pair and needs a metal quota ticket for 192 vCPUs.

**Before you start**

- An AWS account with admin credentials configured; `aws sts get-caller-identity` works.
- `aws`, `terraform` 1.5.7 or later, `helm` 3.10 or later, `kubectl`, `openssl`, `jq`, and `dig`.
- A domain you control. You will create one validation CNAME and one final CNAME.

Throughout, `REGION`, `DOMAIN`, and `ADMIN_EMAIL` are yours.

## 1. Quota check

```sh
aws service-quotas get-service-quota --region $REGION \
  --service-code ec2 --quota-code L-1216C47A \
  --query 'Quota.Value'   # Running On-Demand Standard instances (vCPUs)
```

You need at least 64 for the default fleet plus the small control-plane nodes, or at least
192 if you chose the metal parity shape. Request more before you continue if you are short.

## 2. Terraform, one apply

Write the inputs to `terraform.tfvars` once. The file is gitignored, and every later
`terraform` command reads it, so a fresh shell with `$REGION` unset can never apply an empty
region. The inputs are also validated non-empty.

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

If the domain is in Route 53, add `route53_zone_id = "<hosted zone>"` to the file and Terraform creates
the certificate validation records; otherwise step 4 does it by hand.

This creates the VPC with an S3 gateway endpoint, the chunks bucket, the EKS cluster with the
self-managed KVM auto-scaling group, RDS plus a one-shot job that creates the two databases,
⚡ the coordinator's master key as a real KMS key, the secret shells, every IRSA role, both namespaces, the AWS Load Balancer Controller, the External
Secrets relay, and the ACM certificate request. Expect about 20 minutes; the EKS control
plane is the slow part, and metal instances take longer still.

Cluster credentials:

```sh
aws eks update-kubeconfig --region $REGION \
  --name "$(terraform output -raw cluster_name)"
```

## 3. Populate the secret shells

⚡ Four shells plus the CA pair. The coordinator's master key is the KMS key, so it has no
entry here; the orchestrator still needs a raw key, because it seals its own tables in-process
and has no KMS path. **Do this before the Helm installs.** Until the shells have values, the
relay leaves the in-cluster Secrets unsynced and the pods crash-loop waiting for them.

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

# The egress-proxy CA pair (fleet-wide, ten-year cert). The host agent reads
# these straight from Secrets Manager over IRSA; there is no in-cluster relay
# for the CA on AWS.
openssl req -x509 -newkey rsa:4096 -nodes \
  -keyout /tmp/ca.key -out /tmp/ca.pem -days 3650 \
  -subj "/CN=Engram Egress Proxy CA"
aws secretsmanager put-secret-value --region $REGION \
  --secret-id engram/egress-ca-cert --secret-string file:///tmp/ca.pem
aws secretsmanager put-secret-value --region $REGION \
  --secret-id engram/egress-ca-key --secret-string file:///tmp/ca.key
rm /tmp/ca.key /tmp/ca.pem
```

Check that the relay synced:

```sh
kubectl get externalsecret -A
# every row: STATUS SecretSynced, READY True
```

## 4. Certificate validation

Step 2 requested an ACM certificate for `$DOMAIN`. ACM issues it only after you prove control
of the name with a DNS record. The load balancer in step 5 carries this certificate, so
nothing serves HTTPS until it is **Issued**.

If you passed `route53_zone_id`, Terraform created the validation record and waited for
issuance; skip to step 5. Otherwise print the record and create it in your DNS zone:

```sh
terraform output acm_validation_records
```

The `name` is the fully qualified record name. Most DNS consoles (Cloud DNS, Cloudflare,
Route 53) take the name relative to the zone and append the zone themselves: for zone
`example.com` and name `_abc.engrams.example.com.`, enter `_abc.engrams`. Keep every label;
a dropped label is the usual reason validation never completes. Confirm the record from the
zone's authoritative server, then wait for ACM:

```sh
NAME=$(terraform output -json acm_validation_records | jq -r '.[0].name')
dig +short CNAME "$NAME" @"$(dig +short NS "${DOMAIN#*.}" | head -1)"
#   → the acm-validations.aws. target; empty means the record is missing or misnamed
aws acm wait certificate-validated --region $REGION \
  --certificate-arn "$(terraform output -raw acm_certificate_arn)"
#   returns once Status is ISSUED, usually within minutes of the record resolving
```

You can run step 5 while the certificate is pending. The Helm installs succeed, but the Load
Balancer Controller cannot create the HTTPS listener until issuance, so the Ingress has no
hostname and step 6 cannot start. The controller retries on its own once the certificate is
Issued.

## 5. Helm, two releases

```sh
cd deploy/terraform/aws/quickstart
terraform output -raw engram_values     > /tmp/engram.tfvalues.yaml
terraform output -raw host_fleet_values > /tmp/host-fleet.tfvalues.yaml
cd ../../../..

cp deploy/helm/engram/values-aws.yaml.example /tmp/engram-values.yaml
cp deploy/helm/engram-host-fleet/values-aws.yaml.example /tmp/fleet-values.yaml
# The tfvalues overlays fill in every REPLACE_* the Terraform layer knows.

helm install engram deploy/helm/engram \
  -n engrams -f /tmp/engram-values.yaml -f /tmp/engram.tfvalues.yaml

helm install hf deploy/helm/engram-host-fleet \
  -n engrams-hosts -f /tmp/fleet-values.yaml -f /tmp/host-fleet.tfvalues.yaml
```

If you pull the engrams images from a private registry, create a pull secret named
`ghcr-pull` in both namespaces first; the values examples reference that name.

Release names matter here as on GCP: the fleet dials `engram-coordinator.engrams`, and the
IRSA trust policies name the `hf-*` ServiceAccounts. Different names mean re-applying step 2
with the matching `*_ksa` values in `terraform.tfvars`.

Watch it come up:

```sh
kubectl get nodes -l engram.io/kvm=true   # the KVM instances, 2 Ready
kubectl get pods -n engrams
kubectl get pods -n engrams-hosts          # hf-operator + one hf-host-agent per KVM node
kubectl logs -n engrams deploy/engram-coordinator | grep -i "host registered"
```

## 6. The final CNAME

⚡ The load balancer's hostname exists only after the web Ingress reconciles, so it is the one
value Terraform cannot print up front:

```sh
kubectl get ingress -n engrams engram-web \
  -o jsonpath='{.status.loadBalancer.ingress[0].hostname}'
```

CNAME `$DOMAIN` to that hostname. `https://$DOMAIN` then serves the app. The ALB carries the
ACM certificate and the 3600-second idle timeout the overlay sets; the 60-second default would
cut every quiet SSE and WebSocket connection.

## 7. First login and first image

Sign up with `$ADMIN_EMAIL`; the bootstrap allow-list promotes it to admin. ⚡ Do not put ALB
OIDC or Cognito authentication in front of the app: it breaks CORS preflights and WebSockets.
The orchestrator's login wall is the door.

Enable a first image. `eclipse-temurin:21-jre` is a good first pick; use a glibc-based image,
because the built-in harnesses do not run on Alpine. Add your model credentials under Settings,
or connect OpenRouter under Settings → Model routers, and create a session against the image.
The agent's first reply is the end-to-end proof.

⚡ If you are reusing images enabled on a GCP fleet, the default platforms do not match (m8i
is Granite Rapids, C3 is Sapphire Rapids); enable them fresh on this fleet. Cross-cloud reuse
works only on the `m7i.metal-24xl` parity shape.

## Teardown

The fleet bills while it idles, so tear it down promptly:

```sh
helm uninstall engram -n engrams; helm uninstall hf -n engrams-hosts
# Delete any load balancers the controller created before destroying the VPC:
kubectl delete ingress -n engrams --all
cd deploy/terraform/aws/quickstart && terraform destroy   # reads terraform.tfvars
```

The bucket refuses to be destroyed unless it is empty; `aws s3 rm s3://<bucket> --recursive`
first.

## When something does not come up

| Symptom | Likely cause |
|---|---|
| KVM auto-scaling group stuck at 0 of 2 healthy | vCPU quota (step 1), or the chosen shape is not offered in a chosen availability zone; m8i and metal availability varies by zone. Check the group's activity history. |
| Instances are InService but `kubectl get nodes -l engram.io/kvm=true` is empty and the DaemonSet shows 0 desired | The kubelets cannot authenticate. The node role needs an EKS access entry, which the kvm-nodegroup module creates; `aws eks list-access-entries --cluster-name <cluster>` must list `<name>-node`. Nodes join on their own once it exists, without a relaunch. |
| `terraform apply` fails on a Helm release with `no endpoints available for service "aws-load-balancer-webhook-service"` | The AWS Load Balancer Controller registers a fail-closed webhook on every Service before its pods are ready. The quickstart installs External Secrets after the controller, so this means the controller itself is unhealthy (`kubectl get pods -n kube-system`). Fix that, then run `apply` again. |
| Pods crash-loop on missing Secrets | Step 3 was skipped. `kubectl get externalsecret -A` shows the sync state. |
| Orchestrator `CreateContainerConfigError`, `couldn't find key ENGRAM_KEK_MASTER_KEY` | The `engram/kek-master` shell is empty, or the relay has not re-synced since you filled it. Run `kubectl annotate externalsecret -n engrams engram-orchestrator-secrets force-sync=$(date +%s)`. |
| Host-agent crash-loops on the egress CA | The CA shells are empty, or the fleet's IRSA role cannot read them; it is scoped to exactly those two ARNs. |
| Ingress has no load balancer hostname, or `kubectl delete ingress` hangs on its finalizer | The AWS Load Balancer Controller is not healthy (`kubectl get pods -n kube-system`), or the ACM certificate is not Issued yet. A controller crash-looping on `failed to fetch VPC ID from instance metadata` is missing its explicit `vpcId` and `region`; the quickstart sets both, because the node IMDS hop limit of 1 blocks metadata for pods. |
| ACM certificate stays `PENDING_VALIDATION` | The validation CNAME is missing or misnamed, usually a dropped label when the console appended the zone. Query it against the zone's authoritative nameserver (step 4) and fix the name; ACM re-checks on its own. |
| `no capacity`, sessions queued forever | Hosts never registered. Check the host-agent logs for the coordinator endpoint and the bearer token. |
| Enabling an image fails with `Error creating KVM object: No such file or directory` | `/dev/kvm` is missing on the host: the instance was launched without nested virtualization (a launch template from before the flag, or a 7th-generation shape). `kubectl exec -n engrams-hosts <host-agent pod> -- grep -c vmx /proc/cpuinfo` prints 0. The flag is launch-time only: run `terraform apply`, then replace each instance with `aws autoscaling terminate-instance-in-auto-scaling-group --instance-id <id> --no-should-decrement-desired-capacity`. |
