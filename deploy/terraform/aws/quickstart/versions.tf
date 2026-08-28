terraform {
  required_version = ">= 1.5"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = ">= 5.60"
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
data "aws_eks_cluster_auth" "this" {
  name = module.eks_cluster.cluster_name
}

provider "kubernetes" {
  host                   = module.eks_cluster.cluster_endpoint
  token                  = data.aws_eks_cluster_auth.this.token
  cluster_ca_certificate = base64decode(module.eks_cluster.cluster_certificate_authority_data)
}

provider "helm" {
  kubernetes {
    host                   = module.eks_cluster.cluster_endpoint
    token                  = data.aws_eks_cluster_auth.this.token
    cluster_ca_certificate = base64decode(module.eks_cluster.cluster_certificate_authority_data)
  }
}

provider "kubectl" {
  host                   = module.eks_cluster.cluster_endpoint
  token                  = data.aws_eks_cluster_auth.this.token
  cluster_ca_certificate = base64decode(module.eks_cluster.cluster_certificate_authority_data)
  load_config_file       = false
}
