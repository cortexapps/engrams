# ADR 0044: Migrate the Firecracker host fleet from GCE MIGs to Kubernetes

Status: 2026-06-07 — **Proposed.** Codebase survey + architecture + the verified prior-art research (pass `wfbnl7mjc`) are folded in; the K8s-first decision stands as *defensible-but-pioneering* (see the precedent reckoning). Remaining before Accepted: the operator/autoscaling/security specifics + a feasibility spike. Supersedes the MIG-specific halves of the ADR 0043 deploy work (Phase 3a drain-first, 3d version-pin) — reborn here as operator rollout logic.

## Context

engrams runs its coordinator on GKE (Helm) but its **Firecracker hosts on a GCE Managed Instance Group** (`deploy/terraform/gcp/modules/fc-host-mig/`): a Packer-baked image, a `data.google_compute_image` family pin that triggers a `tf-apply` rolling MIG update on every host-agent change, and a CPU-based region autoscaler. Controlling deployments through that MIG has been the persistent operational friction (the ADR 0043 deploy phases exist to tame it).

Two goals motivate moving the hosts to Kubernetes:

1. **Portability + easier adoption.** A K8s deployment is far easier for OSS users of engrams to stand up than "bake a GCE image + provision a MIG + wire Terraform", and it's portable across clouds (the coordinator is already K8s-native; the hosts are the last GCP-specific piece).
2. **Central declarative management → autoscaling.** A CRD + operator can manage the fleet declaratively (version, count, rollout strategy) and is the natural substrate for **demand-based autoscaling** off the capacity signal the coordinator already collects — strictly better than the MIG's CPU heuristic.

A code survey (two Explore passes) established what the host-agent needs from its host and what's GCP-specific vs portable. The findings are favorable.

## What the survey found

**Already portable (no work, or done):**
- **`BlobStorage` is a trait** with GCS / local / S3 impls (`engram-core/src/traits/storage.rs`; `engram-host-agent/src/blob.rs` selects via `ENGRAM_BLOB_BACKEND`). S3 is "reserved" but the AWS SDK is wired — provider-agnostic storage is an env flag + un-reserving S3, not a rewrite. Works against S3/MinIO/R2.
- **The coordinator is already on GKE/Helm** — portable to any K8s.
- **No Firecracker jailer.** The host-agent spawns `firecracker` directly (`Command::new`, `engram-sandbox-firecracker/src/lib.rs`), *not* via the jailer — so there is no chroot/cgroup/CAP_SYS_ADMIN jailer machinery to reproduce in a pod (usually the hardest part of FC-on-K8s).
- **Instance-metadata discovery soft-fails** (`main.rs` GCE metadata → falls back) and is trivially replaced by the K8s Downward API (pod/node IP).
- **OTel collector** is already in the Helm chart; **drain hook** (`engram-drain.sh`) already exists.

**Host-level requirements (→ pod equivalents):**

| Requirement | Current (MIG / systemd) | K8s pod equivalent | Difficulty |
|---|---|---|---|
| KVM `/dev/kvm` | nested-virt VM, root | privileged + `/dev/kvm` device | easy |
| FC spawn | `Command::new`, root (no jailer) | privileged | easy |
| NBD (`modprobe nbd`, `/dev/nbdN`, ioctls) | module baked + udev + `nbds_max=64` | node prep + `/dev/nbdN` hostPath + privileged | medium |
| userfaultfd | `vm.unprivileged_userfaultfd=1` sysctl | node sysctl + `/dev/userfaultfd` | easy |
| Networking (tap, per-VM netns, iptables, SNAT) | host root netns + `CAP_NET_ADMIN` | **`hostNetwork: true`** + `NET_ADMIN`/`NET_RAW` | medium* |
| Local NVMe (chunk cache, work dir) | `/var/lib/engram/*` on local SSD | hostPath / local PV | easy |
| Guest kernel + RO bundles | baked into Packer image | container image layer / RO hostPath / init-container | easy |
| Host identity + heartbeat | per-process UUID + gRPC register | same; stable id ties to ADR 0043 P3c | easy |

