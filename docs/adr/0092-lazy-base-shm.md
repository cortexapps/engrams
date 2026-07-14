# ADR 0092: Base memory belongs in reclaimable page cache — File-unpinned fresh creates, lazy resume base, guest-side first-message fixes

- Status: **Proposed** — rewritten 2026-07-14 after the full WS0
  measurement campaign (prod-artifact A/B on the dev VM), then revised
  the same day after a follow-up spike showed the ADR 0022 File path,
  minus its `mlock`, gives *kernel-managed* base residency: same
  first-message latency, and the host reclaims base pages under
  pressure instead of OOMing. The first draft proposed *sparse*
  (hot-set) tmpfs residency driven by working-set traces; the
  measurements below show the trace machinery is unnecessary, tmpfs
  pinning is avoidable for fresh creates entirely, and first-message
  latency — the reason eager residency exists at all — is not a
  memory-tiering problem. Moves to Accepted when the canary clears its
  gates.
- Date: 2026-07-13; rewritten 2026-07-14
- Workstream: WS0 of the dev-brain campaign-remediation plan
- Related: ADR 0022 (the canary-gate template), ADR 0045 (base-shm +
  UFFDIO_CONTINUE substrate), ADR 0070 (NVMe keep-set pins), ADR 0043
  (restore-time chunk prefetch), ADR 0037/0062 (persistent harness +
  bundle stamps), ADR 0025 (guest kernel), ADR 0046/0048 (placement
  reservations/packing).

## What are we actually optimizing?

Two goals, previously conflated because eager base residency was
assumed to buy latency at the cost of RAM:

1. **Density** — less pinned host RAM per enabled image, so more
   sessions pack per node. Today every FC host permanently holds every
   enabled image's full base memory in tmpfs: measured **20.68 GiB for
   dev-brain alone** (42,355 × 512 KiB chunks over a 24 GiB guest — the
   first draft's "~7 GiB" was the lean cold-base seed of ADR 0084, not
   the warm base fresh sessions actually restore from). That pin is why
   a 64 GiB host fits one dev-brain session instead of two.
2. **Time-to-first-message (TTFM)** — session create → first assistant
   token from the harness.

**Measured conclusion: the two are (almost) independent.** Eager
residency buys ~2 s of a ~46 s TTFM; the other ~40 s is the guest's own
wake-up behavior. So each goal gets its own lever.

## What we measured (2026-07-14, dev VM, byte-exact prod artifacts)

Setup: prod dev-brain base snapshot copied byte-exact from GCS
(snapshot `43dca346`, memory manifest `4ffe6d43@v2`, disk chain
`cbcb93c5@v53`), FC fork pinned to the gitlink artifact
(`--snapshot-version v10.0.0`, matching the snapshot), full stack
(coordinator + host-agent + orchestrator), real sessions with a Claude
OAuth token, n2-highmem-16. Raw runs archived on the dev VM
(`~/.adr92/runs2/`).

### A/B: full prewarm vs all-holes base (NVMe chunks pinned in both)

| stage (create → first reply)            | A: full prewarm | C: all holes |
|-----------------------------------------|-----------------|--------------|
| create gRPC incl. restore leg           | 2.7–2.9 s       | 4.3–4.5 s    |
| — FC `resume` span (the only real delta)| 1.7 s           | 3.7–3.9 s    |
| prompt dispatch (outbox tick)           | 2.3–2.5 s       | 0.8–4.8 s    |
| harness cold start + first turn         | 40.3–41.1 s     | 39.2–40.6 s  |
| **total**                               | **45.5–46.8 s** | **45.8 s** (worst sample 54.4 s) |

On a RAM-starved 62 GiB box the all-holes run degraded by **+36 s** —
the contended-host tail that gates the rollout. On a healthy host,
full-vs-holes is measurement noise.

### Where fault-serving actually spends (perf + `strace -c` on the handler, ~23k chunk-populates/20 s)

- 95.6% of handler CPU is kernel time: `pwrite64 →
  shmem_alloc_and_add_folio` is 55% (tmpfs page allocation, incl. 10.7%
  memcg charge). Populating tmpfs is the floor; the copy isn't.
