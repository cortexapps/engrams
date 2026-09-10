# AWS quickstart (ADR 0122): zero → an engram-ready EKS cluster in
# one apply — the AWS twin of gcp/quickstart. Composes every module
# under ../modules plus the glue: IRSA roles, the KEK KMS key,
# namespaces (fleet PSA-privileged), the AWS Load Balancer
# Controller, the External Secrets relay, the ACM certificate, and a
# one-shot Job creating the two logical databases (the AWS provider
# cannot).
#
# What it deliberately does NOT do (see docs/deploy-aws.md):
# populate the secret shells, the final DNS CNAME to the ALB (only
# known after `helm install`), and the two `helm install` commands —
# the `engram_values` / `host_fleet_values` outputs render the
# TF-derived halves.
#
# The default KVM shape is m8i.8xlarge (32 vCPU, nested virt enabled
# at launch) × 2 — 64 on-demand vCPUs, which a fresh account's default
# quota may not cover. Operators who need one image bake serving BOTH
# a GCP C3 fleet and this one set kvm_instance_type = "m7i.metal-24xl"
# (CPUID parity; needs a metal quota ticket and costs ~3× more).
# See docs/deploy-aws.md before applying.

data "aws_caller_identity" "current" {}

locals {
  tags = merge(var.tags, { app = "engram" })
}

# ─── network + storage + cluster ──────────────────────────────────

module "network" {
  source = "../modules/network"

  name   = var.name_prefix
  region = var.region
  tags   = local.tags
}

resource "random_id" "bucket_suffix" {
  byte_length = 3
}

module "storage" {
  source = "../modules/storage"

  bucket_name = "${var.name_prefix}-durable-storage-${random_id.bucket_suffix.hex}"
  tags        = local.tags
}

module "eks_cluster" {
  source = "../modules/eks-cluster"

  name               = var.name_prefix
  vpc_id             = module.network.vpc_id
  private_subnet_ids = module.network.private_subnet_ids
  tags               = local.tags
}

module "kvm_nodegroup" {
  source = "../modules/kvm-nodegroup"

  name                 = "${var.name_prefix}-kvm"
  cluster_name         = module.eks_cluster.cluster_name
  cluster_version      = module.eks_cluster.cluster_version
  cluster_endpoint     = module.eks_cluster.cluster_endpoint
  cluster_ca_data      = module.eks_cluster.cluster_certificate_authority_data
  cluster_service_cidr = module.eks_cluster.cluster_service_cidr
  subnet_ids           = module.network.private_subnet_ids
  security_group_ids   = [module.eks_cluster.node_security_group_id]
  instance_type        = var.kvm_instance_type
  initial_node_count   = var.kvm_initial_node_count
  tags                 = local.tags
}

# ─── the KEK ──────────────────────────────────────────────────────
# On AWS the COORDINATOR's KEK is a real KMS key (kek.provider=
# aws-kms — it wraps/unwraps DEKs via kms:Encrypt/Decrypt), not an
# env-var secret. Deliberate per-cloud difference (ADR 0122 D6). The
# orchestrator has no KMS path and keeps a raw key: the `kek-master`
# shell, relayed into its own Secret (eso.tf).

resource "aws_kms_key" "kek" {
  description             = "engram KEK — DEK wrap/unwrap (ADR 0122)"
  deletion_window_in_days = 7
  enable_key_rotation     = true
  tags                    = local.tags
}

resource "aws_kms_alias" "kek" {
  name          = "alias/${var.name_prefix}-kek"
  target_key_id = aws_kms_key.kek.key_id
}

# ─── database + secret shells ─────────────────────────────────────

module "rds" {
  source = "../modules/rds"

  name_prefix                = var.name_prefix
  vpc_id                     = module.network.vpc_id
  subnet_ids                 = module.network.private_subnet_ids
  allowed_security_group_ids = [module.eks_cluster.node_security_group_id]
  tags                       = local.tags
}

module "secret_shells" {
  source = "../modules/secret-shells"

  name_prefix = var.name_prefix
  tags        = local.tags
}

# ─── IRSA roles ───────────────────────────────────────────────────

module "irsa_coordinator" {
  source = "../modules/irsa"

