# ADR 0092: Sparse base-shm — keep only the hot working set resident, fault the cold tail from the NVMe chunk cache

- Status: **Proposed**
- Date: 2026-07-13
- Workstream: WS0 of the dev-brain campaign-remediation plan (design gate)
- Spike branch (if productionized): `ws0-sparse-spike`
- Related: ADR 0022 (runtime memory sharing / File-backend density, the rollout-gate template), ADR 0045 (unified memory substrate v2b — base-shm + UFFDIO_CONTINUE), ADR 0070 (handler is a populate-only cache citizen), the working-set trace (REAP/FaaSnap-style).

## Context

Every FC host permanently holds **every enabled image's full base memory** in a `/dev/shm` (tmpfs) per-template base-shm file. Measured in prod: **21.6 GiB/host**, unreclaimable. dev-brain's own base is ~7 GiB non-hole (guest RAM ceiling 24 GiB; the base is the warm-capture snapshot's non-zero pages). The consequence the campaign hit: **dev-brain fits 1 session/host instead of 2**, and residency grows `O(images × hosts)` with a hard 32 GiB tmpfs ceiling in sight.

Today `prewarm_base_shm` (`engram-host-agent/src/image_prefetch.rs:903`) writes **all** of a memory manifest's chunks into the base file at residency time — the full non-hole base — so the first session on a freshly-rolled host doesn't pay per-page fetch+pwrite during boot. That eager full-fill is exactly what pins 21.6 GiB.

**Hypothesis (user-locked spike):** keep only the *hot* pages resident — the working set the existing trace instrument already records in the first ~5 s after restore — and leave the cold tail as **holes** in the base-shm file, faulted lazily from the NVMe chunk cache on first touch. Expected: hot set ≪ full base, at an acceptable boot-latency cost.

## Investigation (this spike, on the GCP dev VM + code)

### Finding 1 (headline) — the cold-tail tiering the spike proposed to build **already exists in production**

The base-shm fault path in `engram-uffd-handler/src/runtime.rs` already tiers exactly the way the disk chunked-NBD daemon does. For a **Canonical** page with base-shm present (`serve_pagefault` → `install_canonical_shared` → `populate_and_continue`, runtime.rs:1092-1141, 1414-1425):

```
if !base.is_populated(byte_offset, populate_len) {          // SEEK_HOLE probe
    let bytes = self.handle.block_on(self.backend.fetch_chunk(hash))?;  // NVMe cache -> chunk store
    base.write_chunk(byte_offset, &bytes)?;                 // populate the base file in place
}
// then UFFDIO_CONTINUE the range (shared page-cache install)
```

So a **cold hole in a sparse base is already fetched from the chunk store and served transparently** — the handler reaches the chunk store today, and `is_populated` is a real `SEEK_HOLE` extent walk (`base_shm.rs:133`). The eager `prewarm_base_shm` full-fill exists **only** to keep this per-fault fetch off the boot path; nothing else depends on the base being fully resident.

**Therefore sparse residency is NOT a handler change and NOT architecturally blocked.** It is a filter in `prewarm_base_shm`: write only the trace's hot-set chunks, leave the rest as holes; the handler's existing lazy-populate path backstops every cold fault. The prototype is **~10-30 lines**, far under the spike's ~400-line / "handler can't reach the chunk store" abort threshold.

### Finding 2 — the RAM ledger already charges **actual allocated bytes**, so sparse footprint is reflected for free

`ram_ledger.rs` sums `st_blocks * 512` (NOT `st_size`) for base-shm files (`base_shm_used_mib`, and the doc at line 75 is explicit), and `pending_unwritten_bytes` nets each pending charge against the target file's real `file_allocated_bytes`. A sparse (holey) base file reports smaller `st_blocks`, so:

- Steady-state residency charge shrinks automatically to the resident (hot + lazily-faulted) bytes.
- As the handler faults cold pages and `write_chunk`s them, `st_blocks` grows and the ledger tracks it — no drift.

The only tweak needed: `register_pending_base_shm` currently registers `pending = manifest_non_hole_bytes` (the full base). In sparse mode it should register the **hot-set** byte estimate so the transient warm-window reservation isn't a full-base over-charge. That's a 1-line change at the call site (`image_prefetch.rs:788,814`). The "charge actual allocated bytes" work item the plan anticipated is, in substance, **already done**.

### Finding 3 (measured, dev VM) — the base file is sparse-by-construction; only faults populate it

Ran the existing `substrate_uffd_base` FC integration test (stock artifacts, forked FC) with a `du`/`du --apparent-size` probe over the base file:

- Handler created and sized the base file to the manifest: `substrate base shm ready ... total_bytes=134217728` (128 MiB), then `listening`.
- du-probe at the moment of load: **`resident_kb=0  apparent_kb=131072`** — the base is a **pure-hole 128 MiB file** with zero allocated blocks until a fault writes into it.

This directly confirms the residency model: the base file's allocated bytes are 0 at creation and only grow by `write_chunk` on fault. Full residency today is an artifact of `prewarm_base_shm` pre-filling; remove the pre-fill for cold chunks and residency = what actually faults.

### Blocker (measurement, not design) — no substrate-capable FC fork on the dev VM

The base-shm substrate is a **fork-only** FC load param (`uffd_base_file`). The only fork binary on the VM is **v1.10.1** (`~/fc-fork-path/firecracker`), which predates the substrate work and rejects the field:

```
PUT /snapshot/load -> 400: unknown field `uffd_base_file`,
  expected one of `snapshot_path`, `mem_file_path`, `mem_backend`,
  `enable_diff_snapshots`, `resume_vm`, `shared`
```

The substrate fork is v1.16-based (ADR 0045 Phase B) and is **not present** on this box; building it from source was out of scope for the spike (disk was at 95%, and the VM's working tree carried a large unrelated in-flight diff from a concurrent session — not safe to disturb). **Consequence:** the on-VM sparse-vs-full **boot-latency distribution, resident-bytes delta, and cold-fault tail under concurrency could not be measured here.** These require either (a) a v1.16 substrate fork built on the dev VM, or (b) — preferably — the prod canary, which is the rollout gate anyway (see below). Note also that dev-brain itself is too heavy for the 16 GiB VM, so even with the fork the representative hot/full ratio is a prod-canary measurement, not a dev-VM one — this was anticipated by the plan.

### Residency estimate from trace math (what we can quantify without the fork)

- **Full base (what we hold today):** `manifest_non_hole_bytes` = Σ chunk lengths ≈ **7 GiB** for dev-brain (per the campaign's own measurement; 21.6 GiB/host across images).
- **Sparse resident floor (boot):** `trace.chunks.len() × chunk_size` (`WorkingSetTrace::approx_bytes`, chunk_size 512 KiB). The trace is the ordered hot set faulted in the first ~5 s. Working-set literature the trace design cites (REAP, FaaSnap) puts the post-restore hot set at roughly **10-30 %** of a snapshot for service-style images; for a JVM/Gradle image like dev-brain the boot hot set is dominated by a small resident code+heap-touch region, not the full committed heap.
- **Sparse resident ceiling (steady state):** the **union over all co-resident sessions** of the canonical pages each session read-or-write-faults at least once (divergent pages go PRIVATE via COPY and never enter the shared base). This is the honest number to watch, and it is `> trace size` and grows with session lifetime.

So the design saves `full_non_hole − union_of_lifetime_working_sets`. For short-lived and lightly-touching sessions the win is large; for a long dev-brain session that eventually touches most of its 7 GiB the win erodes. **This ratio is precisely what the canary must measure** (per-host trace/prefault-stats vs. steady-state base-shm `st_blocks`), because it is workload-dependent and cannot be faithfully reproduced on a busybox-sized dev-VM VM (which has almost no cold tail — its hot set ≈ its whole non-hole base, which would *understate* the win and mislead).

## Decision

Adopt **sparse base-shm** behind an env gate, defaulting off, promoted by prod canary — the ADR 0022 pattern.

1. **`prewarm_base_shm` gains a hot-set filter.** When `ENGRAM_FC_BASE_SHM_MODE=sparse`, load the canonical working-set trace for the image (`TraceRef::canonical(manifest_id)`, else the per-host trace) and write **only** the chunks whose hash is in the trace; skip the rest (they stay holes). When `full` (default) or no trace exists, behave exactly as today (full fill) — a missing canonical trace is a safe fallback to full, never a broken boot.
2. **Handler is unchanged** — its lazy `populate_and_continue` already serves every cold hole from the NVMe chunk cache. This is the crux: no new handler code, no new failure mode.
3. **Ledger:** register the hot-set byte estimate as the pending charge in sparse mode (1-line); everything else (steady-state `st_blocks` accounting) is already correct.

### Rollout gate (mirrors ADR 0022's `ENGRAM_FC_BASE_RESTORE_MODE=file` canary)

- `ENGRAM_FC_BASE_SHM_MODE` ∈ `{full (default), sparse}`, read once at host-agent startup (same read-once discipline as ADR 0055's `current.json`).
- Canary: enable `sparse` on a single kvm-pool host carrying dev-brain. Gate metrics for promotion:
  - **Residency:** `utilBaseShmMib` / base-shm `st_blocks` per host — the primary win. Compare canary vs. control over ≥24 h of real sessions.
  - **Boot cost:** `engram_resume_prefault_*` + restore-span latency — the cost. Cold-fault fetch+pwrite must stay off the critical path enough that p50/p95 first-prompt latency is within budget.
  - **Cold-fault behavior under concurrency:** the NVMe chunk cache must be warm (pinned) for the cold tail; verify no BlobStorage round-trips on fault (ADR 0070 keep-set already protects these pins).
- Promote to default only after the canary shows residency win with acceptable latency, exactly as ADR 0022 gated File-backend density on its prod canary.

## Consequences

**Positive**
- Directly attacks the 21.6 GiB/host ceiling with a **~10-30 line, handler-untouched** change riding an already-shipped, already-exercised lazy-populate path — very low risk surface.
- Ledger already reflects the smaller footprint; capacity/autoscaler (WS1) sees the freed RAM for free.
- Reversible per-host via one env var; `full` remains the safe default and the trace-missing fallback.

**Negative / risks**
- **Steady-state erosion:** the shared base re-grows toward full as sessions touch canonical pages. A long, memory-touching dev-brain session may reclaim little. The win is real for the cold tail *no co-resident session ever touches* (guard pages, unused heap, cold code of other in-image services) and for churny/short sessions — but it is workload-dependent and must be proven by the canary, not assumed.
- **Cold-fault latency tail:** first touch of a cold page now pays fetch(NVMe)+pwrite+CONTINUE instead of CONTINUE-only. Bounded by NVMe locality (chunks are pinned/warm), but a cold-storm (many cold pages faulted at once under 2 concurrent boots) could add a tail. **Unmeasured on the dev VM** (fork blocker) — canary must watch p99 first-prompt latency.
- **Canonical trace dependency:** sparse is only as good as the trace. If no `canonical.json` exists for an image (bake-time trace) the host-local trace is used, and a cold host with neither falls back to full — correct but no win until a trace is recorded. **Open question flagged by the plan:** confirm whether `engram-rootfs-materializer` actually publishes `canonical.json` for our base images or only per-host traces exist — if canonical is absent, add its bake-time capture as a prerequisite of sparse rollout.

**Measurement debt to close on the canary (could not be done on the dev VM)**
1. Real hot/full ratio: per-host trace bytes vs. `manifest_non_hole_bytes` for dev-brain.
2. Steady-state resident base `st_blocks` sparse vs. full over a real session lifetime.
3. Boot-latency distribution (N≥5) sparse vs. full, and cold-fault p99 under 2 concurrent boots.

## Go / No-Go

**GO on feasibility (conditional GO on rollout).** The single biggest de-risking finding is that the hard part — a memory-leg cold-tail tiering that reaches the chunk store — **already exists and runs in prod**; sparse residency is a small prewarm-filter + a 1-line ledger tweak, and the ledger already accounts by real allocation. Proceed to implement behind `ENGRAM_FC_BASE_SHM_MODE=sparse` and **gate promotion on the prod canary numbers** (residency win vs. latency cost), since the representative A/B is inherently a prod-workload measurement (dev-brain is too heavy for the dev VM, and the VM lacks a substrate-capable FC fork). Do **not** default it on until the canary clears the gate. If the canary shows steady-state residency erodes to near-full for real dev-brain sessions, this becomes a No-Go in favor of the (rejected) reservation-driven reclaim or a memory-ballooning approach.

---
*Raw measurement logs: `scratchpad/ws0-logs/vm-raw-measurements.txt` (build, substrate test run, du-probe).*
