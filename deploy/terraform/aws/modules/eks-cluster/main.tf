# EKS cluster for the engram control plane — the AWS twin of
# gcp/modules/gke-cluster (ADR 0122).
#
# A thin wrapper over the pinned community module
# (terraform-aws-modules/eks): raw EKS is aws-auth/addon/bootstrap
# boilerplate the community module already gets right — same
# don't-re-roll-clusters posture as the GCP README. The wrapper pins
# the version and the engram-shaped settings:
#
# - IRSA (the OIDC provider) — every workload identity binds through
#   it; the charts' `eks.amazonaws.com/role-arn` annotations are
#   inert without it.
# - A small managed node group for the control-plane pods
#   (coordinator / web / orchestrator / operator / ESO). The KVM
#   fleet is the SEPARATE self-managed kvm-nodegroup module — a
#   managed group cannot host it (the ADR 0048 operator actuates the
#   ASG directly, and managed groups fight external actuation).
#
# The AWS Load Balancer Controller install (helm) lives in the
# quickstart, which holds cluster-credentialed providers — this
# module stays pure-AWS.

module "eks" {
  source = "terraform-aws-modules/eks/aws"
  # v21: AWS provider ≥ 6.59. The kvm-nodegroup's launch-time nested-
  # virtualization flag needs provider ≥ 6.33, which v20 (< 6.0) pins
  # out.
  version = "~> 21.25"

  name               = var.name
  kubernetes_version = var.cluster_version

  vpc_id     = var.vpc_id
  subnet_ids = var.private_subnet_ids

  # Public endpoint (kubectl from anywhere; the API server is
  # auth-gated), private nodes.
  endpoint_public_access = true

  enable_irsa = true

  # The applier becomes cluster-admin — the quickstart's in-cluster
  # resources (namespaces, ESO, the DB-init Job) apply in the same
  # run.
  enable_cluster_creator_admin_permissions = true

  addons = {
    coredns    = {}
    kube-proxy = {}
    vpc-cni    = {}
  }

  eks_managed_node_groups = {
    control-plane = {
      instance_types = [var.control_plane_instance_type]
      min_size       = var.control_plane_min_nodes
      max_size       = var.control_plane_max_nodes
      desired_size   = var.control_plane_desired_nodes

      labels = {
        "engram.io/control-plane" = "true"
      }
    }
  }

  tags = var.tags
}
