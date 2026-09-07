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
# - **KVM needs Intel hardware.** On EC2 that means the 8th-gen
#   Xeon-6 virtual shapes — C8i/M8i/R8i and their -flex variants, the
#   only families RunInstances accepts `NestedVirtualization=enabled`
#   for (EC2 API reference, CpuOptionsRequest) — or bare metal
#   (`*.metal`). No AMD, no Graviton, and NOT the 7th-gen virtual
#   shapes: `describe-instance-types` lists `nested-virtualization`
#   under m7i/c7i/r7i too, but the launch flag is 8th-gen only. The
#   default `m8i.8xlarge` (32 vCPU / 128 GiB) is the nearest shape
#   above the GCP quickstart's `c3-standard-22` (22 vCPU / 88 GiB) —
#   sizes jump from 4xlarge (16 vCPU) to 8xlarge (32 vCPU); there is
#   no 6xlarge. FC runs nested exactly the way it does on GCP (L2
#   under the cloud hypervisor).
#
# - **Nested virtualization is a LAUNCH-TIME flag, off by default.**
#   `cpu_options.nested_virtualization = "enabled"` on the launch
#   template. Without it the guest has no `vmx` and no `/dev/kvm`,
#   kubelet joins happily, the host registers, and the first capture
#   dies with "Error creating KVM object: No such file or directory"
#   (the first AWS bring-up, 2026-09-07). It cannot be flipped on a
#   running instance — replace the instances after changing it.
#
# - **CPUID is a one-way door.** m8i is Granite Rapids; GCP C3 is
#   Sapphire Rapids. Images baked on this fleet are GNR-pinned:
#   newer-platform snapshots never restore on older hardware, so an
#   AWS fleet on m8i bakes its own images and its snapshots do not
#   move to a C3 fleet. Operators who need ONE bake serving both
#   clouds pick `m7i.metal-24xl` instead (Sapphire Rapids, CPUID
#   parity with C3 — metal, because EC2 does not enable nested virt
#   on 7th-gen virtual shapes; needs a metal quota, ~3× the cost).
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
#
# - **The node role is registered with the cluster here.** A
#   self-managed group gets no aws-auth / access-entry mapping from
#   the EKS module (only its managed groups do), so without the
#   `aws_eks_access_entry` below kubelet boots cleanly and then fails
#   authentication forever: instances sit InService, no CSR, no node,
#   the host-agent DaemonSet has zero targets (the first AWS
#   bring-up, 2026-09-04). The ASG waits for the entry.

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

  # The launch-time nested-virtualization flag (see the header). Metal
  # shapes have KVM natively and do not take the flag.
  dynamic "cpu_options" {
    for_each = strcontains(var.instance_type, ".metal") ? [] : [1]
    content {
      nested_virtualization = "enabled"
    }
  }

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

# EKS access entry (API_AND_CONFIG_MAP / API auth modes): lets kubelets
# under the node role join. EC2_LINUX carries the system:nodes policy.
resource "aws_eks_access_entry" "node" {
  cluster_name  = var.cluster_name
  principal_arn = aws_iam_role.node.arn
  type          = "EC2_LINUX"
  tags          = var.tags
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

  # Kubelets must be able to authenticate the moment they boot.
  depends_on = [aws_eks_access_entry.node]
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
