# ADR 0042 — supporting evidence (verified research findings)

Companion to [0042](0042-substrate-architecture-survey.md). Verbatim record of the verified findings from the deep-research passes (adversarially verified, 2/3-refute kill threshold) and the direct E2B source analysis, so the evidence survives independent of the synthesis. **Pass 1** (competitive) is recorded below. **Pass 2** (live-migration / network-disk-for-memory / state-of-the-art) will be appended when it lands.

---

## Pass 1 — competitive substrate survey (verified)

Stats: 6 angles, 24 sources fetched, 109 claims extracted, 25 verified, **22 confirmed / 3 killed**, 13 after synthesis.

### Confirmed findings

**Replit — closest disk twin (high confidence).** Persists a Repl's filesystem as **16 MiB chunks in GCS**, served via a custom **Rust NBD server ("margarine")** fronted by a co-located local memory+disk cache, lazily loaded; the Repl establishes the NBD session before any FS access and fetches only touched blocks → "boot instantly regardless of size." Forks a Repl in **constant time** by copying only a **manifest of chunk pointers** (COW, race-free) — "milliseconds whether 100 MB or 100 GB." Their **prior** design (btrfs send/receive serialized at boot) suffered "several minutes," capped Repls at 1 GiB, was "pretty much unusable" — the exact cold-resume-from-full-reconstruct problem; escape was lazy-block NBD-over-GCS.
- Compute substrate: **Docker containers on preemptible GCE** managed by a per-VM "conman" — **not Firecracker**. (Optimizing container *shutdown* — SIGKILL direct to container PIDs, bypassing Docker's serial netlink cleanup — cut p99 session boot 2 min → 15 s.)
- Sources: `replit.com/blog/inside-replits-snapshot-engine`, `replit.com/blog/replit-storage-the-next-generation`, `replit.com/blog/killing-containers-at-scale`.

**CodeSandbox — memory on LOCAL NVMe, not object storage (high confidence).** Firecracker microVMs (Devboxes). Memory snapshots chunked into **8 KiB units, lz4-compressed, stored on local NVMe** for low-latency access; **resume 400 ms avg / P99 2 s / P01 150 ms** at ~**2.5 M VM resumes/month**. Article never mentions object storage for memory.
- **Desparsification:** page-aligned-offset sparse files crippled random-access (btrfs poor at sparse); concatenate chunks contiguously + trailing page-offset manifest → resume **4 s → 1 s**.
- **`MAP_SHARED` continuous flush:** naive FC memory snapshot ~1 s/GB (8 GB ≈ 8 s, the dominant clone cost); mmap `MAP_SHARED` so the kernel lazily syncs dirty pages to the backing file → snapshot-save **8–12 s → 30–100 ms**, I/O off the critical path. Tradeoff: fragments xfs fast.
- **Page Sources Table:** per-4 KB-page source record (Vm/File/ZstdCompressedFile/…/Network?), 4 GiB VM = 1,048,576 entries; UFFD resolves any fault with **no fork-chain traversal**.
- **Density lesson:** old `cp --reflink=always` (xfs) CoW degraded at **50+ VMs/node** (clone 2 s → 14 s; 100k+ small random writes fragment the memory file); replaced by in-userspace UFFD CoW (6 mo in prod), avg fork 1954 ms → 921 ms, P99 17519 ms → 1833 ms.
- **Clone budget:** pause ~16 ms, save ~100 ms, copy mem+disk ~800 ms, start ~400 ms → under 2 s, CoW reduced copy several-seconds → ~50 ms.
- Sources: `codesandbox.io/blog/how-we-clone-a-running-vm-in-2-seconds`, `.../cloning-microvms-using-userfaultfd`, `.../how-we-scale-our-microvm-infrastructure-using-low-latency-memory-decompression`.

**Universal: lazy memory page-in via UFFD (high confidence).** On each guest pagefault the VM pauses, a userspace handler fetches/decompresses the chunk and `UFFDIO_COPY`s it, then resumes; a resume faults in only the ~300–400 MB actually touched even though the guest reports 3–4 GB used (~200–300 ms FC-internal page-in). Confirmed for CodeSandbox + engrams (`engram-uffd-handler`, `RestoreMode::Uffd`) and upstream FC docs.

**Modal — gVisor, not FC (high confidence).** Runs gVisor (runsc userspace kernel); cold-start via gVisor's **built-in checkpoint/restore** (not CRIU). Restore ~**2.5×** faster than container startup (import torch ~5 s → 1.05 s p50 / 0.69 s p0; Stable Diffusion ~13 s → ~3.5 s) by **not waiting to load the full snapshot**: reads pages lazily in the background (prioritizing blocked-on pages) while aggressively preloading the pages file into page cache.
- Sources: `modal.com/blog/mem-snapshots`, gVisor checkpoint/restore docs.

**Morph — FC snapshot+branch (medium; marketing).** Infinibranch/MorphVM claims <250 ms startup (~150 ms restore, ~250 ms branch) vs "2–3 min typical VMs" (strawman baseline). Vendor self-report, no methodology.

**Host-loss / deploy survival (high confidence, by absence).** No provider found does transparent live migration across a rolling deploy without a pause-and-restore primitive. Field splits: explicit snapshot-and-rehome (Replit/engrams) vs suspend/resume (Fly/Morph).

### Killed claims (do NOT rely on)
- "Replit's 16 MiB GCS chunks are content-addressed / nearly identical to engrams" — **refuted 0-3**; Replit uses per-version manifest pointers, not content addressing.
- "CodeSandbox Free-Page-Reporting balloon-inflate-before-snapshot (13.10/16 GiB uninitialized)" — **1-2**, specific figures unpinned.
- "MorphVM instantaneous near-zero-overhead unlimited branching" — **refuted 0-3** as marketing.

### Evidence gaps (NOT researched to bar — absence ≠ nothing to learn)
Fly.io (Machines migrations, volumes on NVMe-over-fabric), Cloudflare (Containers/Durable Objects), AWS Lambda SnapStart, Daytona, Gitpod/Coder — **no claims survived verification.** This is precisely the network-block-storage + live-migration axis → the subject of Pass 2.

### Open questions carried into Pass 2
- For MEMORY specifically: does any provider successfully round-trip guest RAM through object storage on the hot path, or is the universal verdict "keep memory local / lazy-fault, never ship to object storage"? (Every verified point → the latter.)
- Where does the local cache live — compute host (engrams) or a separate co-located storage tier (Replit's margarine)? The latter decouples cache from host rolls.
- `MAP_SHARED` fragmentation cost at engrams' scale + on our cache FS.

### Caveats
All latency/throughput numbers are vendor-published benchmarks on favorable workloads (directionally trustworthy, not contractual). CodeSandbox posts span 2022–2024 (each *extends* the prior; "current" = 2024 decompression design). "This fixes engrams problem N" framing is verifier synthesis — vendor facts are solid; engrams-applicability must be validated against our actual reconstruct/cache code.

---

## Pass 2 — live migration / network-disk-for-memory / state-of-the-art

_(pending — `wf_91afc498`; will be appended on completion)_
