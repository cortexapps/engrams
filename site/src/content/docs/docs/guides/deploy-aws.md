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

- The KVM fleet defaults to two `m8i.6xlarge` instances (24 vCPUs each, nested
  virtualization), the shape twin of the GCP quickstart's `c3-standard-22`, at roughly $2.90
  an hour for the pair. Tear it down when you are not using it.
- Check that your "Running On-Demand Standard instances" vCPU quota covers the 48 fleet
  vCPUs plus the control-plane nodes; a fresh account's default may not. Request the increase
  under Service Quotas → EC2 before you apply. Grants can take hours to days.
- KVM needs Intel hardware: the Xeon 6 shapes (C8i, M8i, R8i) with nested virtualization, or
  bare metal (`*.metal`). No AMD, no Graviton.
- ⚡ The CPU platform is a one-way door. m8i is Granite Rapids; the GCP quickstart's C3 is
  Sapphire Rapids. Images enabled on the default AWS fleet never restore on a C3 fleet, because
  newer silicon never restores on older. If you run both clouds and want one enable to serve
  them, set `kvm_instance_type = "m7i.metal-24xl"` (Sapphire Rapids; metal because m7i has no
  nested virtualization). That path is over $10 an hour for the pair and needs 192 vCPUs of
  quota.

**Before you start**

- An AWS account with admin credentials configured; `aws sts get-caller-identity` works.
- `aws`, `terraform` 1.5 or later, `helm` 3.10 or later, `kubectl`, and `openssl`.
- A domain you control. You will create one validation CNAME and one final CNAME.

Throughout, `REGION`, `DOMAIN`, and `ADMIN_EMAIL` are yours.

## 1. Quota check

```sh
aws service-quotas get-service-quota --region $REGION \
  --service-code ec2 --quota-code L-1216C47A \
  --query 'Quota.Value'   # Running On-Demand Standard instances (vCPUs)
```

You need at least 48 for the default fleet plus the small control-plane nodes, or at least
192 if you chose the metal parity shape. Request more before you continue if you are short.

## 2. Terraform, one apply

```sh
cd deploy/terraform/aws/quickstart
terraform init
terraform apply \
  -var region=$REGION \
  -var domain=$DOMAIN \
  -var admin_email=$ADMIN_EMAIL
```

If the domain is in Route 53, add `-var route53_zone_id=<hosted zone>` and Terraform creates
the certificate validation records; otherwise step 4 does it by hand.

This creates the VPC with an S3 gateway endpoint, the chunks bucket, the EKS cluster with the
self-managed KVM auto-scaling group, RDS plus a one-shot job that creates the two databases,
⚡ the master key as a real KMS key (so there is no key secret to populate on AWS), the secret
shells, every IRSA role, both namespaces, the AWS Load Balancer Controller, the External
Secrets relay, and the ACM certificate request. Expect about 20 minutes; the EKS control
plane is the slow part, and metal instances take longer still.

Cluster credentials:

```sh
aws eks update-kubeconfig --region $REGION \
  --name "$(terraform output -raw cluster_name)"
```

## 3. Populate the secret shells

⚡ Three shells plus the CA pair, and no master-key entry, because the master key is the KMS
key. **Do this before the Helm installs.** Until the shells have values, the relay leaves the
in-cluster Secrets unsynced and the pods crash-loop waiting for them.

```sh
# The machine bearer allow-list (one token to start).
aws secretsmanager put-secret-value --region $REGION \
  --secret-id engram/auth-tokens \
  --secret-string "$(openssl rand -hex 32)"

# The orchestrator's session-signing secret.
aws secretsmanager put-secret-value --region $REGION \
  --secret-id engram/better-auth-secret \
  --secret-string "$(openssl rand -base64 48)"

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

If you passed `route53_zone_id`, the validation records exist; wait until `terraform output`
or the ACM console shows **Issued**. Otherwise create the CNAMEs from:

```sh
terraform output acm_validation_records
```

Issuance follows within minutes of the records resolving.

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
with the matching `-var *_ksa` values.

Watch it come up:

```sh
kubectl get pods -n engrams
kubectl get pods -n engrams-hosts
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
cd deploy/terraform/aws/quickstart && terraform destroy -var ... # the same vars as apply
```

The bucket refuses to be destroyed unless it is empty; `aws s3 rm s3://<bucket> --recursive`
first.

## When something does not come up

| Symptom | Likely cause |
|---|---|
| KVM auto-scaling group stuck at 0 of 2 healthy | vCPU quota (step 1), or the chosen shape is not offered in a chosen availability zone; m8i and metal availability varies by zone. Check the group's activity history. |
| Pods crash-loop on missing Secrets | Step 3 was skipped. `kubectl get externalsecret -A` shows the sync state. |
| Host-agent crash-loops on the egress CA | The CA shells are empty, or the fleet's IRSA role cannot read them; it is scoped to exactly those two ARNs. |
| Ingress has no load balancer hostname | The AWS Load Balancer Controller is not healthy (`kubectl get pods -n kube-system`), or the ACM certificate is not Issued yet. |
| `no capacity`, sessions queued forever | Hosts never registered. Check the host-agent logs for the coordinator endpoint and the bearer token. |
