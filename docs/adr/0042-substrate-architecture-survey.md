# ADR 0042: Substrate architecture survey — memory/disk persistence, fast resume, host survival (prior-art + directions)

Status: 2026-06-07 — **Proposed (research / exploration).** This is a *survey* ADR: it records what comparable systems do, what that implies for our substrate bets, and the candidate directions — it does **not** decide a change. A follow-up ADR will pick a direction once the open questions (esp. the live-migration / network-disk-for-memory axis, under active research) resolve.

## Context

A run of prod incidents (see ADR 0038, ADR 0039, the disk-flush-atomicity / NBD / single-flight / evac fixes in PRs #109–#111) made it clear that **almost our entire substrate bug surface clusters in one layer**: "chunk guest memory + disk into content-addressed 16 MiB objects in GCS, front with a local NVMe cache, and *reconstruct per host*." That model exists to make a snapshot **portable** (host A captures → host B rebuilds from GCS). The bugs it produced:

- **chunk-drop** (manifest referenced a chunk that didn't durably upload → guest EIO) — #109;
- **NBD device-open hang** (a slow/missing chunk fetch wedged FC's partition-probe read 60s+ in D-state instead of EIO) — open;
- **single-flight poison-on-cancel** (a cancelled GCS-fetch leader orphaned the in-flight slot → all waiters hang forever) — #111;
- **slow cold resume** dominated by eagerly prefetching guest RAM from GCS (66 s of a 69 s cold resume);
- **deploy storms + evac fragility** — every host-agent merge recreates the FC MIG, killing live sessions and forcing the evac-resumer (single-threaded, unbounded dead-host RPC) to run.

That prompted the question this ADR surveys: **is the chunk-to-GCS + reconstruct-per-host substrate the right shape, or do comparable systems make a fundamentally different bet — especially for memory?** We dug into the E2B OSS codebase directly and ran two adversarially-verified deep-research passes across the field.

## What comparable systems do

### E2B (`e2b-dev/infra`) — read directly from source

Our near-architectural-twin: Firecracker + an **in-process NBD userspace block backend** (COW overlay over a chunked base) + **UFFD memory** + object-store blocks (GCS) with an **NFS warm cache** (`WrapInNFSCache`) and per-sandbox local mmap cache. Key takeaways:

- **They already fixed our exact substrate bugs, with regression tests.** A strict NBD timeout hierarchy (kernel I/O 90 s > per-chunk fetch 60 s > GCS op 5–10 s) and an NBD dispatch loop that converts *any* backend error into an EIO byte while staying alive (`nbd/dispatch.go`, `nbd/path_direct.go`, `path_direct_slow_test.go`) — the fix for our open NBD-hang. Single-flight dedup via `golang.org/x/sync/singleflight` (cleanup in `defer`, runs on cancel/panic) + a streaming chunker that detaches the shared fetch with `context.WithoutCancel` — structurally immune to our poison bug. A snapshot's rootfs diff is **one atomic GCS object per build** (offset-addressed), so there's no per-chunk manifest to dangle a reference into — immune to our chunk-drop class (tradeoff: no content-addressed cross-build dedup).
- **They sidestep our *hardest* bugs by not having the machinery.** No periodic background checkpoints (pause is explicit-RPC only; the GCS upload runs after the VM is killed, off the freeze path). No server-side idle detection (wall-clock TTL, client-extended; evictor default = *kill*, auto-pause opt-in, concurrent-per-sandbox). **No evac-resumer** — on host loss, un-paused sandboxes are *lost*; recovery is only from a prior explicit snapshot, re-scheduled with `Status()==Ready` gating + 70 s/60 s deadlines + 5 s gRPC keepalive + cross-node retry. So our "evac RPC to a dead host hangs 6 min and starves the single-threaded scanner" is structurally impossible.
- **Deploys don't roll live nodes.** Blue/green *by node boot*: hosts version-pin at boot, the old Nomad job is left running (`deregister_on_id_change = false`), the MIG is `OPPORTUNISTIC`/`ONLY_SCALE_OUT`, and FC VMs are detached from the orchestrator process (`context.WithoutCancel` + `Setsid`) so a host-agent restart doesn't kill them. A deploy never touches a node with live sandboxes.
- **Persistent volumes** are a *separate* concern: a per-team POSIX filesystem on **GCP Filestore (managed NFS)**, exposed to the guest via a userspace NFS proxy + iptables redirect (no per-session block-device attach → density-friendly), fully decoupled from sandbox/host lifecycle. Notably, **E2B does NOT put the sandbox rootfs/memory on Filestore** — that state stays on the chunk-to-GCS model with NFS only as a warm cache.

### Competitive deep-research (adversarially verified; sources below)

| Provider | Compute | Memory snapshot | State storage |
|---|---|---|---|
| **Replit** | Docker/GCE containers (**not FC**) | n/a (container; FS-persist) | 16 MiB chunks in GCS + Rust NBD ("margarine") + **co-located** cache; **manifest-only constant-time COW fork** |
| **CodeSandbox** | Firecracker | **local NVMe** (lz4 8 KiB chunks), UFFD lazy page-in, **400 ms** resume, ~2.5 M resumes/mo | local NVMe + CoW |
| **Modal** | **gVisor** (not FC) | gVisor checkpoint/restore (not CRIU), lazy background page-load + page-cache preload, ~2.5× | — |
| **Morph** | Firecracker | FC snapshot + branch, <250 ms *claimed* (deep "instant unlimited branching" claims **refuted** as marketing) | — |

Verified findings that matter most:

1. **Our DISK model is validated.** Replit *and* E2B independently chose chunk-to-GCS + NBD + local cache. Replit's *prior* design — btrfs send/receive serialized at boot, "several minutes," capped at 1 GiB, "pretty much unusable" — is our exact cold-resume-from-full-reconstruct pain; their escape was lazy-block NBD-over-object-storage. (Correction: Replit's chunks are per-version *manifest pointers*, not content-addressed — the "nearly identical to engrams" framing was refuted 0-3. Similar topology, different addressing. And Replit's cache lives on a **separate co-located storage tier, not the compute host** — which decouples cache warmth from host rolls.)
2. **Our MEMORY model is the outlier.** Replit, CodeSandbox, and Modal all keep guest memory **off object storage** — local NVMe, or paged in lazily. CodeSandbox (highest-scale FC operator found) keeps memory snapshots on **local NVMe**, lz4-chunked, **400 ms resume**, and the article never mentions object storage for memory. Engrams (and E2B) are the minority shipping guest RAM through GCS on the resume hot path — which *is* our #1 cold-resume cost.
3. **Lazy page-in via UFFD is universal** (FC: CodeSandbox, engrams) / gVisor background page-load (Modal); resume faults in only the ~300–400 MB actually touched of a 3–4 GB guest.
4. **No provider does transparent live migration across a rolling deploy without a pause-and-restore primitive.** The field splits between snapshot-and-rehome (us, Replit) and explicit suspend/resume (Fly, Morph). This *confirms* that our deploy-survival answer is the deploy-*mechanism* fix, not magic we're missing. (Caveat: the first pass could not verify Fly/Cloudflare/AWS-SnapStart to bar — the **network-block-storage / live-migration axis is an evidence gap**, under active research in the follow-up pass.)

### Concrete, verified techniques worth borrowing (CodeSandbox)

- **Desparsification** — storing memory chunks at page-aligned offsets in *sparse* files crippled random-access reconstruct (btrfs is bad at sparse); concatenating chunks contiguously + a trailing page-offset manifest cut resume **4 s → 1 s**. Direct relief for our sparse-reconstruct bug surface.
- **`MAP_SHARED` continuous dirty-page flush** — a naive FC memory snapshot is I/O-bound at ~1 s/GB (8 GB ≈ 8 s); mmap-ing the memory file `MAP_SHARED` lets the kernel lazily flush dirty pages to the backing file continuously, cutting snapshot-*save* to **30–100 ms** and moving the bulk of I/O off the pause path. This is exactly what ADR 0038 B3 was reaching for. (Disclosed tradeoff: fragments xfs fast.)
- **Per-page Page Sources Table** — one entry per 4 KB page recording its source (another VM / file / compressed chunk / network), so the UFFD handler resolves every fault with **no fork-chain traversal**. A cleaner model than offset-based reconstruct.
- **Density lesson** — filesystem-reflink CoW + eager memory-to-disk sync degraded at **50+ VMs/node** (clone 2 s → 14 s; fragmentation from 100k+ small random writes); the escape was userspace UFFD CoW + lazy/shared flush.

## Synthesis — what this implies for engrams

- **Disk:** keep the chunk-to-GCS + NBD + local-cache model — two independent FC operators (Replit, E2B) converged on it. The work is *hardening* it (the timeout/EIO discipline, atomicity, single-flight cancel-safety — much of which we already shipped in #109–#111 and can finish by porting E2B's NBD timeout hierarchy for the open device-open hang).
- **Memory is the real lever.** Everyone fast keeps memory local; we round-trip it through GCS. The synthesis is **tiered memory**: local-NVMe-primary (fast resume + `MAP_SHARED` off-pause flush) with **GCS only as an async cold/durable backstop**, never on the resume hot path. The open question (active research) is whether a **persistent network disk** can be that durable backstop *and* the fault source — getting host-loss survival without the GCS round-trip.
- **Our hardest bugs are self-inflicted by the durability machinery** (periodic checkpoints + idle-evict + evac) that E2B deliberately doesn't have — but "survive host loss transparently" is a real engrams differentiator we don't want to give up. The reframe: keep the durability for *unplanned* loss, but make *planned* deploys **drain-first / version-pinned / VM-detached** so the evac path almost never runs.
- **The user's stated appetite:** drop *aggressive* continuous memory checkpointing; keep an explicit/infrequent **pause → flush dirty disk+memory → restore** at a relaxed cadence. This aligns with the field (explicit-pause-only is the norm) and with `MAP_SHARED` (flush is continuous-but-cheap, so the relaxed "pause" is near-instant).

## Candidate directions (not yet decided)

**Tier 1 — portable now, low risk:**
- Port E2B's **NBD timeout hierarchy + EIO-not-block dispatch** → fixes the open device-open hang.
- **Deadlines on evac/resume RPCs + concurrent-per-session evac + `Ready`-gate before dispatch** → fixes the evac-starvation / dead-host-RPC class.
- **`MAP_SHARED` continuous flush** to move snapshot-save off the pause path (validate fragmentation on our cache FS first).
- **Desparsified contiguous chunk layout + offset manifest** to shrink reconstruct cost/bugs.

**Tier 2 — structural, needs an ADR + spike:**
- **Tiered memory**: local NVMe primary, async GCS backstop; resume-immediately + prefetch-in-background (the ADR 0039 #19 "high-risk half" we deferred — CodeSandbox/Modal are existence proofs it works).
- **Drain-first / version-pinned host deploys + VM-detach** (E2B's blue/green-by-boot) → eliminates deploy storms; pairs with the existing ADR 0028 drain-before-roll follow-up.
- **Cache on a separate co-located storage tier** (Replit-style) to decouple warmth from host rolls.

**Tier 3 — under active research (follow-up pass `wf_91afc498`):**
- **Persistent network disk as the memory backing** — mmap the FC memory file off a reattachable network block device (Hyperdisk/PD/NVMe-oF), UFFD-fault from it; "pause" = flush dirty pages to that disk (block write, not GCS upload); "host loss / deploy" = detach + reattach + re-fault. Eliminates the chunk-store for memory entirely *if* the latency/density/cost work.
- **Post-copy / cross-host lazy memory fault** (QEMU-style post-copy via UFFD): resume on the destination immediately and fault pages from the source/network — true near-zero-pause migration.
- **Far / disaggregated memory** (RDMA paging, CXL.mem) as a shared persistent memory tier — almost certainly research-only/speculative on GCP today, but worth scoping.

## Open questions (the follow-up research pass is answering)

1. Does a persistent network disk (or far/disaggregated memory) actually beat object-storage-chunking + local cache **for memory specifically**, and on which axis (fault latency, host-loss survival, cost, density)? Concrete Hyperdisk/PD random-4 KB-read latency vs GCS vs local NVMe.
2. What does "move a running session across hosts" cost under each model — post-copy live migration vs snapshot-and-reattach-disk vs snapshot-to-GCS-and-rehome?
3. The Fly/Cloudflare/AWS evidence gap on network-block-storage + live-migration that the first pass couldn't verify.
4. Is there a **non-obvious technique the incumbents are NOT doing** that engrams could combine (network-disk-backed UFFD memory, unified reattachable disk for disk+memory, post-copy cross-host fault)?

## Sources

Prior art read directly: `e2b-dev/infra` (orchestrator `pkg/sandbox`, `pkg/nfsproxy`, `pkg/volumes`; `shared/pkg/storage`; `iac/`).

Verified competitive research (primary engineering blogs):
- Replit — *Inside Replit's Snapshot Engine* (Dec 2025); *Replit Storage: The Next Generation* (2023); *Killing Containers at Scale* (2020).
- CodeSandbox — *How we clone a running VM in 2 seconds* (2022); *Cloning microVMs using userfaultfd* (2023); *How we scale our microVM infrastructure using low-latency memory decompression* (2024).
- Modal — *Memory snapshots: Checkpoint/restore for sub-second startup* (2025); gVisor checkpoint/restore docs.
- Firecracker NSDI'20; FC snapshot/UFFD docs; AWS Lambda SnapStart (under-the-hood).

(Tier-3 sources to be appended when `wf_91afc498` lands.)

## Status

Proposed (research). No code change. Decision deferred to a follow-up substrate ADR once the live-migration / network-disk-for-memory questions resolve.
