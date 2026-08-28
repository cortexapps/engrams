# TF-derived Helm values for the engram-host-fleet chart — rendered
# by the AWS quickstart (`terraform output -raw host_fleet_values`).
# Layer it OVER your copy of the chart's values-aws.yaml.example:
#
#   helm install hf deploy/helm/engram-host-fleet -n ${fleet_namespace} \
#     -f my-fleet-values.yaml -f host-fleet.tfvalues.yaml
#
# Everything here has a single source of truth in Terraform; edit the
# static file, not this one.
serviceAccount:
  annotations:
    eks.amazonaws.com/role-arn: ${host_fleet_role_arn}
blob:
  s3Bucket: ${bucket}
  s3Region: ${region}
egress:
  caAwsCertSecret: ${ca_cert_secret}
  caAwsKeySecret: ${ca_key_secret}
operator:
  serviceAccount:
    annotations:
      eks.amazonaws.com/role-arn: ${operator_role_arn}
  autoscaling:
    nodePool: ${asg_name}
