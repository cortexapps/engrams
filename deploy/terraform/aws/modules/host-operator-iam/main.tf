# ADR 0048 / ADR 0122: the host-fleet OPERATOR's cloud identity on
# AWS — the twin of gcp/modules/host-operator-iam. An IRSA role with
# a least-privilege policy for ASG autoscaling.
#
# Deliberately separate from BOTH the coordinator role (ZERO cloud
# control-plane permissions — only the operator ever touches the
# scaling APIs) and the host-fleet role (S3 + secrets — a different
# blast radius).
#
# The `asg` scaler's exact calls (crates/engram-cloud-aws/src/asg.rs):
#   - set_size    → autoscaling:SetDesiredCapacity
#   - remove_node → ec2:DescribeInstances (name → instance id)
#                 → autoscaling:DescribeAutoScalingInstances
#                   (membership check — never kill a foreign instance)
#                 → autoscaling:TerminateInstanceInAutoScalingGroup
#                   (named victim + decrement, so the group doesn't
#                   replace it)
#
# The mutating actions are scoped to the fleet ASG by ARN. The
# Describe* actions don't support resource scoping (AWS evaluates
# them against *), which is why they sit in their own statement.

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
      values   = ["system:serviceaccount:${var.namespace}:${var.ksa_name}"]
    }

    condition {
      test     = "StringEquals"
      variable = "${var.oidc_provider}:aud"
      values   = ["sts.amazonaws.com"]
    }
  }
}

data "aws_iam_policy_document" "scaler" {
  statement {
    sid    = "DescribeUnscopable"
    effect = "Allow"
    actions = [
      "autoscaling:DescribeAutoScalingGroups",
      "autoscaling:DescribeAutoScalingInstances",
      "ec2:DescribeInstances",
    ]
    resources = ["*"]
  }

  statement {
    sid    = "ActuateFleetAsg"
    effect = "Allow"
    actions = [
      "autoscaling:SetDesiredCapacity",
      "autoscaling:TerminateInstanceInAutoScalingGroup",
    ]
    resources = [var.asg_arn]
  }
}

resource "aws_iam_role" "host_operator" {
  name               = "${var.name_prefix}-host-operator"
  assume_role_policy = data.aws_iam_policy_document.assume.json
  tags               = var.tags
}

resource "aws_iam_role_policy" "scaler" {
  name   = "asg-scaler"
  role   = aws_iam_role.host_operator.id
  policy = data.aws_iam_policy_document.scaler.json
}