\* The survey flagged per-VM netns and `/dev/nbdN` slots as "hard/medium blockers" — but **only for a multi-pod-per-node model.** They evaporate under the model below.

## Decision — the model: one privileged host-agent DaemonSet pod per node

Run the host-agent as a **privileged DaemonSet, one pod per KVM-capable node, with `hostNetwork: true`.** This maps **1:1** onto today's "one systemd host-agent per MIG VM": the pod lives in the node's root netns and owns the node's `/dev/kvm`, `/dev/nbdN`, taps, and iptables exactly as the systemd unit does. The per-VM netns + NBD-slot model is therefore **unchanged** — the "blockers" only arise if multiple host-agent pods contend for one node's network/devices, which this model never does.

**The real crux is node-level OS setup + the nested-virt ceiling, not the application.** KVM modules, `modprobe nbd nbds_max=64`, `vm.unprivileged_userfaultfd=1`, iptables persistence — the things Packer bakes today — are node concerns, handled by either (a) a **pre-baked node image** (the same Packer logic, targeting the K8s node OS) or (b) a **privileged node-prep DaemonSet/init container** on node join. `/dev/kvm` reaches the pod via a **privileged hostPath** (the canonical KVM device plugins — KubeVirt's, cgwalters' — are archived/incubating, not production-grade, so hostPath is the realistic path).

The hard external constraint is **nested virtualization, and it is Intel-only on every managed provider examined** (verified, research pass `wfbnl7mjc`):
- **GKE:** Standard clusters only (**not Autopilot**), Intel Haswell+ `minCpuPlatform` (**no AMD, no Arm/Tau**), enabled **only at node-pool creation** (not toggleable), and **incompatible with node auto-provisioning** — which directly constrains the autoscaling vision below.
- **EKS:** bare-metal instances, *or* (new Feb 2026) nested-virt on Intel **Xeon-6 C8i/M8i/R8i** only (no AMD/Graviton); AWS itself recommends **bare metal** for latency-sensitive virtualization — relevant to engrams' latency priority.
- **AKS:** unverified (open).

So "portability across providers" is real but **bounded to specific Intel node families + specific cluster configs** — there is no Arm/Graviton Firecracker-on-managed-K8s path today. This is the portability ceiling.

### Deploy-mechanism mapping

| Current (GCP) | Kubernetes |
|---|---|
| FC-host MIG + rolling update | privileged DaemonSet on a tainted, nested-virt node pool |
| Packer image (base + thin) | container image + node-prep (pre-baked node image *or* setup DaemonSet) |
| `tf-apply` family-pin roll | **Fleet CRD + operator** (digest-pinned, drain-gated, node-by-node rollout) |
| GCE metadata server | Downward API (pod/node IP) |
| GCS bucket | `BlobStorage` S3/MinIO/GCS (env flag) |
| GCP Secret Manager | K8s Secrets / external-secrets |
| MIG CPU autoscaler | cluster-autoscaler/Karpenter on the node pool, driven by coordinator capacity metrics |

### The CRD + operator (the central-management goal)

A `HostFleet` (or `EngramFleet`) **CRD** declares: target image digest, node-pool selector, desired capacity floor/ceiling, and rollout policy. A **controller** reconciles it:
- **Drain-first, node-by-node rollout**: cordon a node → tell the coordinator to drain it (`POST /api/hosts/:id/drain?migrate_active=true`) → wait for **drain-complete** (the external gate) → replace the pod/node → next. This is ADR 0043 Phase 3a expressed as operator logic instead of MIG `max_surge` mechanics.
- **Version pinning** = the digest in the CR (ADR 0043 Phase 3d), rolled intentionally, not on a family-pointer move.
- **Autoscaling**: the coordinator already emits capacity + utilization in its heartbeat (`HostUtilization`, `HostCapacityReport`). Feed that to a custom-metrics autoscaler (KEDA / HPA-on-external-metrics) that scales the node pool on **session demand**, with cluster-autoscaler/Karpenter materializing nodes. Mind the **cold-node problem** (a fresh node has a cold chunk cache → its first sessions pay full reconstruct; pairs with prefetch/warm-pool work).

