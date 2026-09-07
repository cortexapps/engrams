# engrams on AWS — Terraform

The AWS (EKS) deployment surface (ADR 0122), mirroring
[`../gcp/`](../gcp/README.md): **composable modules + a quickstart
root**. The step-by-step bring-up — including the manual steps and
the cost/quota warnings — is
[`docs/deploy-aws.md`](../../../docs/deploy-aws.md). The
cloud-agnostic contract is [`../README.md`](../README.md).

## Layout

```
modules/
  network/            VPC, ALB-tagged subnets, one NAT, an S3
                      gateway endpoint (chunk traffic skips the NAT)
  storage/            S3 chunks bucket + the abort-incomplete-
                      multipart rule the streaming backend relies on
  eks-cluster/        Thin wrapper over pinned terraform-aws-modules/
                      eks: IRSA + the small control-plane node group
  kvm-nodegroup/      THE KVM fleet: a SELF-MANAGED launch template +
                      ASG (Intel metal, operator-owned size,
                      AZRebalance suspended — read its header)
  rds/                Postgres 16 + the two DSN secrets (with the
                      load-bearing sslmode split)
  secret-shells/      Empty Secrets Manager shells (slash-namespaced;
                      the kek-master shell is the ORCHESTRATOR's raw
                      key — the coordinator's KEK is a KMS key on AWS)
  irsa/               IRSA role factory (OIDC trust + caller policy)
  host-operator-iam/  The asg scaler's least-privilege role, scoped
                      to the fleet ASG
quickstart/           ONE apply: all of the above + the KEK KMS key,
                      namespaces (fleet PSA-privileged), the AWS Load
                      Balancer Controller, the External Secrets
                      relay, the ACM certificate, a one-shot
                      DB-create Job — and rendered Helm values as
                      outputs
```

## Quickstart

> **Cost + quota:** the default KVM shape is `m8i.8xlarge` (32 vCPU,
> nested virtualization enabled at launch) × 2 — 64 on-demand vCPUs,
> which a fresh account's default quota may not cover. The metal
> alternative (`m7i.metal-24xl`, for CPUID parity with a GCP C3
> fleet) needs a metal quota ticket and costs ~3× more. Read
> `docs/deploy-aws.md` first.

```sh
cd deploy/terraform/aws/quickstart
terraform init
cat > terraform.tfvars <<EOF   # gitignored; every later command reads it
region      = "us-west-2"
domain      = "engrams.example.com"
admin_email = "you@example.com"
EOF
terraform apply
```

Then follow `docs/deploy-aws.md`: populate the secret shells
(`secret_shell_names` output; the shells module header carries the
exact `aws secretsmanager put-secret-value` commands), validate the
ACM cert (automatic with `route53_zone_id`, else the
`acm_validation_records` output), render the values overlays —

```sh
terraform output -raw engram_values      > engram.tfvalues.yaml
terraform output -raw host_fleet_values  > host-fleet.tfvalues.yaml
```

— `helm install` the two charts with your copied
`values-aws.yaml.example` files plus these overlays, and finally
CNAME the domain to the ALB hostname (`kubectl get ingress` — the
one output Terraform cannot know).

Mind the ordering: ExternalSecrets sync only after the shells are
populated; Helm releases installed before that CrashLoop on missing
Secrets until the first successful sync.

## BYO cluster / database

Compose the modules directly — each stands alone. The KVM nodegroup
and the operator IAM carry the engram-specific invariants; RDS can be
any Postgres ≥ 14 you already run (mind the sslmode split documented
in the rds module).
