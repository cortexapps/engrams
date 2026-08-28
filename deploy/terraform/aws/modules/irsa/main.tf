# IRSA role factory (ADR 0122): one IAM role a K8s ServiceAccount can
# assume via the cluster's OIDC provider, with caller-supplied policy
# JSON. Used for the coordinator (S3 + Secrets Manager + KMS), the
# host-fleet SA (S3 + the CA secrets), ESO, and the AWS Load Balancer
# Controller.
#
# Identity note (why IRSA everywhere, including the hostNetwork host
# pods): IRSA is env/file-based — the pod-identity webhook injects
# AWS_WEB_IDENTITY_TOKEN_FILE + a projected token — so it works where
# GKE Workload Identity's metadata intercept does not.

data "aws_iam_policy_document" "assume" {
  statement {
    effect  = "Allow"
    actions = ["sts:AssumeRoleWithWebIdentity"]

    principals {
      type        = "Federated"
      identifiers = [var.oidc_provider_arn]
    }

    condition {
      test     = "StringEquals"
      variable = "${var.oidc_provider}:sub"
      values   = ["system:serviceaccount:${var.namespace}:${var.service_account}"]
    }

    condition {
      test     = "StringEquals"
      variable = "${var.oidc_provider}:aud"
      values   = ["sts.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "this" {
  name               = var.role_name
  assume_role_policy = data.aws_iam_policy_document.assume.json
  tags               = var.tags
}

resource "aws_iam_role_policy" "inline" {
  for_each = var.policies

  name   = each.key
  role   = aws_iam_role.this.id
  policy = each.value
}

resource "aws_iam_role_policy_attachment" "managed" {
  for_each = toset(var.managed_policy_arns)

  role       = aws_iam_role.this.name
  policy_arn = each.value
}
