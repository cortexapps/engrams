# TF-derived Helm values for the engram-host-fleet chart — rendered
# by the GCP quickstart (`terraform output -raw host_fleet_values`).
# Layer it OVER your copy of the chart's values-gcp.yaml.example:
#
#   helm install hf deploy/helm/engram-host-fleet -n ${fleet_namespace} \
#     -f my-fleet-values.yaml -f host-fleet.tfvalues.yaml
#
# Everything here has a single source of truth in Terraform; edit the
# static file, not this one.
serviceAccount:
  annotations:
    iam.gke.io/gcp-service-account: ${host_sa_email}
blob:
  gcsBucket: ${bucket}
operator:
  serviceAccount:
    annotations:
      iam.gke.io/gcp-service-account: ${operator_sa_email}
  autoscaling:
    nodePool: ${kvm_pool_name}
