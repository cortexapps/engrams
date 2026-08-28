# TF-derived Helm values for the engram chart — rendered by the AWS
# quickstart (`terraform output -raw engram_values`). Layer it OVER
# your copy of values-aws.yaml.example:
#
#   helm install engram deploy/helm/engram -n ${app_namespace} \
#     -f my-values.yaml -f engram.tfvalues.yaml
#
# Everything here has a single source of truth in Terraform; edit the
# static file, not this one.
blob:
  # The selector rides with the config it selects — without it the
  # chart default (gcs) wins and the coordinator boots against a
  # bucket that does not exist.
  backend: s3
  s3:
    bucket: ${bucket}
    region: ${region}
kek:
  provider: aws-kms
  awsKeyId: ${kek_key_arn}
serviceAccount:
  create: true
  name: ${coordinator_ksa}
  annotations:
    eks.amazonaws.com/role-arn: ${coordinator_role_arn}
web:
  ingress:
    annotations:
      alb.ingress.kubernetes.io/certificate-arn: ${cert_arn}
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
