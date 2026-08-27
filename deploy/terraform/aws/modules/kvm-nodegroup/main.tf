# ADR 0044 / ADR 0122: THE KVM node group for the Firecracker host
# fleet on EKS — the AWS twin of gcp/modules/gke-kvm-pool.
#
# A SELF-MANAGED node group (launch template + Auto Scaling group),
# NOT an EKS managed node group: the ADR 0048 autoscaling operator
# actuates the ASG directly (SetDesiredCapacity +
# TerminateInstanceInAutoScalingGroup), and managed node groups
# reconcile their scaling config against external actuation.
#
# Invariants (each is load-bearing — see the GCP twin for the war
# stories):
#
# - **KVM needs Intel hardware.** On EC2 that means bare metal
#   (`*.metal`) or the Xeon-6 C8i/M8i/R8i shapes. No AMD, no
#   Graviton. The default `m7i.metal-24xl` is Sapphire Rapids —
#   deliberate CPUID parity with the GCP quickstart's C3 pool, so
#   images bake ONCE and snapshots restore on either fleet (CPUID is
#   a one-way door: newer-platform snapshots never restore on older
#   hardware).
#
# - **The operator owns the size.** `ignore_changes` on
#   desired_capacity; `max_size` sits above the operator's ceiling.
#   Never attach target-tracking or the cluster autoscaler.
#
# - **AZRebalance is suspended** — it terminates instances of its own
#   choosing to even out zones, which is exactly the "cloud picks an
#   arbitrary, possibly loaded victim" failure the NodePoolScaler
#   contract exists to prevent. The operator's remove_node names its
#   victim.
#
# - **Scale-in protection stays OFF.** TerminateInstanceInAutoScalingGroup
#   (the operator's shrink verb) is the sanctioned terminator.
#
# - The **label/taint pair** matches the host-fleet chart:
#   `engram.io/kvm=true` label + `engram.io/kvm=true:NoSchedule`
#   taint, set via nodeadm in user data.

data "aws_ssm_parameter" "eks_ami" {
  # EKS-optimized AL2023 AMI for the cluster's K8s version.
  name = "/aws/service/eks/optimized-ami/${var.cluster_version}/amazon-linux-2023/x86_64/standard/recommended/image_id"
}

locals {
  # nodeadm config: join the cluster + carry the fleet label/taint.
  user_data = base64encode(<<-EOT
    MIME-Version: 1.0
    Content-Type: multipart/mixed; boundary="BOUNDARY"

    --BOUNDARY
    Content-Type: application/node.eks.aws

    apiVersion: node.eks.aws/v1alpha1
    kind: NodeConfig
    spec:
      cluster:
        name: ${var.cluster_name}
        apiServerEndpoint: ${var.cluster_endpoint}
        certificateAuthority: ${var.cluster_ca_data}
        cidr: ${var.cluster_service_cidr}
      kubelet:
        flags:
          - --node-labels=engram.io/kvm=true${var.extra_node_labels == "" ? "" : ",${var.extra_node_labels}"}
          - --register-with-taints=engram.io/kvm=true:NoSchedule

    --BOUNDARY--
  EOT
  )
}

resource "aws_launch_template" "kvm" {
  name_prefix   = "${var.name}-"
  image_id      = data.aws_ssm_parameter.eks_ami.value
  instance_type = var.instance_type

  vpc_security_group_ids = var.security_group_ids
  user_data              = local.user_data

  iam_instance_profile {
    arn = aws_iam_instance_profile.node.arn
  }

  # Work dir + chunk cache + snapshots ride the root volume unless
  # the shape carries local NVMe (metal shapes do — the chart's
  # storage.dedicatedDevices stripes them instead; this stays the
  # durable fallback).
  block_device_mappings {
    device_name = "/dev/xvda"

    ebs {
      volume_type           = "gp3"
      volume_size           = var.root_volume_size_gb
      iops                  = var.root_volume_iops
      throughput            = var.root_volume_throughput
      delete_on_termination = true
      encrypted             = true
    }
  }

  metadata_options {
    http_endpoint = "enabled"
    http_tokens   = "required" # IMDSv2
    # hostNetwork host-agent pods share the node netns, so hop limit 1
    # suffices (no bridge hop) — and IRSA is the primary identity
    # anyway. Keeping it at 1 also stops bridge-networked pods (and
    # any guest traffic that escapes the egress policy) one hop short
    # of the node credentials.
    http_put_response_hop_limit = 1
  }

  tag_specifications {
    resource_type = "instance"
    tags = merge(var.tags, {
      Name                                        = var.name
      "kubernetes.io/cluster/${var.cluster_name}" = "owned"
    })
  }

  tags = var.tags
}

resource "aws_autoscaling_group" "kvm" {
  name = var.name

  # Seed values only — the ADR 0048 operator owns desired_capacity.
  desired_capacity = var.initial_node_count
  min_size         = var.min_size
  max_size         = var.max_size

  vpc_zone_identifier = var.subnet_ids

  launch_template {
    id      = aws_launch_template.kvm.id
    version = "$Latest"
  }

  # The operator names its victims (ADR 0048); AZRebalance would pick
  # its own.
  suspended_processes = ["AZRebalance"]

  # Instances must be replaced through the operator's roll, never by
  # an ASG refresh — mirror of the GKE pool's auto_upgrade=false.
  tag {
    key                 = "kubernetes.io/cluster/${var.cluster_name}"
    value               = "owned"
    propagate_at_launch = true
  }

  dynamic "tag" {
    for_each = var.tags
    content {
      key                 = tag.key
      value               = tag.value
      propagate_at_launch = true
    }
  }

  lifecycle {
    ignore_changes = [desired_capacity] # the ADR 0048 operator owns the size
  }
}

# ── node IAM ──────────────────────────────────────────────────────
# The standard EKS worker roles. S3/Secrets access rides IRSA on the
# host-fleet KSA (works in hostNetwork pods), NOT the node role — the
# node role stays minimal.

resource "aws_iam_role" "node" {
  name = "${var.name}-node"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Action    = "sts:AssumeRole"
      Principal = { Service = "ec2.amazonaws.com" }
    }]
  })

  tags = var.tags
}

resource "aws_iam_role_policy_attachment" "node_worker" {
  role       = aws_iam_role.node.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonEKSWorkerNodePolicy"
}

resource "aws_iam_role_policy_attachment" "node_cni" {
  role       = aws_iam_role.node.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonEKS_CNI_Policy"
}

resource "aws_iam_role_policy_attachment" "node_ecr" {
  role       = aws_iam_role.node.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonEC2ContainerRegistryReadOnly"
}

resource "aws_iam_instance_profile" "node" {
  name = "${var.name}-node"
  role = aws_iam_role.node.name
}
