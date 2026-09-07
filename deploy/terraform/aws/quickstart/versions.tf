terraform {
  required_version = ">= 1.5.7" # the EKS module v21 floor

  required_providers {
    aws = {
      # ≥ 6.33 for aws_launch_template cpu_options.nested_virtualization
      # (the KVM fleet is dead without it); ≥ 6.59 for the EKS module v21.
      source  = "hashicorp/aws"
      version = ">= 6.59, < 7.0"
    }
    kubernetes = {
      source  = "hashicorp/kubernetes"
      version = ">= 2.30"
    }
    helm = {
      source  = "hashicorp/helm"
      version = ">= 2.13, < 3.0"
    }
    # kubectl (not kubernetes_manifest) for CRD-typed resources: it
    # defers schema validation to apply time, so ExternalSecret
    # manifests plan cleanly before their CRDs exist on the cluster.
    kubectl = {
      source  = "gavinbunney/kubectl"
      version = ">= 1.14"
    }
    random = {
      source  = "hashicorp/random"
      version = ">= 3.5"
    }
  }
}

provider "aws" {
  region = var.region
}

# Cluster-credentialed providers, fed from the module outputs — one
# apply stands up the cluster AND the in-cluster resources.
#
# Credentials come from `aws eks get-token` via exec, NOT a
# data.aws_eks_cluster_auth token: that token is minted once at plan
# time and lives 15 minutes, so an apply that rolls a node group
# first (the EKS module v21 upgrade did) reaches the in-cluster
# resources with an expired token and fails with "the server has
# asked for the client to provide credentials". Exec re-mints per
# request.
locals {
  eks_exec = {
    api_version = "client.authentication.k8s.io/v1beta1"
    command     = "aws"
    args        = ["eks", "get-token", "--cluster-name", module.eks_cluster.cluster_name, "--region", var.region]
  }
}

provider "kubernetes" {
  host                   = module.eks_cluster.cluster_endpoint
  cluster_ca_certificate = base64decode(module.eks_cluster.cluster_certificate_authority_data)

  exec {
    api_version = local.eks_exec.api_version
    command     = local.eks_exec.command
    args        = local.eks_exec.args
  }
}

provider "helm" {
  kubernetes {
    host                   = module.eks_cluster.cluster_endpoint
    cluster_ca_certificate = base64decode(module.eks_cluster.cluster_certificate_authority_data)

    exec {
      api_version = local.eks_exec.api_version
      command     = local.eks_exec.command
      args        = local.eks_exec.args
    }
  }
}

provider "kubectl" {
  host                   = module.eks_cluster.cluster_endpoint
  cluster_ca_certificate = base64decode(module.eks_cluster.cluster_certificate_authority_data)
  load_config_file       = false

  exec {
    api_version = local.eks_exec.api_version
    command     = local.eks_exec.command
    args        = local.eks_exec.args
  }
}
