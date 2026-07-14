# ADR 0092: Base-shm residency is a density lever, not a latency lever — populate lazily, fix first-message latency in the guest

- Status: **Proposed** — rewritten 2026-07-14 after the full WS0
  measurement campaign (prod-artifact A/B on the dev VM). The first
  draft proposed *sparse* (hot-set) residency driven by working-set
  traces; the measurements below show the trace machinery is
  unnecessary for the win, and that first-message latency — the reason
  eager residency exists at all — is not a memory-tiering problem.
  Moves to Accepted when the `lazy` canary clears its gates.
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

### Goal 1 (density): lazy base population, gated and canaried

`ENGRAM_FC_BASE_SHM_MODE ∈ {full (default today), lazy}`, read once at
host-agent startup, rolled out in the ADR 0022 shape:

1. **`lazy`**: `prewarm_base_shm` becomes a no-op — the base file is
   created and sized by the handler as today, and every canonical page
   populates on first fault from the NVMe chunk cache. The **NVMe chunk
   prefetch + keep-set pin stays mandatory and unchanged** (ADR 0070):
   a GCS-served fault storm is the catastrophic case this substrate
   must never hit. The RAM ledger already accounts by `st_blocks`, so
   freed residency is visible to placement with no accounting change.
2. **No trace machinery on this path.** The first draft's hot-set
   prewarm — and its canonical-trace-at-bake prerequisite — is dropped.
   If the canary shows an unacceptable contended-host tail, a hot-set
   insurance mode can be revisited as a follow-up; it is insurance, not
   the mechanism.
3. **Prerequisite hardening** (bugs found during the campaign that
   `lazy` leans on):
   - The tmpfs headroom-skip is sticky: a skipped prewarm never retries
     when tmpfs frees (nothing invalidates readiness). Mode-independent
     bug; fix the retry.
   - Orphaned VMs from failed creates pin tmpfs and silently starve
     prewarm/placement (the campaign's silent-reserve-reject class);
     the teardown reconciler must reap them.
   - Keep the absence-triggered re-warm (`exists()` recheck); in `lazy`
     mode it re-creates the empty file only.
4. **Canary gates** (single kvm-pool host carrying dev-brain, ≥24 h):
   - Residency: `utilBaseShmMib` on session-less holds → expect
     ~20.7 GiB/image reclaimed. Active-session hosts are unchanged (see
     erosion above), so packing math and ADR 0046/0048 reservations
     keep budgeting active sessions at full residency.
   - Latency: p50/p95/**p99** first-prompt vs control. Healthy-host
     expectation is parity (+~2 s restore span); the p99 under two
     concurrent boots plus an in-session build is the real question
     (the +8 s / +36 s contended samples).
   - No BlobStorage round-trips on the fault path (keep-set holds).

### Goal 2 (TTFM): fix it where it lives — the guest

Base-shm policy moves this number by ±2 s. The levers that matter, as
follow-up work items in priority order:

1. **Harness spawn priority** (agentd): spawn the harness with elevated
   CPU weight (nice/cgroup) so a ~1 s-CPU cold start cuts through the
   resume stampede instead of queueing behind it. Small agentd change;
   measured headroom says storm-time first prompts drop from ~40 s to
   single digits.
2. **Verify warm-harness reattach actually fires in prod** (ADR
   0037/0062): a stamp-matched create should reattach the captured warm
   harness and skip the cold spawn; any stamp churn between capture and
   create silently forces the slow path fleet-wide.
3. **Capture-time quiesce guidance** for warm images: settle GC/timers
   before the `[warm]` capture so resume doesn't start with a
   thundering herd (image-config documentation; dev-brain first).
4. Minor: the prompt-dispatch outbox tick contributes ~2.3 s median;
   event-driven dispatch would shave it.

Debuggability rider: the ADR 0025 guest kernel compiles out PSI
(`/proc/pressure`); enable it in the next kernel rev — this
investigation had to infer guest starvation from the host side.

## Consequences

**Positive**
- ~20.7 GiB/image reclaimed on every host not actively running that
  image — directly attacks the 21.6 GiB/host ceiling and the
  1-session-per-host regression, with **no measured latency cost on
  healthy hosts** and no new machinery (the handler's lazy-populate
  path already serves everything).
- Retires the eager prewarm's ~45 s per-roll cost, the canonical-trace
  prerequisite, and the sparse draft's trace plumbing.
- TTFM work is unblocked from the memory substrate entirely and lands
  as small guest-side changes with measured 15× headroom.

**Negative / risks**
- The contended-host tail is real (+36 s measured on a RAM-starved
  box); the canary p99 gate is the decision point, and `full` remains
  one env var away per host.
- Active sessions still erode to full residency — density planning must
  not assume `lazy` frees RAM under running dev-brain sessions (the
  ledger reports truth; reservations budget worst-case).
- The NVMe keep-set becomes even more load-bearing: a swept cache under
  `lazy` means GCS-latency faults (the PR #634 incident class).

## Go / No-Go

**GO.** Implement `ENGRAM_FC_BASE_SHM_MODE=lazy` (a prewarm no-op plus
the hardening items), canary on the kvm pool, promote to default on the
gates above. The TTFM follow-ups (harness spawn priority first) proceed
independently — they are where first-message latency actually is.

---
*Measurement appendix: raw run dirs, perf data, and strace logs on the
dev VM (`~/.adr92/runs2/`, `/tmp/handler-perf.txt`,
`/tmp/handler-stracec.txt`); the four-cell TTFM matrix (Mac baseline,
quiet-guest direct, quiet-guest harness-path, storm-time) and the
per-stage A/C tables above are reproducible via `~/.adr92/measure-run2.sh`.*