  role_name         = "${var.name_prefix}-coordinator"
  oidc_provider_arn = module.eks_cluster.oidc_provider_arn
  oidc_provider     = module.eks_cluster.oidc_provider
  namespace         = var.app_namespace
  service_account   = var.coordinator_ksa
  tags              = local.tags

  policies = {
    engram = jsonencode({
      Version = "2012-10-17"
      Statement = [
        {
          Sid      = "ChunksBucket"
          Effect   = "Allow"
          Action   = ["s3:GetObject", "s3:PutObject", "s3:DeleteObject", "s3:ListBucket", "s3:AbortMultipartUpload"]
          Resource = [module.storage.bucket_arn, "${module.storage.bucket_arn}/*"]
        },
        {
          Sid      = "Kek"
          Effect   = "Allow"
          Action   = ["kms:Encrypt", "kms:Decrypt"]
          Resource = [aws_kms_key.kek.arn]
        },
        {
          # The coordinator resolves image-manifest secret refs
          # (secrets.backend=aws) under the slash namespace.
          Sid      = "SecretsPrefix"
          Effect   = "Allow"
          Action   = ["secretsmanager:GetSecretValue"]
          Resource = ["arn:aws:secretsmanager:${var.region}:${data.aws_caller_identity.current.account_id}:secret:${var.name_prefix}/*"]
        },
        {
          # ECR registry auth (aws_ecr rows) is resolved on the
          # COORDINATOR, not the host-agent — the host POSTs
          # /auth/resolve-registry and gets back a short-lived token.
          # GetAuthorizationToken does not accept resource scoping;
          # "*" is the narrowest possible grant.
          Sid      = "EcrToken"
          Effect   = "Allow"
          Action   = ["ecr:GetAuthorizationToken"]
          Resource = "*"
        },
      ]
    })
  }
}

module "irsa_host_fleet" {
  source = "../modules/irsa"

  role_name         = "${var.name_prefix}-fc-host"
  oidc_provider_arn = module.eks_cluster.oidc_provider_arn
  oidc_provider     = module.eks_cluster.oidc_provider
  namespace         = var.fleet_namespace
  service_account   = var.host_fleet_ksa
  tags              = local.tags

  policies = {
    engram = jsonencode({
      Version = "2012-10-17"
      Statement = [
        {
          Sid      = "ChunksBucket"
          Effect   = "Allow"
          Action   = ["s3:GetObject", "s3:PutObject", "s3:DeleteObject", "s3:ListBucket", "s3:AbortMultipartUpload"]
          Resource = [module.storage.bucket_arn, "${module.storage.bucket_arn}/*"]
        },
        {
          # caSource=aws-secrets-manager: the host-agent reads the CA
          # pair directly — IRSA works in its hostNetwork pod.
          Sid    = "EgressCa"
          Effect = "Allow"
          Action = ["secretsmanager:GetSecretValue"]
          Resource = [
            module.secret_shells.secret_arns["egress-ca-cert"],
            module.secret_shells.secret_arns["egress-ca-key"],
          ]
        },
      ]
    })
  }
}

module "irsa_host_operator" {
  source = "../modules/host-operator-iam"

  name_prefix       = var.name_prefix
  oidc_provider_arn = module.eks_cluster.oidc_provider_arn
  oidc_provider     = module.eks_cluster.oidc_provider
  namespace         = var.fleet_namespace
  ksa_name          = var.operator_ksa
  asg_arn           = module.kvm_nodegroup.asg_arn
  tags              = local.tags
}

# ─── namespaces ───────────────────────────────────────────────────

resource "kubernetes_namespace_v1" "app" {
  metadata {
    name   = var.app_namespace
    labels = local.tags
  }

  depends_on = [module.eks_cluster]
}

# ADR 0044 hard constraint: the host-agent DaemonSet is privileged +
# hostPID + hostNetwork; the namespace must enforce the `privileged`
# Pod Security level.
resource "kubernetes_namespace_v1" "fleet" {
  metadata {
    name = var.fleet_namespace
    labels = merge(local.tags, {
      "pod-security.kubernetes.io/enforce" = "privileged"
    })
  }

  depends_on = [module.eks_cluster]
}

