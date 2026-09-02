---
title: "Helm chart: engram-host-fleet"
description: The Firecracker host fleet as a DaemonSet, the operator that rolls and scales it, and the values behind them.
sidebar:
  order: 6
---

`deploy/helm/engram-host-fleet` runs the Firecracker hosts. It deploys the host agent as a
privileged DaemonSet on the nodes labeled `engram.io/kvm=true`, a node-prep DaemonSet that
readies each node, and an operator that rolls and scales the fleet. Install it in its own
namespace with the `privileged` pod security level enforced, because the host agent needs the
node's network, the node's PID namespace, `/dev/kvm`, and the cgroup tree.

## The pieces

**node-prep** runs once per node before the host agent: it loads the NBD module with
`nbd.nbdsMax` devices, sets `vm.unprivileged_userfaultfd` and `net.ipv4.ip_forward`, and
mounts the tmpfs that holds each image's canonical memory.

**node-assets** is an init container on the host-agent pod. It stages Firecracker, the guest
kernel, and the read-only session bundles (the in-guest daemon, the harnesses, the skills)
into a shared directory, because stock Kubernetes nodes do not carry them.

**The host agent** is the DaemonSet's main container. It dials the coordinator named in
`coordinator.endpoint` with the bearer from `coordinator.tokenSecretName`, and runs the VMs.

**The operator**, with `operator.enabled`, owns rollouts and scaling. The DaemonSet's update
strategy is `OnDelete`, so Kubernetes never rolls these pods on its own: the operator drains a
host, replaces its pod, and moves to the next, and a replaced pod reattaches to the VMs the
old one left running. The same operator sizes the node pool from the coordinator's demand
signal through a per-cloud scaler, `gke` or `asg`. With the operator off, roll the DaemonSet
by hand and drain each host first.

## The values that matter

| Section | What it sets |
|---|---|
| `image`, `imagePullSecrets`, `serviceAccount` | The host-agent image and its ServiceAccount, annotated for Workload Identity or IRSA. |
| `nodeSelector`, `tolerations` | Pin the fleet to the KVM pool. The default selector is `engram.io/kvm: "true"`. |
| `coordinator` | `endpoint`, the in-cluster coordinator Service, and the Secret that holds the bearer token. |
| `blob` | The chunk store, matching the control-plane chart. |
| `nodeAssets` | The image that carries Firecracker, the guest kernel, and the bundles. Pin it to a digest in production. |
| `firecracker` | Restore modes, the canonical-memory tmpfs and its size, the density knobs, and `vmCgroupParent`, the node-level cgroup VMs move into so a pod restart does not kill them. |
| `egress` | The proxy's ports and `caSource`, which is `env` on GKE and `aws-secrets-manager` on EKS. The [configuration reference](../configuration/) explains why. |
| `storage` | The host's working directory on the node, the chunk cache budget, and any dedicated devices. The working directory must be a host path so it survives pod restarts. |
| `nbd` | `nbdsMax`, the number of NBD devices node-prep creates, and `warmSlots`, how many the host agent keeps ready. One device serves one running VM. |
| `nodePrep` | The node-prep image and the sysctls it sets. |
| `hostAgent` | Ports, resource requests and limits, and `extraEnv` for any `ENGRAM_*` variable not exposed as a value. |
| `otel`, `metrics` | Traces and Prometheus metrics from the host agent. |
| `updateStrategy` | `OnDelete`. Leave it. |
| `operator` | Whether the operator runs, its image, its own ServiceAccount, and `scaler`: `noop`, `gke`, or `asg`. |

## The custom resource

The chart installs a `HostFleet` custom resource definition and, by default, renders one
`HostFleet` for its DaemonSet. The operator reads it for the fleet's desired size and rollout
state, and the coordinator's demand signal updates it. Terraform seeds the node pool's size
and then ignores it; the operator owns it from there.

## Per-cloud overlays

`values-gcp.yaml.example` and `values-aws.yaml.example` carry the identity annotations, the
CA source, and the scaler for each cloud, and the Terraform quickstarts render a second
overlay with the bucket, endpoint, and Secret names. The bring-up guides show the install with
both files.