## How this reshapes ADR 0043 Phase 3 (decided: K8s-first)

- **3a (drain-first) and 3d (version-pin)** are MIG mechanics → **reborn as the operator's rollout logic** above. We do *not* build them on the MIG and throw them away.
- **3c (VM-detach / `kill_on_drop` removal)** is **more** important here — pod restarts are routine on K8s, and a host-agent pod restart must not kill the node's running microVMs. The pidfd-reattach hardening carries over directly and becomes a prerequisite.
- **3b (retire evac)** is coordinator-side and **unaffected** — proceeds independently.

## Migration path (phased, draft)

1. **Portability prerequisites (no K8s yet):** un-reserve the S3 `BlobStorage` backend + test against MinIO; replace GCE-metadata discovery with explicit/Downward-API advertise addr; make node-prep (modules/sysctls/iptables) reproducible outside Packer.
2. **DaemonSet on a hand-rolled node pool:** privileged host-agent DaemonSet + node-prep on a nested-virt node pool (GKE first), `hostNetwork`, devices via hostPath; boot one FC microVM end-to-end (the spike). Validate networking/NBD/UFFD parity with the MIG.
3. **VM-detach (ADR 0043 P3c):** host-agent pod restart survives the node's VMs (reattach), so DaemonSet rollouts don't drop sessions.
4. **Fleet CRD + operator:** declarative version + drain-gated node-by-node rollout.
5. **Autoscaling:** coordinator-capacity-driven node-pool scaling; address the cold-node warm-up.
6. **Cut over + retire the MIG/Packer/Terraform host path.**

## Prior art + the precedent reckoning (research findings)

The most important finding cuts against the grain: **no comparable production microVM platform runs Firecracker inside Kubernetes pods.** Every peer with public detail chose a non-K8s orchestrator:
- **Fly.io** built a custom orchestrator (`flyd`) after starting on Nomad, and explicitly rejected K8s ("Kubernetes is out"). Documented reason: async/consensus scheduling is the wrong fit for **synchronous scale-from-zero on a live request**.
- **E2B** (the closest open-source peer) runs entirely on **Nomad + Consul**, zero K8s, with a custom Nomad node-pool autoscaler, on nested-virt `m8i.4xlarge`.
- **Koyeb** migrated *off* K8s to Nomad+Firecracker.
- The only "production-blessed" FC-on-K8s route in the literature is **Kata Containers' `kata-fc` RuntimeClass** (firecracker-containerd's own CRI/K8s support is explicitly roadmap-only, "multi-tenant at your own risk"). And FC networking doesn't compose with CNI — FC only takes a tap NIC; `tc-redirect-tap` bridges it, and under CRI the CNI config ends up *inside* the VM (the FC authors flag this as "complexity and confusion").

**Why this is much weaker for engrams than for the peers — the decisive nuance.** The Fly/E2B objection targets letting an async orchestrator **schedule the latency-critical workload** (the microVM, on a live request). engrams **does not delegate that**: the coordinator schedules sessions itself (`host_registry.pick_for_session` picks a host and dials it directly). K8s would orchestrate **only the host-agent DaemonSet** — one long-lived privileged daemon per node — *not* session placement. Session scale-from-zero stays on the coordinator's synchronous path, untouched by K8s scheduling latency. So the single biggest documented anti-K8s reason **does not bind us.** Likewise, we run our **own** host-agent driving FC directly, so the `kata-fc` storage (devmapper, no virtiofs) and CNI-inside-VM constraints **don't apply** — `hostNetwork: true` puts the host-agent in the node root netns where its existing tap/iptables/NBD model works unchanged.

**What remains genuinely true after that nuance:** we'd be **first** to run a production FC fleet on K8s pods (no precedent to copy); nested virt is Intel-only + config-constrained; and the privileged-DaemonSet surface is real. So K8s-first is *defensible for engrams specifically* — and it directly serves the #1 goal (deploy-ease/adoption: most users already run K8s, few run Nomad) — but it is pioneering, not well-trodden.