- `UFFDIO_CONTINUE` costs **43 µs/call (4% of total)** — the serving
  primitive is nearly free.
- Handler overheads worth retiring eventually (not load-bearing here):
  per-chunk `openat`+`statx`+`close` of cache files (~5%), ~2.3 `read`s
  per chunk (27%), fault-loop futex contention (33%).
- Populate throughput from warm NVMe: ~1.5–2 GB/s. A hollow base
  refills to full in ~15 s under a live guest; the eager prewarm costs
  ~45 s per host roll at ~470 MB/s.

### Erosion: the working set IS the base (for dev-brain)

The handler's working-set recorder shows the resumed guest demand-faults
**~12,100 chunks (5.9 GiB) in the first 5 s** and converges the base to
full ~15 s after resume — dev-brain's resumed JVM fleet (GC catch-up,
Spring health checks, timer floods after the clock step) touches
essentially everything. Consequences:

- A host **actively running** a dev-brain session holds full residency
  under ANY policy. There is no steady-state RAM saving while a session
  runs, and hot set ≈ full base for this image — the first draft's
  trace-driven sparse mode would have saved nothing on active hosts.
- The saving is on hosts holding an image **without an active session**
  (the actual fleet pain: every host pins 21.6 GiB regardless of use)
  and on multi-image hosts.
- Holes are stable while idle: the readiness recheck re-warms only on
  file *absence* (`exists()`), so a lazily-populated base is not fought
  by the prefetch supervisor.

### File mode without the mlock: kernel-managed residency at TTFM parity

The ADR 0022 File path (fresh creates `MAP_PRIVATE` a per-image
`memory.bin` materialized on NVMe at residency) still exists and is the
substrate-off default. As built it `mlock`s the whole memfile
(`pin_memfile`, added because the 2026-06 prod canary measured an
evicted-cold restore at ~4.7 s vs ~0.6 s warm) — so today it pins
~24 GB of *locked page cache*, no better than tmpfs. Spiked with the
pin gated off:

| config | create→first reply | residency w/ live session | under 85 GiB ballast (125 GiB host) |
|---|---|---|---|
| substrate tmpfs (prod) | 45.5–46.8 s | 20.7 GiB pinned | unreclaimable |
| File + mlock (ADR 0022 as-built) | 44.7 s | 24 GB locked cache | unreclaimable |
| **File unpinned** | **47.1 s** (run noise) | 24 GB *reclaimable* cache — host `available` **+23 GB** even mid-session | kernel evicted 12.6 GiB (LRU kept the hot half), session unaffected, post-evict prompt **0.37 s** |

Supporting facts: the memfile materializes in ~18 s from warm NVMe
chunks (~1.3 GB/s, one-time per image per host), costs ~24 GB disk next
to the ~21 GB chunk cache, siblings share its page cache
(`file_restore_shared_rss` proves Σpss ≪ Σrss), and the old 4.7 s
evicted-cold penalty is (a) bounded, (b) paid only when the alternative
was OOM, and (c) invisible inside a ~46 s TTFM dominated by the guest
stampede.

One blind spot found en route: deleting a memfile under a live pin is
not detected (the supervisor's recheck never re-materializes; the old
process holds a phantom mlock on the unlinked inode). The recheck must
watch the path like it watches base-shm.

### Resumes still need the tmpfs base

An idle-resume composes canonical pages + the session's memory diff.
`UFFDIO_CONTINUE` (the composition mechanism) is kernel-restricted to
shmem, so a resume cannot map `memory.bin` directly; the File-mode
resume alternative materializes a full per-session memory.bin (a ~24 GB
disk write per resume — why the substrate exists). So the tmpfs base
survives, but only as the **resume** substrate — and it can be lazy:
populated on demand by what resumed sessions actually touch, instead of
20.7 GiB unconditionally.

### TTFM: the 40 s is the guest, not the substrate

- Cold `claude --print` on a **quiet** guest: **2.8 s** (3.0 s under
  strace — 5.9k syscalls, no slow calls, no gaps, no TLS/DNS stalls).