# ─── AWS Load Balancer Controller ─────────────────────────────────
# Required for the chart's `className: alb` Ingress to do anything.
# The IAM policy is the controller project's published document,
# vendored beside this file.

module "irsa_alb_controller" {
  source = "../modules/irsa"

  role_name         = "${var.name_prefix}-alb-controller"
  oidc_provider_arn = module.eks_cluster.oidc_provider_arn
  oidc_provider     = module.eks_cluster.oidc_provider
  namespace         = "kube-system"
  service_account   = "aws-load-balancer-controller"
  tags              = local.tags

  policies = {
    alb-controller = file("${path.module}/alb-controller-iam-policy.json")
  }
}

resource "helm_release" "alb_controller" {
  name       = "aws-load-balancer-controller"
  namespace  = "kube-system"
  repository = "https://aws.github.io/eks-charts"
  chart      = "aws-load-balancer-controller"
  version    = "1.8.2"

  set {
    name  = "clusterName"
    value = module.eks_cluster.cluster_name
  }
  set {
    name  = "serviceAccount.create"
    value = "true"
  }
  set {
    name  = "serviceAccount.name"
    value = "aws-load-balancer-controller"
  }
  set {
    name  = "serviceAccount.annotations.eks\\.amazonaws\\.com/role-arn"
    value = module.irsa_alb_controller.role_arn
  }

  depends_on = [module.eks_cluster]
}

# ─── ACM certificate ──────────────────────────────────────────────

resource "aws_acm_certificate" "web" {
  domain_name       = var.domain
  validation_method = "DNS"
  tags              = local.tags

  lifecycle {
    create_before_destroy = true
  }
}

resource "aws_route53_record" "acm_validation" {
  for_each = var.route53_zone_id == "" ? {} : {
    for dvo in aws_acm_certificate.web.domain_validation_options : dvo.domain_name => {
      name   = dvo.resource_record_name
      type   = dvo.resource_record_type
      record = dvo.resource_record_value
    }
  }

  zone_id = var.route53_zone_id
  name    = each.value.name
  type    = each.value.type
  ttl     = 300
  records = [each.value.record]
}

resource "aws_acm_certificate_validation" "web" {
  count = var.route53_zone_id == "" ? 0 : 1

  certificate_arn         = aws_acm_certificate.web.arn
  validation_record_fqdns = [for r in aws_route53_record.acm_validation : r.fqdn]
}

# ─── logical databases ────────────────────────────────────────────
# The AWS provider can't create databases inside an RDS instance;
# this one-shot Job does, idempotently (the RDS-module twin of
# Cloud SQL's google_sql_database resources). Credentials come from
# a TF-created Secret — the password is TF-owned state either way
# (the documented rds-module exception).

resource "kubernetes_secret_v1" "db_init" {
  metadata {
    name      = "${var.name_prefix}-db-init"
    namespace = kubernetes_namespace_v1.app.metadata[0].name
  }

  data = {
    PGHOST     = module.rds.address
    PGUSER     = module.rds.master_user
    PGPASSWORD = module.rds.master_password
  }
}

resource "kubernetes_job_v1" "db_init" {
  metadata {
    name      = "${var.name_prefix}-db-init"
    namespace = kubernetes_namespace_v1.app.metadata[0].name
  }

  spec {
    backoff_limit = 6

    template {
      metadata {
        labels = { job = "${var.name_prefix}-db-init" }
      }
      spec {
        restart_policy = "Never"

        container {
          name  = "psql"
          image = "postgres:16-alpine"
          command = [
            "sh", "-ec",
            <<-EOT
              export PGDATABASE=postgres PGSSLMODE=require
              for db in controlplane orchestrator; do
                if psql -tAc "SELECT 1 FROM pg_database WHERE datname = '$db'" | grep -q 1; then
                  echo "database $db exists"
                else
                  psql -c "CREATE DATABASE $db"
                  echo "database $db created"
                fi
              done
            EOT
          ]

          env_from {
            secret_ref {
              name = kubernetes_secret_v1.db_init.metadata[0].name
            }
          }
        }
      }
    }
  }

  wait_for_completion = true

  timeouts {
    create = "10m"
    update = "10m"
  }

  depends_on = [module.rds]
}
