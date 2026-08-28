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

  depends_on = [module.eks_cluster]
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
# DATABASE_URL + ENGRAM_AUTH_TOKENS. No KEK entry — on AWS the KEK is
# the KMS key (kek.provider=aws-kms), not an env var.
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
      data:
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
