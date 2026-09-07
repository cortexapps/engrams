# External Secrets Operator — the AWS twin of gcp/quickstart/eso.tf
# (ADR 0122). Relays the DSNs + bearer/better-auth material into K8s
# Secrets.
#
# Narrower than the GCP relay on purpose: the host-agent reads the
# egress CA DIRECTLY from Secrets Manager (caSource=
# aws-secrets-manager — IRSA works in its hostNetwork pod, unlike
# GKE WI), so there is no CA ExternalSecret here. What still relays
# is the BOOTSTRAP env the pods need before they can talk to
# anything: DSNs, the bearer allow-list, the better-auth secret.

locals {
  eso_namespace = "external-secrets"
  eso_ksa_name  = "external-secrets"
}

module "irsa_eso" {
  source = "../modules/irsa"

  role_name         = "${var.name_prefix}-eso"
  oidc_provider_arn = module.eks_cluster.oidc_provider_arn
  oidc_provider     = module.eks_cluster.oidc_provider
  namespace         = local.eso_namespace
  service_account   = local.eso_ksa_name
  tags              = local.tags

  policies = {
    reader = jsonencode({
      Version = "2012-10-17"
      Statement = [{
        Sid    = "ReadRelayedSecrets"
        Effect = "Allow"
        Action = ["secretsmanager:GetSecretValue"]
        Resource = [
          module.rds.database_url_secret_arn,
          module.rds.orchestrator_database_url_secret_arn,
          module.secret_shells.secret_arns["auth-tokens"],
          module.secret_shells.secret_arns["better-auth-secret"],
          module.secret_shells.secret_arns["kek-master"],
        ]
      }]
    })
  }
}

resource "helm_release" "external_secrets" {
  name             = "external-secrets"
  namespace        = local.eso_namespace
  create_namespace = true

  repository = "https://charts.external-secrets.io"
  chart      = "external-secrets"
  version    = "2.6.0"

  set {
    name  = "installCRDs"
    value = "true"
  }
  set {
    name  = "serviceAccount.name"
    value = local.eso_ksa_name
  }
  set {
    name  = "serviceAccount.annotations.eks\\.amazonaws\\.com/role-arn"
    value = module.irsa_eso.role_arn
  }

  # The AWS Load Balancer Controller chart registers a fail-closed
  # MutatingWebhookConfiguration on every Service
  # (mservice.elbv2.k8s.aws) the moment it installs, before its pods
  # are ready. ESO's own webhook Service hits that webhook, so an
  # install racing the controller fails with "no endpoints available
  # for service aws-load-balancer-webhook-service". Serialize behind
  # the controller release (helm waits for its Deployment).
  depends_on = [module.eks_cluster, helm_release.alb_controller]
}

resource "kubectl_manifest" "aws_secret_store" {
  yaml_body = <<-YAML
    apiVersion: external-secrets.io/v1
    kind: ClusterSecretStore
    metadata:
      name: aws-secrets-manager
    spec:
      provider:
        aws:
          service: SecretsManager
          region: ${var.region}
          auth:
            jwt:
              serviceAccountRef:
                name: ${local.eso_ksa_name}
                namespace: ${local.eso_namespace}
  YAML

  depends_on = [helm_release.external_secrets]
}

# ─── engram-coordinator-secrets ───────────────────────────────────
# DATABASE_URL + ENGRAM_AUTH_TOKENS. No KEK entry — on AWS the
# coordinator's KEK is the KMS key (kek.provider=aws-kms), not an env
# var. The orchestrator's raw KEK rides ITS secret below.
resource "kubectl_manifest" "coordinator_external_secret" {
  yaml_body = <<-YAML
    apiVersion: external-secrets.io/v1
    kind: ExternalSecret
    metadata:
      name: engram-coordinator-secrets
      namespace: ${kubernetes_namespace_v1.app.metadata[0].name}
    spec:
      refreshInterval: 1h
      secretStoreRef:
        name: aws-secrets-manager
        kind: ClusterSecretStore
      target:
        name: engram-coordinator-secrets
        creationPolicy: Owner
      data:
        - secretKey: DATABASE_URL
          remoteRef:
            key: ${module.rds.database_url_secret_name}
        - secretKey: ENGRAM_AUTH_TOKENS
          remoteRef:
            key: ${module.secret_shells.secret_names["auth-tokens"]}
  YAML

  depends_on = [kubectl_manifest.aws_secret_store]
}

# ─── engram-orchestrator-secrets ──────────────────────────────────
# CONTROL_PLANE_BEARER is TEMPLATED: auth-tokens is a comma-separated
# allow-list and a comma list is not a valid bearer — the sprig
# pipeline emits the first element (the same non-obvious wiring as
# the GCP relay).
#
# ENGRAM_KEK_MASTER_KEY is here, not in the coordinator's secret: the
# orchestrator seals its own tables (user secrets, OIDC keys) IN
# PROCESS with a raw 32-byte key and has no KMS path (config.ts makes
# the var required). Neither tier opens the other's sealed rows, so
# the coordinator on KMS + the orchestrator on this key is sound. The
# chart's `orchestrator.kekSecret.existingSecret` (values-aws) points
# at this Secret.
resource "kubectl_manifest" "orchestrator_external_secret" {
  yaml_body = <<-YAML
    apiVersion: external-secrets.io/v1
    kind: ExternalSecret
    metadata:
      name: engram-orchestrator-secrets
      namespace: ${kubernetes_namespace_v1.app.metadata[0].name}
    spec:
      refreshInterval: 1h
      secretStoreRef:
        name: aws-secrets-manager
        kind: ClusterSecretStore
      target:
        name: engram-orchestrator-secrets
        creationPolicy: Owner
        template:
          engineVersion: v2
          type: Opaque
          data:
            ORCHESTRATOR_DATABASE_URL: "{{ .databaseUrl }}"
            BETTER_AUTH_SECRET: "{{ .betterAuthSecret }}"
            CONTROL_PLANE_BEARER: '{{ .authTokens | splitList "," | first }}'
            ENGRAM_KEK_MASTER_KEY: "{{ .kekMaster }}"
      data:
        - secretKey: kekMaster
          remoteRef:
            key: ${module.secret_shells.secret_names["kek-master"]}
        - secretKey: databaseUrl
          remoteRef:
            key: ${module.rds.orchestrator_database_url_secret_name}
        - secretKey: betterAuthSecret
          remoteRef:
            key: ${module.secret_shells.secret_names["better-auth-secret"]}
        - secretKey: authTokens
          remoteRef:
            key: ${module.secret_shells.secret_names["auth-tokens"]}
  YAML

  depends_on = [kubectl_manifest.aws_secret_store]
}

# The host-agent + operator read the coordinator bearer from their
# own namespace (the chart's coordinator.tokenSecretName).
resource "kubectl_manifest" "fleet_coordinator_secret" {
  yaml_body = <<-YAML
    apiVersion: external-secrets.io/v1
    kind: ExternalSecret
    metadata:
      name: engram-coordinator-secrets
      namespace: ${kubernetes_namespace_v1.fleet.metadata[0].name}
    spec:
      refreshInterval: 1h
      secretStoreRef:
        name: aws-secrets-manager
        kind: ClusterSecretStore
      target:
        name: engram-coordinator-secrets
        creationPolicy: Owner
      data:
        - secretKey: ENGRAM_AUTH_TOKENS
          remoteRef:
            key: ${module.secret_shells.secret_names["auth-tokens"]}
  YAML

  depends_on = [kubectl_manifest.aws_secret_store]
}
