---
title: "Terraform: AWS"
description: The composable modules and the one-apply quickstart for EKS.
sidebar:
  order: 8
---

`deploy/terraform/aws` is the AWS deployment surface, mirroring the [GCP one](../terraform-gcp/):
composable modules plus a quickstart root. The step-by-step bring-up, including the cost and
quota warnings, is [Deploy on AWS](../../guides/deploy-aws/). The cloud contract both
implement is on the GCP page.

## Layout

```
modules/
  network/            VPC, load-balancer-tagged subnets, one NAT, an S3 gateway
                      endpoint so chunk traffic skips the NAT
  storage/            the S3 chunks bucket, with the abort-incomplete-multipart
                      rule the streaming backend relies on
  eks-cluster/        a thin wrapper over the pinned terraform-aws-modules/eks:
                      IRSA plus the small control-plane node group
  kvm-nodegroup/      THE KVM fleet: a self-managed launch template and
                      auto-scaling group, Intel only, operator-owned size,
                      zone rebalancing suspended. Read its header.
  rds/                Postgres 16 plus the two connection-string secrets
  secret-shells/      empty Secrets Manager shells, slash-namespaced. No
                      master-key shell: the master key is a KMS key on AWS.
  irsa/               an IRSA role factory: OIDC trust plus the caller's policy
  host-operator-iam/  the auto-scaling-group scaler's least-privilege role,
                      scoped to the fleet's group
quickstart/           ONE apply: all of the above plus the master-key KMS key,
                      namespaces, the AWS Load Balancer Controller, the External
                      Secrets relay, the ACM certificate, a one-shot
                      database-create Job, and rendered Helm values as outputs
```

## Quickstart

The default KVM shape is two `m8i.8xlarge` instances, 64 on-demand vCPUs, which a fresh
account's quota may not cover. The metal alternative, `m7i.metal-24xl`, exists for CPU parity
with a GCP C3 fleet and needs a metal quota ticket. Read the bring-up guide first.

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

Set `route53_zone_id` in `terraform.tfvars` to have the certificate validation records
created. The outputs you use next are `secret_shell_names`, `acm_validation_records` when you
validate by hand, `cluster_name`, and the two rendered values overlays:

```sh
terraform output -raw engram_values      > engram.tfvalues.yaml
terraform output -raw host_fleet_values  > host-fleet.tfvalues.yaml
```

The one thing Terraform cannot print up front is the load balancer's hostname, which exists
only after the web Ingress reconciles; the final CNAME comes from `kubectl get ingress`.

Mind the order: the External Secrets relay syncs only after the shells are populated, and a
Helm release installed before that crash-loops on missing Secrets until the first sync.

## Bring your own cluster or database

Compose the modules directly; each stands alone. The KVM node group and the operator IAM
carry the engrams-specific invariants. RDS can be any Postgres 14 or later you already run;
the rds module documents the TLS mode split between the two databases that the connection
strings depend on.
