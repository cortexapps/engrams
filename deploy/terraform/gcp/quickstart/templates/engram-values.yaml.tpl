# TF-derived Helm values for the engram chart — rendered by the GCP
# quickstart (`terraform output -raw engram_values`). Layer it OVER
# your copy of values-gcp.yaml.example:
#
#   helm install engram deploy/helm/engram -n ${app_namespace} \
#     -f my-values.yaml -f engram.tfvalues.yaml
#
# Everything here has a single source of truth in Terraform; edit the
# static file, not this one.
blob:
  gcs:
    bucket: ${bucket}
secrets:
  gcpProjectId: ${project_id}
serviceAccount:
  create: true
  name: ${coordinator_ksa}
  annotations:
    iam.gke.io/gcp-service-account: ${coordinator_sa_email}
web:
  ingress:
    annotations:
      kubernetes.io/ingress.class: gce
      networking.gke.io/managed-certificates: ${managed_cert_name}
      kubernetes.io/ingress.global-static-ip-name: ${static_ip_name}
    hosts:
      - host: ${domain}
        paths:
          - path: /
            pathType: Prefix
orchestrator:
  publicUrl: https://${domain}
  auth:
    adminEmails:
      - ${admin_email}