## Alternatives considered

- **Keep the MIG, finish ADR 0043 Phase 3 on it.** Faster pain relief, but builds drain-first/version-pin logic we'd redo for the operator, and keeps the GCP lock-in + adoption friction. Rejected per the K8s-first decision.
- **Nomad + Consul (E2B's proven path).** The de-facto industry choice for exactly this workload — proven, no nested-virt-scheduling friction. **The real contender to K8s.** Rejected as the *primary* target because it loses the #1 goal (Nomad is far less ubiquitous than K8s for OSS adopters), and engrams' coordinator-owns-scheduling model neutralizes Nomad's main advantage over K8s here. **Worth keeping as the fallback** if the K8s spike (Phase 2 below) hits a wall — the host-agent-as-a-privileged-DaemonSet design ports to a Nomad system-job with little change.
- **`kata-fc` RuntimeClass / KubeVirt** instead of our own host-agent-in-a-pod. Rejected: they run VMs-as-pods under their own runtime, which doesn't fit our chunk-store/UFFD/snapshot substrate; our host-agent already *is* the FC driver, so we bypass them.

## Open questions

**Resolved by the research pass (`wfbnl7mjc`):** nested-virt feasibility per provider (Intel-only; GKE-Standard + creation-time + no-auto-provisioning; EKS bare-metal or Intel Xeon-6; AKS unverified); `/dev/kvm` exposure (privileged hostPath — device plugins are archived/incubating); FC-on-K8s prior art (kata-fc only blessed route; firecracker-containerd CRI is roadmap-only; CNI-inside-VM friction — all bypassed by our own-host-agent model); K8s-vs-Nomad (everyone chose Nomad/custom for *scheduling latency*, which doesn't bind engrams since the coordinator schedules sessions itself).

**Still open (research concentrated on substrate/precedent; these went unverified — resolve before Accepted):**
1. **Operator drain-gated-rollout patterns** — how the NVIDIA GPU Operator / node-feature-discovery / KubeVirt drive node-by-node privileged-DaemonSet rollouts, and the pattern for gating each step on an *external* "drain complete" signal from the coordinator (vs node-readiness alone).
2. **Cold-node + autoscaling specifics** — pre-warming a freshly-added node's chunk cache; cluster-autoscaler/Karpenter on a fixed nested-virt instance family (since GKE node-auto-provisioning is unavailable here); KEDA/HPA-on-external-metrics driven by the coordinator's capacity signal.
3. **AKS nested-virt support** — the third managed provider, unverified; needed to claim true cross-cloud portability.
4. **Privileged-DaemonSet security posture** — dedicated tainted node pool, Pod Security Admission exemption scope, blast radius of `privileged + /dev/kvm + hostNetwork`.

## Sources

Codebase survey (two Explore passes over `engram-host-agent`, `engram-sandbox-firecracker`, `deploy/`). Verified research pass `wfbnl7mjc` (24/25 claims confirmed): GKE/GCE nested-virtualization docs; AWS EC2 nested-virtualization (Feb 2026 launch + User Guide); KubeVirt + cgwalters KVM device plugins; Kata Containers hypervisors + AWS Kata-on-EKS blog; firecracker-containerd networking/roadmap docs; Fly.io "carving the scheduler" blog; e2b-dev/infra (Nomad+Consul); Koyeb K8s→Nomad blog. Full verified findings + per-claim sources in the workflow output.

## Status

Proposed (draft). The model + the K8s-first/reframe-Phase-3 decision are settled and the research is folded in. The research *complicates* the call honestly — no production precedent, Intel-only nested virt, Nomad is the proven peer choice — but the coordinator-owns-scheduling nuance neutralizes the main objection and K8s ubiquity serves the deploy-ease goal, so K8s-first stands as defensible-but-pioneering. Remaining: the operator/autoscaling/security specifics (above) + a proof-of-concept spike. No code yet — this gates the reframed Phase 3.