- The same cold spawn through the **full harness path** with the first
  prompt delayed 90 s: **2.7 s** (`run_started` → reply in 2.46 s).
- The same prompt riding the boot: **39–41 s**, every run. The freshly
  spawned harness needs ~1 s of CPU and gets it ~15× diluted by the
  wake-up stampede across all 8 vCPUs.
- Substrate-independent: A ≈ C on this number. Also noteworthy: a
  bundle-stamp mismatch forces an agentd refresh + fresh harness spawn
  on every create (all dev creates; prod creates whose stamp matches
  the capture reattach the warm harness and skip the cold spawn
  entirely).

## Decision

### Goal 1 (density): evictable base for creates, lazy tmpfs for resumes

Three knobs, each defaulting to today's behavior, read once at
host-agent startup, rolled out together in the ADR 0022 canary shape
(canary config = `file` + pin off + `lazy`):

1. **Fresh creates go File-unpinned.**
   - `ENGRAM_FC_FRESH_RESTORE_MODE ∈ {derived (default), file, uffd}`:
     today `effective_restore_mode` forces Uffd for fresh creates
     whenever `uffd_base_dir` is set; `file` overrides that so a
     substrate host (which still needs UFFD for resumes) restores fresh
     creates from the per-image memfile. The memfile materializer
     already exists (ADR 0022) but is gated to substrate-off hosts —
     re-derive `base_memfile_dir` from this mode instead of from
     `uffd_base_dir`.
   - `ENGRAM_FC_BASE_MEMFILE_PIN ∈ {1 (default), 0}`: gates
     `pin_memfile`'s mlock. `0` = kernel-managed residency — the
     measured config: TTFM parity, +23 GB reported-available per host
     with a live session, graceful reclaim under pressure instead of
     OOM.
   - Fix the memfile recheck blind spot: re-materialize when the
     memfile path vanishes (mirror the base-shm `exists()` recheck).
2. **The tmpfs base becomes resume-only and lazy.**
   `ENGRAM_FC_BASE_SHM_MODE ∈ {full (default), lazy}`: in `lazy`,
   `prewarm_base_shm` is a no-op — the handler creates/sizes the base
   and populates canonical pages on first fault, so tmpfs holds only
   what resumed sessions actually touch. The **NVMe chunk prefetch +
   keep-set pin stays mandatory and unchanged** (ADR 0070): a
   GCS-served fault storm is the catastrophic case this substrate must
   never hit. The RAM ledger already accounts by `st_blocks`; freed
   residency is visible to placement with no accounting change.
3. **No trace machinery anywhere on this path.** The first draft's
   hot-set prewarm — and its canonical-trace-at-bake prerequisite — is
   dropped. If the canary shows an unacceptable contended tail, hot-set
   insurance can be revisited as a follow-up; it is insurance, not the
   mechanism.
