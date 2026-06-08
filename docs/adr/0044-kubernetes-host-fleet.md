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
2. **DaemonSet on a hand-rolled node pool (THE SPIKE):** privileged host-agent DaemonSet + node-prep on a nested-virt node pool (GKE Standard first), devices via hostPath; boot one FC microVM end-to-end. **Start from `forkd`'s `packaging/k8s/forkd-controller.yaml`** (known-good privileged pod spec: `/dev/kvm` hostPath, cgroup-v2 hostPath, `NET_ADMIN`/`SYS_ADMIN`). Resolve the **netns decision** here: `hostNetwork: true` (host-agent owns the node root netns + iptables, as today) vs forkd's pod-netns + per-child-netns model (better isolation). Validate tap/NBD/UFFD parity with the MIG.
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

**The one counter-example — `forkd` (deeplethe/forkd), which validates this exact model.** A kindred open-source FC-snapshot project (fork-from-warm, a `MAP_SHARED` mem-backend patch, diff-snapshot chains — engrams' ADR 0042/Phase-6 territory) ships a Kubernetes deployment built on precisely the model below: **"one controller Pod hosts N sandbox children; the K8s scheduler runs once at Pod creation regardless of fan-out, unlike Kata / Firecracker-on-K8s designs that schedule one Pod per sandbox."** Their starter manifest (`packaging/k8s/forkd-controller.yaml`) is verified to the point that the controller Pod runs on a bare-metal k3s node (`/healthz`, bearer-auth work); actually forking FC VMs in-pod is set up but not over-claimed end-to-end. The pod spec is the reference for our spike: `privileged:true` + `runAsUser:0` + `NET_ADMIN`/`SYS_ADMIN`, `/dev/kvm` hostPath CharDevice, `/sys/fs/cgroup` hostPath (cgroup v2), amd64+KVM `nodeSelector`, `Recreate` strategy. It independently confirms the nested-virt ceiling ("managed K8s typically does not qualify unless metal SKU or explicit nested virt") and explicitly **punts on the exact things engrams' operator provides** — multi-node DaemonSet, drain-first rollout (it uses `Recreate`!), and autoscaling. So engrams' design = **forkd's validated single-host model + the production fleet-management forkd doesn't attempt.** Notably, forkd does *not* use `hostNetwork` — its controller manages per-child netns *inside the pod's netns* (children inherit pod PID/IPC). That's a viable, better-isolated alternative to the `hostNetwork: true` proposal below, and a concrete spike question.

**Why the precedent gap is much weaker for engrams than for the Nomad peers — the decisive nuance.** The Fly/E2B objection targets letting an async orchestrator **schedule the latency-critical workload** (the microVM, on a live request). engrams **does not delegate that**: the coordinator schedules sessions itself (`host_registry.pick_for_session` picks a host and dials it directly). K8s would orchestrate **only the host-agent DaemonSet** — one long-lived privileged daemon per node — *not* session placement. Session scale-from-zero stays on the coordinator's synchronous path, untouched by K8s scheduling latency. So the single biggest documented anti-K8s reason **does not bind us.** Likewise, we run our **own** host-agent driving FC directly, so the `kata-fc` storage (devmapper, no virtiofs) and CNI-inside-VM constraints **don't apply** — `hostNetwork: true` (or forkd's pod-netns model) keeps the existing tap/iptables/NBD model working.

**What remains genuinely true after that nuance:** no one runs a *large-scale production* FC fleet on K8s yet, so we'd be ahead of the field at scale (though `forkd` proves the single-host shape works + hands us a known-good manifest); nested virt is Intel-only + config-constrained; and the privileged-DaemonSet surface is real. So K8s-first is *defensible for engrams specifically* — it serves the #1 goal (deploy-ease/adoption: most users already run K8s, few run Nomad), has a kindred reference to copy, and avoids the peers' one binding objection — but at scale it's still pioneering, not well-trodden.

## Alternatives considered

- **Keep the MIG, finish ADR 0043 Phase 3 on it.** Faster pain relief, but builds drain-first/version-pin logic we'd redo for the operator, and keeps the GCP lock-in + adoption friction. Rejected per the K8s-first decision.
- **Nomad + Consul (E2B's proven path).** The de-facto industry choice for exactly this workload — proven, no nested-virt-scheduling friction. **The real contender to K8s.** Rejected as the *primary* target because it loses the #1 goal (Nomad is far less ubiquitous than K8s for OSS adopters), and engrams' coordinator-owns-scheduling model neutralizes Nomad's main advantage over K8s here. **Worth keeping as the fallback** if the K8s spike (Phase 2 below) hits a wall — the host-agent-as-a-privileged-DaemonSet design ports to a Nomad system-job with little change.
- **`kata-fc` RuntimeClass / KubeVirt** instead of our own host-agent-in-a-pod. Rejected: they run VMs-as-pods under their own runtime, which doesn't fit our chunk-store/UFFD/snapshot substrate; our host-agent already *is* the FC driver, so we bypass them.

## Open questions

**Resolved by the research pass (`wfbnl7mjc`):** nested-virt feasibility per provider (Intel-only; GKE-Standard + creation-time + no-auto-provisioning; EKS bare-metal or Intel Xeon-6; AKS unverified); `/dev/kvm` exposure (privileged hostPath — device plugins are archived/incubating); FC-on-K8s prior art (kata-fc only blessed route; firecracker-containerd CRI is roadmap-only; CNI-inside-VM friction — all bypassed by our own-host-agent model); K8s-vs-Nomad (everyone chose Nomad/custom for *scheduling latency*, which doesn't bind engrams since the coordinator schedules sessions itself).

**Resolved by the K3/K4 designs (see implementation log):**
1. ~~Operator drain-gated-rollout patterns~~ → **K3**: cordon (K8s node + coordinator) → `admin/drain` → poll `running_sandboxes → 0` as the drain-complete gate, with a PG-derived `drain-status` endpoint as the robust follow-up.
2. ~~Cold-node + autoscaling specifics~~ → **K4**: feed the coordinator's `HostUtilization`/`HostCapacityReport` to KEDA / HPA-on-external-metrics scaling a *fixed* nested-virt instance family (node-auto-provisioning is incompatible with nested virt); mitigate the cold chunk cache by scaling ahead of demand + prefetch.

**Still open (resolve before Accepted):**
3. **AKS nested-virt support** — the third managed provider, unverified; needed to claim true cross-cloud portability.
4. **Privileged-DaemonSet security posture** — the hostNetwork-vs-pod-netns choice resolved to **pod-netns** (the spike + K2 reattach both use it). Remaining: a **dedicated tainted node pool** + a **dedicated namespace at PSA-`privileged`** (so the coordinator's namespace stays restricted) + a KVM device plugin to scope `/dev/kvm` (later hardening, not a blocker). The blast radius of `privileged + /dev/kvm + hostPID` is real and bounded by the tainted pool.

## Sources

Codebase survey (two Explore passes over `engram-host-agent`, `engram-sandbox-firecracker`, `deploy/`). Verified research pass `wfbnl7mjc` (24/25 claims confirmed): GKE/GCE nested-virtualization docs; AWS EC2 nested-virtualization (Feb 2026 launch + User Guide); KubeVirt + cgwalters KVM device plugins; Kata Containers hypervisors + AWS Kata-on-EKS blog; firecracker-containerd networking/roadmap docs; Fly.io "carving the scheduler" blog; e2b-dev/infra (Nomad+Consul); Koyeb K8s→Nomad blog. Direct read of **`deeplethe/forkd`** (`packaging/k8s/forkd-controller.yaml` + `packaging/k8s/README.md`) — the one-controller-Pod-N-children K8s model + a verified privileged-pod reference manifest. Full verified findings + per-claim sources in the workflow output.

## Spike findings (2026-06-07 — k3s on a KVM dev-vm)

The KVM-in-pod question — the #1 unknown — is **empirically retired.** On a single-node **k3s v1.35** cluster (the same k3s line forkd verified on), a privileged pod (`privileged: true`, busybox base, the static `firecracker` binary + the engram guest kernel + an ext4 rootfs all mounted via hostPath, `/dev/kvm` CharDevice) **booted a real Firecracker microVM end-to-end**: FC reported "Successfully started microvm", the guest kernel detected nested KVM and booted (`Hypervisor detected: KVM`, kvm-clock, virtio-mmio), mounted the ext4 rootfs over virtio-blk, and reached `Run /bin/sh as init process` (no panic). The pod ran in its **own CNI netns** (IP 10.42.0.4, **no `hostNetwork`**), so the boot path needs neither hostNetwork nor a tap.

**Then the full substrate was driven in-pod** — the existing FC integration tests, run inside a privileged `ubuntu:24.04` pod (own CNI netns, no hostNetwork; `/nix` + repo + artifacts + `/dev` hostPath-mounted, `NET_ADMIN`+`SYS_ADMIN`), all green:
- **NBD chunked-disk + FC boot** (`nbd_chunked_disk`, **2 passed**): the engrams chunk-store NBD daemon served `/dev/nbd0` via the kernel ioctls, FC booted off the NBD-backed rootfs, the guest sentinel round-tripped byte-identical, clean teardown.
- **UFFD restore** (`snapshot_uffd`, **2 passed**) — including `uffd_restore_succeeds_when_memory_bin_absent_locally`, the cross-host materialize-from-chunks + UFFD-restore case.
- **Networking primitives** (the `net.rs` operations, in the pod's own netns): `ip tuntap add`, `ip addr/link`, `ip netns add`, veth-into-netns, `ip netns exec`, `iptables -t nat MASQUERADE` — all succeeded.

So **the entire engrams substrate runs inside a k3s pod**: KVM + FC microVM + the NBD chunked-disk daemon + UFFD restore + tap/netns/iptables. The two test "failures" along the way were both harness artifacts (an absolute kernel symlink that dangled in the pod; `CARGO_MANIFEST_DIR` not set when running the prebuilt test binary directly) — **not** substrate limitations. Caveats: this is a *single-node* k3s on a nested-virt GCE VM, devices via broad hostPath + `privileged: true` (production would tighten to a KVM device plugin / scoped caps), and the `vm.unprivileged_userfaultfd=0` node still worked because the pod runs root (UFFD-as-non-root would need the node sysctl). Manifests on the dev-vm: `~/fc-spike.yaml`, `~/fc-parity.yaml`, `~/fc-parity-uffd.yaml`, `~/tap-check.yaml`.

This retires the feasibility question. What remains is **design + production-hardening**, not "does it work": the `HostFleet` CRD + drain-gated operator, the node-prep + tightened security posture (device plugin, tainted pool, PSA), multi-node + autoscaling, and the `hostNetwork`-vs-pod-netns choice (the spike used pod-netns throughout and it worked).

## Implementation log

### Phase plan (K1–K5)

The migration path above, expressed as the discrete phases we ship under. Each maps to a "Migration path" step; these K-labels are what the chart comments, PRs, and the deployment cutover runbook all reference.

| Phase | Scope | Migration step | Status |
|---|---|---|---|
| **K1** | `engram-host-fleet` Helm chart — host-agent + node-prep DaemonSets, self-contained image | 1–2 | shipped (#120, #121) |
| **K2** | VM-detach — a host-agent restart leaves the node's microVMs running; the successor pidfd-reattaches them. Includes the stable-HostId + cgroup-escape + listener-rebind completions | 3 | shipped #123 (dev-vm-validated) |
| **K3** | `HostFleet` CRD + drain-gated operator — declarative, digest-pinned, node-by-node rollout | 4 | designed below |
| **K4** | Demand autoscaling — scale the nested-virt node pool off the coordinator's capacity signal | 5 | designed below |
| **K5** | Cutover + retirement — stand the K8s fleet up alongside the existing fleet, shift load, drain, retire the MIG/Packer/Terraform host path | 6 | designed below |

Two cross-cutting prerequisites recur below and are flagged where they bind:
- **Stable HostId** — the host-agent must keep one identity across a pod restart, or detach+reattach is pointless (the coordinator would migrate the live VM out from under itself). Landed in K2.
- **Node-asset staging** — stock K8s nodes have no `firecracker` binary or guest kernel (Packer baked them on the MIG); the node-prep DaemonSet must stage them. A K5 prerequisite.

### K1 — `engram-host-fleet` Helm chart (shipped: #120, #121)

The host-agent + node-prep DaemonSets, `OnDelete` update strategy (the operator drives rollout, not K8s), and the self-contained host-agent image. #121 folded `e2fsprogs`/`iproute2`/`iptables` into the runtime image — on the GCE/Packer hosts these came from the node image; in a container they must be bundled (k3s dev-vm crashed at `host_startup` without `mke2fs`).

### K2 — VM-detach (in review)

**Goal:** a host-agent pod restart (the routine DaemonSet upgrade) must not drop the node's sessions. This is ADR 0043 Phase 3c, and it's *more* load-bearing on K8s than on the MIG because pod restarts are routine.

**Model — detach + live-reattach, decided with the user:**
- **The VM lifecycle is fully decoupled from the host-agent process.** FC (and any uffd-handler) are spawned **without `kill_on_drop`**, so no host-agent lifecycle event — graceful SIGTERM, panic-unwind, or a dropped backend — ever kills a running VM. The host-agent only kills a VM via an explicit `destroy()`. The create/restore *window* keeps a precise backstop: a `SpawnKillGuard` SIGKILLs a half-spawned process on any `?` early-return, disarmed once the sandbox is committed.
- **SIGTERM = detach + exit.** The successor pod pidfd-reattaches the still-live VMs off their on-disk `sandbox.json` manifests, so the restart drops zero sessions. The chart sets **`hostPID: true`** so FC lives in the node's PID namespace and survives the pod's teardown; the reattach pass is now **unconditional for the FC backend** (no longer gated on `ENGRAM_LIVE_ATTACH`).
- **The on-shutdown SIGTERM checkpoint pipeline was removed entirely** — `ENGRAM_GRACEFUL_SHUTDOWN`, the per-sandbox snapshot fan-out, the final checkpoint-flush heartbeat, the `last_local_snapshot` manifest field, and the `live_attach` path-2 NVMe-restore that consumed it. It was the source of several durability incidents (idle-evict bricking a live session, the ADR-0028 "sandbox not found", the disk-flush atomicity bug), and detach makes it redundant for the routine case. **Durability boundary:** an *uncontrolled* node loss is covered by the always-on **periodic checkpoint** (the ADR 0043 P2a 10-min cadence — untouched here; worst case ≤ one interval lost, recovered elsewhere). A *controlled* drain migrates active sessions off the node first (the K3 operator / admin endpoints), so by the time SIGTERM lands there is nothing to lose. The host-agent never tries to be clever on the way out.

**Reattach covers all active sessions, both lineages.** The cold/host-root path already worked; restored VMs were the gap because (a) the restore path never persisted a manifest, and (b) warm restores run in a **per-VM netns** (ADR 0014 M1.16 — the snapshot bakes a fixed TAP name + guest IP that FC v1.10.1 won't let us re-point, so identical restores must be netns-isolated). Both are closed: manifest persistence is unified across create + restore (host-root `network` *or* per-VM `netns` recorded, plus the uffd-handler pid), and `reattach_sandbox` gained the **symmetric netns branch** — re-reserve the SNAT `/30` from the *same* host-pool allocator, verify the netns survived, rebuild `NetnsSetup`. The netns fork is forced by Firecracker, not a design choice; the reattach branch is a parallel to the host-root one, reusing the same allocator. (A reattached uffd-handler is now tracked by pid so `destroy()` can reap it — a latent leak the old code had.)

**Stable HostId (GAP 1, shipped in K2).** Detach+reattach is only useful if the successor re-registers under the *same* host identity — otherwise the coordinator sees the old host go silent, reconcile (≈15s strike-out, 30s dead-host) migrates the live VM out from under itself, and the successor re-adopts a VM the coordinator already moved. Two halves, both load-bearing: (1) the host-agent now resolves a **stable** id — persisted in the work_dir hostPath, **node-name-seeded** on K8s via the Downward API — instead of a per-process UUID; (2) the coordinator's heartbeat self-heal re-points its gRPC dial channel when a *known* host's advertised addr changes (a restarted pod keeps the stable id but gets a fresh POD_IP — previously the self-heal only fired for *unknown* hosts, so it kept dialing the dead IP). A bare `kubectl delete pod` with no drain still races the reattach pass against the strike-out window; the K3 operator's drain-gate removes that race for planned rolls.

**Surviving the pod, not just the host-agent.** Two mechanics make the FC processes outlive a pod restart. `hostPID: true` shares the node's PID namespace, so deleting the pod's PID-1 no longer tears down a namespace full of VMs. But that's necessary, not sufficient: FC is still a *member of the pod's cgroup*, and on a systemd-cgroup-driver node (GKE) the pod-stop `cgroup.kill`s that whole cgroup — SIGKILLing FC despite hostPID. So the FC backend also **moves each VM's FC + uffd-handler into a node-level cgroup** (`<parent>/<sandbox_id>/`; the chart sets `ENGRAM_FC_VM_CGROUP_PARENT=/sys/fs/cgroup/engram-vms`), out of the pod's scope. Off by default (no parent ⇒ no escape — correct for the MIG/dev where there's no pod scope), loud on failure. **And the harness survives the gap:** the in-guest `engram-harness-claude` decouples the `claude` run from the host vsock link — on disconnect it *pauses* via backpressure rather than dropping events, re-dials, and replays the in-flight event (at-least-once). So reattach only has to **re-bind the host-side harness/forge/upload vsock listeners** (a gap the first cut missed — create + restore bound them, reattach didn't), and the session resumes losslessly.

**Validation:** macOS compile + clippy (`-D warnings`) + the unit suites (incl. host-id + node-cgroup helpers) are green; `tests/reattach_detach.rs` (create → drop backend → assert FC alive → assert the harness listener is *gone* → reattach → assert it's *re-bound* → destroy-by-pid) **passes on real KVM** in CI's `test-firecracker` job. **Validated on the dev-vm (cgroup v2 + systemd — GKE-like)** before merge: a systemd-scope experiment confirmed both the threat and the fix — a child process left in a service's cgroup is `cgroup.kill`ed on `systemctl stop`, while one **moved out to a node cgroup survives** (FC is a *child* of the host-agent, reached only via the cgroup, so this is exactly the right shape); and the escape's exact ops (`create_dir_all` of `<parent>/<id>` + pid-write to `cgroup.procs` + `rmdir`) migrate a real process on cgroupfs. So every mechanism the two fixes rely on — detach, reattach, listener re-bind, cgroup escape — is proven against the cgroup driver GKE uses. The remaining *optional* integration test is the full claude-harness loop (a real session, host-agent restart mid-prompt, prompt round-trips); it exercises the *same* harness reconnect/backpressure path already proven for snapshot/restore, gated only on the now-tested listener re-bind — integration confidence, not a new mechanism. Merged in #123.

### K3 — `HostFleet` CRD + drain-gated operator (designed)

K1's chart uses `updateStrategy: OnDelete` precisely so K8s never rolls a host-agent pod out from under a live microVM. K3 is the controller that *does* drive rollout — declaratively, digest-pinned, drain-gated, node-by-node. It's ADR 0043 Phase 3a (drain-first) + 3d (version-pin) reborn as operator logic instead of MIG `max_surge` mechanics.

**CRD.** A `HostFleet` resource declares: the host-agent **image digest**, the **node-asset digest** (the `firecracker`+kernel bundle, see K5), a **node-pool selector**, a **capacity floor** (never drain below N schedulable hosts), and a **rollout policy** (max-unavailable, surge). Version changes are a digest edit on the CR — rolled intentionally, never on a registry-tag move.

**Controller reconcile loop** (per node needing the target digest, oldest first):
1. **Cordon** the K8s node *and* the coordinator's view of the host (`POST /api/admin/hosts/:id/cordon`) so `pick_for_session` stops placing new sessions there. The cordon is in-memory + best-effort PG and a concurrent heartbeat can clobber it back to `Ready` — the operator must **re-assert** until drain completes (a PG-anchored cordon is the real fix).
2. **Drain** via `POST /api/admin/hosts/:id/drain` (202 `{evacuating, failures}`) — this evacuates each active session (`Evacuating` = snapshot + warm-restore on a peer), so the fleet must always have **receiving capacity** (hence the floor).
3. **Gate on drain-complete.** There is no drain-complete endpoint today; v1 **polls** the host's heartbeat `running_sandboxes → 0` *and* `failures == []`, with a timeout that aborts the roll (don't delete the pod) on stall. A future `GET /api/admin/hosts/:id/drain-status` (derived from PG session states) is the robust replacement — it gives a real terminal signal instead of inferring from a count that can briefly read 0 mid-reattach. **(Resolves open question #1.)**
4. **Roll** the node — swap the node-asset digest (drain-gated; you can't change the FC binary under a VM that will be reattached) and delete the host-agent pod (or recreate the node). Drained, K2 reattach has nothing to re-adopt — clean.
5. **Verify** the successor pod is `Ready` + re-registers under the **same stable HostId** (K2) + heartbeats the new digest, then **uncordon** both.
6. **Next node**, respecting the floor.

**Interim, before the operator exists:** a host-agent `preStop` hook (the K8s analog of the MIG's `engram-drain.sh`) that cordons + drains + polls `running_sandboxes → 0` within `terminationGracePeriodSeconds`. Its limits — bounded by the grace period and needs receiving capacity — are exactly why the operator-driven drain (which gates *before* deleting the pod, with no grace-period race) is strictly better. Until the operator lands, **every host-agent image update must go through the preStop-drain path, never a bare `kubectl delete`** (with `OnDelete`, a tag bump does nothing until pods are deleted — easy to `rollout restart` and silently lean on the K2 reattach-vs-strike-out race).

**Tech:** Rust + `kube-rs`, a new crate (no CRD/controller code exists today).

### K4 — demand autoscaling (designed)

The coordinator already emits `HostUtilization` + `HostCapacityReport` in every heartbeat. Feed that to a custom-metrics autoscaler (KEDA / HPA-on-external-metrics) that scales the **node pool** on session demand, with cluster-autoscaler / Karpenter materializing nodes. The hard constraint: **node auto-provisioning is incompatible with nested virtualization** (it's a creation-time, fixed-`minCpuPlatform` property), so the autoscaler drives a **fixed nested-virt instance family**, not arbitrary shapes. The **cold-node problem** is the load-bearing caveat: a freshly-added node has an empty chunk cache, so its first sessions pay full reconstruct from blob storage — autoscaling must scale *ahead* of demand and/or pre-warm a new node's cache (pairs with prefetch / warm-pool work) rather than reactively. **(Resolves open question #2.)**

### K5 — cutover + retirement (designed)

The cutover is a **parallel run**, not a flag-day. The K8s fleet and the existing (MIG) fleet register with the *same* coordinator and the scheduler (`pick_for_session`) places sessions across both uniformly — it doesn't know or care which is which. So you stand the K8s fleet up alongside, shift load by cordoning the old fleet, drain it node-by-node (the K3 path), and retire. Two prerequisites:

- **Node-asset staging (the hard blocker).** Stock K8s nodes have no `firecracker` binary or guest kernel — the chart hostPath-mounts them as `type: File`, so a missing file is a hard pod failure, not a soft fallback. The node-prep DaemonSet (which already does `modprobe nbd` + sysctls) gains an **init container** that stages both from a dedicated, digest-pinned `node-assets` image (built in the same pipeline as the host-agent binary, off the same artifacts the Packer provisioners use) into the hostPath; the host-agent pod gets a wait-for-assets init container so it can't crash-loop on the mount. Chosen over a custom node image (rebuilds the GCP lock-in + Packer pipeline this ADR exists to delete) or baking into the host-agent image (couples the asset version to the agent version + muddies the K2 reattach/binary-skew story).
- **Snapshot/CPUID compatibility.** Evacuation = snapshot + warm-restore on a peer, so the new fleet's CPU platform must be CPUID-compatible with the old or restored VMs fault — pin the new node pool to the same `minCpuPlatform`.

Retirement then removes the MIG-specific surface (the `fc-host-mig` Terraform module, the Packer base/thin bake, the family-pointer roll workflows) and, optionally, **un-reserves the S3 `BlobStorage` backend** (`engram-coordinator`/`engram-host-agent` `blob.rs` — currently errors "reserved") for true cross-cloud portability; the trait + GCS impl are proven, S3 is straightforward. Deployment-specific cutover mechanics (node-pool provisioning, Workload Identity, values parity, per-step verify/rollback) live in the deployment repo's runbook, not here.

## Status

Proposed. The model + the K8s-first/reframe-Phase-3 decision are settled, the research is folded in, and the **full substrate-parity spike has passed** (above) — KVM + FC boot + NBD chunked-disk daemon + UFFD restore + tap/netns/iptables all run in a privileged k3s pod. The research complicates the call honestly — no *large-scale* production precedent, Intel-only nested virt — but `forkd` proves the shape, our own spike proves the *whole substrate* in-pod, the coordinator-owns-scheduling nuance neutralizes the peers' main objection, and K8s ubiquity serves the deploy-ease goal. K8s-first now stands as **substantiated**, with feasibility retired; what's left is design + production-hardening, not "does it work". Implementation is underway (see the Phase plan + implementation log above): **K1** (Helm chart) shipped, **K2** (VM-detach — incl. stable HostId + the cgroup-escape/listener-rebind completions) **shipped + dev-vm-validated** (#123). **K3 is the next phase.** Remaining before Accepted: **K3** (`HostFleet` CRD + drain-gated operator) → **K4** (demand autoscaling) → **K5** (cutover + MIG/Packer/TF retirement; **node-asset staging** is its hard prerequisite). The hostNetwork-vs-pod-netns choice resolved to **pod-netns** (the spike + K2 reattach both use it).
