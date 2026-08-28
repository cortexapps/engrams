# External Secrets Operator (ESO) — the relay that extends the
# "secret material never enters tfstate" policy to the K8s side.
# ExternalSecrets declaratively sync Secret Manager values into K8s
# Secrets at RUNTIME, so git + tfstate hold only references.
#
# Promoted from the production deployment (ADR 0122). Why a relay at
# all: the host-agent pods are hostNetwork and BYPASS Workload
# Identity — they authenticate as the scope-limited node SA and 403
# on direct Secret Manager reads. ESO runs with proper WI and does
# the read; the charts consume plain K8s Secrets.
#
# NOTE the ordering dependency documented in docs/deploy-gcp.md:
# ExternalSecrets sync only after the shells are POPULATED. Helm
# releases installed before that will CrashLoop on missing Secrets
# until the first successful sync.

locals {
  eso_namespace = "external-secrets"
  eso_ksa_name  = "external-secrets"
}

resource "google_service_account" "eso" {
  account_id   = "${var.name_prefix}-eso"
  display_name = "Engram External Secrets reader (${var.name_prefix})"
}

resource "google_service_account_iam_member" "eso_wi" {
  service_account_id = google_service_account.eso.name
  role               = "roles/iam.workloadIdentityUser"
  member             = "serviceAccount:${var.project_id}.svc.id.goog[${local.eso_namespace}/${local.eso_ksa_name}]"
}

# Pinned chart; CRDs install through it so the ClusterSecretStore /
# ExternalSecret kinds exist before the manifests below apply.
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
  # Pin the controller KSA name so the WI member string above cannot
  # drift from it, and annotate it for GKE Workload Identity.
  set {
    name  = "serviceAccount.name"
    value = local.eso_ksa_name
  }
  set {
    name  = "serviceAccount.annotations.iam\\.gke\\.io/gcp-service-account"
    value = google_service_account.eso.email
  }

  depends_on = [module.gke_cluster]
}

# Cluster-scoped so any namespace's ExternalSecret resolves through it.
resource "kubectl_manifest" "gcp_secret_store" {
  yaml_body = <<-YAML
    apiVersion: external-secrets.io/v1
    kind: ClusterSecretStore
    metadata:
      name: gcp-secret-manager
    spec:
      provider:
        gcpsm:
          projectID: ${var.project_id}
          auth:
            workloadIdentity:
              clusterLocation: ${var.region}
              clusterName: ${module.gke_cluster.cluster_name}
              serviceAccountRef:
                name: ${local.eso_ksa_name}
                namespace: ${local.eso_namespace}
  YAML

  depends_on = [helm_release.external_secrets]
}

# ─── engram-coordinator-secrets ───────────────────────────────────
# What the coordinator Deployment reads: DATABASE_URL +
# ENGRAM_AUTH_TOKENS + ENGRAM_KEK_MASTER_KEY (the orchestrator also
# reads the KEK from it). Names match the chart defaults.
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
        name: gcp-secret-manager
        kind: ClusterSecretStore
      target:
        name: engram-coordinator-secrets
        creationPolicy: Owner
      data:
        - secretKey: DATABASE_URL
          remoteRef:
            key: ${module.cloudsql.database_url_secret_id}
        - secretKey: ENGRAM_AUTH_TOKENS
          remoteRef:
            key: ${module.secret_shells.secret_ids["auth-tokens"]}
        - secretKey: ENGRAM_KEK_MASTER_KEY
          remoteRef:
            key: ${module.secret_shells.secret_ids["kek-master"]}
  YAML

  depends_on = [kubectl_manifest.gcp_secret_store]
}

# ─── engram-orchestrator-secrets ──────────────────────────────────
# CONTROL_PLANE_BEARER is TEMPLATED: the coordinator's auth-tokens
# value is a comma-separated allow-list, and a comma list is not a
# valid bearer — the sprig pipeline emits the first element. This
# non-obvious wiring is exactly why the ExternalSecret (not a hand
# copy) owns the K8s Secret.
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
        name: gcp-secret-manager
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
            key: ${module.cloudsql.orchestrator_database_url_secret_id}
        - secretKey: betterAuthSecret
          remoteRef:
            key: ${module.secret_shells.secret_ids["better-auth-secret"]}
        - secretKey: authTokens
          remoteRef:
            key: ${module.secret_shells.secret_ids["auth-tokens"]}
  YAML

  depends_on = [kubectl_manifest.gcp_secret_store]
}

# ─── the egress CA, into the FLEET namespace only ─────────────────
# The host fleet is the only CA consumer (caSource=env on the
# host-agent). The private key stays out of the app namespace — the
# coordinator never touches the CA, and syncing the key there would
# hand it to anything that can read app-namespace Secrets. A
# DEDICATED CA secret (not folded into the coordinator one) keeps
# the same boundary inside the fleet namespace.
resource "kubectl_manifest" "host_egress_ca_external_secret" {
  yaml_body = <<-YAML
    apiVersion: external-secrets.io/v1
    kind: ExternalSecret
    metadata:
      name: engram-host-egress-ca
      namespace: ${kubernetes_namespace_v1.fleet.metadata[0].name}
    spec:
      refreshInterval: 1h
      secretStoreRef:
        name: gcp-secret-manager
        kind: ClusterSecretStore
      target:
        name: engram-host-egress-ca
        creationPolicy: Owner
      data:
        - secretKey: ENGRAM_EGRESS_CA_CERT_PEM
          remoteRef:
            key: ${module.secret_shells.secret_ids["egress-ca-cert"]}
        - secretKey: ENGRAM_EGRESS_CA_KEY_PEM
          remoteRef:
            key: ${module.secret_shells.secret_ids["egress-ca-key"]}
  YAML

  depends_on = [kubectl_manifest.gcp_secret_store]
}

# The host-agent + operator read the coordinator bearer from their own
# namespace too (the chart's coordinator.tokenSecretName).
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
        name: gcp-secret-manager
        kind: ClusterSecretStore
      target:
        name: engram-coordinator-secrets
        creationPolicy: Owner
      data:
        - secretKey: ENGRAM_AUTH_TOKENS
          remoteRef:
            key: ${module.secret_shells.secret_ids["auth-tokens"]}
  YAML

  depends_on = [kubectl_manifest.gcp_secret_store]
}