4. **Prerequisite hardening** (bugs found during the campaign):
   - The tmpfs headroom-skip is sticky: a skipped prewarm never retries
     when tmpfs frees (nothing invalidates readiness). Mode-independent
     bug; fix the retry.
   - Orphaned VMs from failed creates pin tmpfs/RAM and silently starve
     prewarm/placement (the campaign's silent-reserve-reject class);
     the teardown reconciler must reap them. Root cause found: the
     coordinator's `session_owning_sandbox` query is malformed and
     errors on every call, so the host's unbound-sandbox arm always
     "assumes owned"; `sandbox_ownership` also ignores terminal status.
   - Keep the absence-triggered re-warm (`exists()` recheck); in `lazy`
     mode it re-creates the empty file only.
5. **Canary gates** (single kvm-pool host carrying dev-brain, ≥24 h):
   - Residency: `utilBaseShmMib` ≈ resume working set only; host
     MemAvailable higher by ~the base size even under active sessions
     (the memfile is reclaimable cache). Packing math and ADR 0046/0048
     reservations still budget active sessions at full guest size.
   - Latency: p50/p95/**p99** first-prompt vs control, including under
     two concurrent boots plus an in-session build (the +8 s / +36 s
     contended samples are the risk; eviction-then-refault adds a
     bounded ~4.7 s worst case only under real pressure).
   - No BlobStorage round-trips on the fault path (keep-set holds).
   - Disk headroom: +~24 GB/image for the memfile next to the chunk
     cache — confirm fleet NVMe sizing.

### Goal 2 (TTFM): fix it where it lives — the guest

Base-shm policy moves this number by ±2 s. Lever ordering CORRECTED
after implementation-time validation (2026-07-14, PR #669):

1. ~~Harness spawn priority~~ — **implemented and measured
   insufficient**: with the harness verified running at nice -10
   mid-storm, first replies stayed 38.9–39.5 s. The starvation is not
   CPU-scheduler contention: the cold start is **fault/IO-bound** — its
   first-touch page faults and cold disk reads queue behind the JVM
   stampede in the single handler fault loop (33% futex serialization
   in the profile) and the NBD daemon, which priority cannot jump. The
   change ships anyway as cheap, correct insurance for CPU-shaped
   contention, but is not the 40 s fix.
2. **Warm reattach does not exist for fresh creates today**: dev-brain's
   base capture carries the *sentinel* at dyn_0 — no captured harness —
   so every fresh create cold-spawns by construction (reattach only
   serves resumes of a session's own snapshot). The real fresh-create
   levers are therefore:
   - **a warm harness in the base capture** (ADR 0037's persistent-
     harness design, revived for base captures): the harness's pages
     become canonical base pages (shared, pre-faultable) and the cold
     start disappears entirely; or
   - **capture-time quiesce** for warm images (settle GC/timers before
     the `[warm]` capture) so the resume stampede the cold start queues
     behind is smaller.
   Both need their own design pass — follow-up ADR material, not this
   one.
3. Minor: the prompt-dispatch outbox tick contributes ~2.3 s median;
   event-driven dispatch would shave it.

Debuggability rider: the ADR 0025 guest kernel compiles out PSI
(`/proc/pressure`); enable it in the next kernel rev — this
investigation had to infer guest starvation from the host side.

## Consequences

**Positive**
- Pinned base RAM goes from **20.7 GiB/image always** to **zero for
  fresh creates and resume-working-set-only for resumes** — and the
  create-side residency that remains is *reclaimable page cache* the
  kernel trades against real demand (measured: 12.6 GiB reclaimed under
  pressure with a live, unaffected session). Directly attacks the
  21.6 GiB/host ceiling and the 1-session-per-host regression at
  **measured TTFM parity**.
- Under memory pressure the failure mode changes from OOM-kill to
  bounded re-fault latency (~4.7 s worst case, prod-canary measured).
- Retires the eager prewarm's ~45 s per-roll cost, the canonical-trace
  prerequisite, and the sparse draft's trace plumbing.
- TTFM work is unblocked from the memory substrate entirely and lands
  as small guest-side changes with measured 15× headroom.

**Negative / risks**
- +~24 GB disk per image for the memfile (next to the ~21 GB chunk
  cache) — cheap, but fleet NVMe sizing must be confirmed.
- The contended-host tail is real (+36 s measured on a RAM-starved
  box, ~4.7 s evicted-cold re-faults); the canary p99 gate is the
  decision point, and every knob defaults to today's behavior.
- Resume-heavy hosts still grow tmpfs residency toward the resume
  working set (the ledger reports truth; reservations budget
  worst-case).
- The NVMe keep-set becomes even more load-bearing: a swept cache means
  GCS-latency faults (the PR #634 incident class).

## Go / No-Go

**GO.** Implement the three knobs (File-unpinned fresh creates, lazy
resume base) plus the hardening items, canary on the kvm pool with
`file` + pin-off + `lazy`, promote to defaults on the gates above. The
TTFM follow-ups (harness spawn priority first) proceed independently —
they are where first-message latency actually is.

---
*Measurement appendix: raw run dirs, perf data, and strace logs on the
dev VM (`~/.adr92/runs2/`, `/tmp/handler-perf.txt`,
`/tmp/handler-stracec.txt`); the four-cell TTFM matrix (Mac baseline,
quiet-guest direct, quiet-guest harness-path, storm-time) and the
per-stage A/C tables above are reproducible via `~/.adr92/measure-run2.sh`.*
