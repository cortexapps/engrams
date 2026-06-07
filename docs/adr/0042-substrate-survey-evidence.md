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

## Pass 2 — live migration / network-disk-for-memory / state-of-the-art (verified)

Stats: 5 angles, 22 sources, 106 claims, **25 verified / 25 confirmed / 0 killed** (the structured synthesis array truncated to a "size probe"; findings reconstructed from the verification logs + 4 detailed findings + source list, all 3-0 unless noted).

### Live migration

- **Post-copy live migration (the "move without dropping" primitive):** transfer only CPU state, **resume the destination immediately**, then demand-page guest RAM over the network from the source via userfaultfd + a background push, **each page transferred at most once**. Modern QEMU UFFD post-copy downtime is **~7–12 ms** (vs the 2009 Xen ~600 ms–1 s, which was implementation-bound). Sources: `qemu.org/docs/master/devel/migration/postcopy.html`, Hines'09 (`kartikgopalan.github.io/publications/hines09postcopy_osr.pdf`).
- **Post-copy host-failure caveat:** state is split across source+destination, so **failure of EITHER side loses the guest**; QEMU `postcopy-recover` handles only network blips, not a host crash. *Mitigated only if the working set is independently durable* — which is engrams' opening. Source: QEMU docs.
- **Stock Firecracker does NOT implement true live migration** (pre- or post-copy); its only cross-host primitive is snapshot-on-source → restore-on-destination, and FC's UFFD is a **local** lazy restore from an mmapped file, not network post-copy. True FC live migration exists only in forks. Sources: FC discussion #3119, `snapshot-support.md`.
- **FC's UFFD restore is page-source-PLUGGABLE** (the key seam): the user-written handler receives the userfaultfd + memory layout over a Unix socket and resolves faults via `UFFDIO_COPY` from any source; upstream demos a local file only, so network fetch must be hand-rolled — and **E2B/CodeSandbox/BuildBuddy already hand-roll network-backed UFFD handlers**. Sources: FC `handling-page-faults-on-snapshot-resume.md`, issue #2938.
- **Fly.io Machine migrations** — cold move / suspend-resume / volume-reattach (sources: `fly.io/blog/machine-migrations`, `fly.io/docs/reference/machine-migration`).

### Network-disk / far / disaggregated memory for VM RAM

- **FluidMem** backs an *unmodified* KVM/QEMU VM's entire guest RAM with external/remote memory via userfaultfd — transparent paging of VM RAM from a network tier; median page-fault latency in the **tens-of-μs** range (RDMA); cites OSDI'16 "Network requirements for resource disaggregation." Source: `arxiv.org/pdf/1707.07780`. *(Proof that VM RAM can be paged from a remote tier transparently.)*
- **Fastswap** (OSDI'20) — kernel driver + scheduler for **page-granular RDMA far-memory paging**. **AIFM** (OSDI'20) — application-integrated far memory at object (not page) granularity. **Carbink** (OSDI'22, Google) — application-runtime far memory over one-sided RDMA. **TMO** (ASPLOS'22, Meta) — transparent memory offloading. All RDMA-class latency (single-digit–tens of μs); all require an RDMA fabric. Sources: `clusterfarmem/fastswap`, `osdi20-ruan`, `osdi22-zhou-yang`, `tmo_asplos22`.

### Fast snapshot-restore research (the lazy-fault-done-right consensus)

- **REAP** (ASPLOS'21), **FaaSnap** (EuroSys'22), **Catalyzer** (ASPLOS'20) converge: don't full-load, don't pure-serial-lazy-fault — **record the working set on first run and prefetch exactly those pages (contiguously) on restore**, then lazy-fault the cold tail. The fix for the serial-fault storm; what engrams' deferred ADR 0039 #19 was circling. Sources: `marioskogias.github.io/docs/reap.pdf`, `faasnap-eurosys22.pdf`, `dl.acm.org/doi/10.1145/3373376.3378512`.

### Drafter + Silo — read directly from source (`~/test/drafter` + `silo` v0.2.21)

Drafter (`loopholelabs/drafter`, **archived**, AGPL-3.0) is a thin FC orchestrator; the real machinery is **`silo`** (separate AGPL module). It's the closest existing implementation of the unified-disk+memory, post-copy, durable-backed model engrams is reaching for.

- **Unified device abstraction:** *everything* — guest disk AND the FC memory file — is a `storage.Provider` (`ReadAt`/`WriteAt`/`Size`/`Flush`), composed in one stack: backing → CoW base+overlay → `dirtytracker` → `expose` (NBD) → `migrator` → S3-sync (`silo/pkg/storage/{storage.go,device/device.go,devicegroup/}`). Disk and memory share **one** migration + durability + dirty-tracking path. The FC **memory file is exposed as an NBD device** (`File` backend with `Shared:true`, MAP_SHARED) — *not* via FC's UFFD seam (silo has **no userfaultfd at all**; it faults memory at the NBD/block layer or via `/proc/<pid>/mem`).
- **Post-copy:** hybrid pre-copy (dirty-block streaming, `DirtyManager` convergence → suspend → authority transfer) + **post-copy by default** — the destination resumes the VM *before all data arrives* (`drafter-peer/main.go:273-283`); a guest read of a missing block blocks in `waitingcache.Local.ReadAt`, the first waiter fires `NeedAt(offset,len)` to the source, which prioritizes that block (`silo/pkg/storage/waitingcache/`, `protocol/`). **Block-granular** (KiB–MiB), custom wire protocol (not gRPC), per-device goroutine pools.
- **THE key pattern for engrams — durable-backed migratable device** (`device.go:375-653` + `sources/s3_storage.go`): a `sources.S3Storage` Provider stores each block as an offset-keyed S3 object (≈ engrams' GCS chunk store); a background `migrator.Syncer` continuously replicates dirty blocks to S3 while the VM runs. On migration the source emits `AlternateSource{offset,len,hash,location}` for blocks already safe in S3; the destination **pulls cold blocks from S3 in parallel and only faults the hot delta P2P from the source** (`WriteCombinator` merges two prioritized inputs: P2P vs object-store, verify-by-SHA-256). So one device is simultaneously live (NBD), P2P-migratable, AND continuously object-store-checkpointed. *Caveat:* the S3 sync is a lagging bulk replica (CheckPeriod/MaxAge), not a crash-consistent in-flight-RAM snapshot — a mid-migration host crash recovers "from last S3 state + surviving overlay," not a transparent transactional resume.
- **Requires a forked Firecracker** (`loopholelabs/firecracker` `release-main-live-migration` + forked go-sdk) adding `Msync`/`MsyncAndState` snapshot types that flush the MAP_SHARED memory mmap *without a full pause* (the dirty-sync enabler). Cannot reproduce on stock FC without the fork or an equivalent UFFD/NBD dirty-flush.
- **Maturity/caveats:** archived but a real product (backed a KubeCon cross-continent live-migrate demo; productized as "Architect"); silo migration logic is sophisticated + test-covered. **Both AGPL-3.0** → treat as a *design reference to reimplement*, not code to import (engrams is OSS publishing images; vendoring imposes copyleft). Offset-keyed (no content dedup — engrams' content-addressing is strictly better here); block sizes far finer than engrams' 16 MiB chunks.
