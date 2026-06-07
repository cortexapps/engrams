# ADR 0043: Substrate evolution — immediate-resume memory, retire evac for drain-first, post-copy-with-durable-fallback

Status: 2026-06-07 — **Proposed.** A phased roadmap. The no-fork phases (0–3) are committed and execute now, one PR each; the Firecracker-fork phases (4–6) are a gated decision revisited after 0–3 land + prove out in prod. Flips to **Accepted** as the phases ship (commit chain appended).

## Context

A run of prod incidents — disk-flush chunk-drop (#109), single-flight poison-on-cancel (#111), the NBD device-open hang, the evac/resume hangs and deploy storms we chased live — showed that almost our entire substrate bug surface clusters in one layer: **chunk guest memory+disk into content-addressed objects in GCS and reconstruct per host.** That layer exists to make a snapshot *portable*. Its worst symptom is that **we ship gigabytes of guest RAM to/from GCS on the resume hot path**, which dominates cold-resume latency; its worst *bugs* came from the periodic-checkpoint + idle-evict + **session-evac** durability machinery layered on top.

ADR 0042 surveyed the field (E2B read from source; Replit/CodeSandbox/Modal + Drafter/silo via two adversarially-verified research passes) and a code survey mapped our gaps. Two findings frame this ADR:

- **We are ~60–70% of the way to the target memory model already** — working-set prefetch (REAP-style), a shared content-addressed chunk store for disk+memory, NVMe-warm-primary reads, and diff-checkpoints all exist and work in prod. The remaining memory gap is narrow: **we block resume on prefetch.**
- **Fast cross-host migration is not a storage-tier difference; it's post-copy.** Drafter/silo resume the destination immediately and demand-fault the tail (hot delta P2P from the source, cold blocks pulled from object storage in parallel) — so the object-store transfer is *off the downtime-critical path*, overlapped with a live VM. Engrams is slow because the same GCS reconstruct is *on* the resume critical path. The fix is not "stop using cold storage"; it's "stop blocking resume on it."

## Decision

Evolve the substrate toward a "golden state" in phases, ordered so the highest-leverage, no-fork wins ship first and the Firecracker fork is a gated decision, not a prerequisite.

**Golden state:**
- **Memory:** local-NVMe warm primary; resume the VM *immediately* and prefetch the working set in the background; GCS as a durable backstop off the hot path; (fork) `MAP_SHARED` continuous dirty-page flush so snapshot-save is off the pause path.
- **Cross-host move without dropping:** (fork) **post-copy** — resume on the destination, demand-fault hot pages P2P from the source, pull cold blocks from GCS in parallel, **with the GCS snapshot as the crash-safe fallback** (the intersection no incumbent occupies: silo/Drafter do post-copy *without* a durable fallback; the field does snapshot-and-rehome *without* post-copy — our durable store gives us both).
- **Deploys:** drain-first / version-pinned / VM-detached; relaxed (explicit/event-driven) checkpointing instead of a timer.

**Key sub-decisions:**

1. **Gate the Firecracker fork.** Ship the no-fork phases (immediate-resume, fail-fast, relaxed checkpointing, drain-first deploys, evac deletion) first. Commit to forking FC — which is the *only* way to get MAP_SHARED off-pause flush and post-copy — once those land. We already own the guest kernel (ADR 0025), so the VMM build pipeline is a smaller step than for most shops; keep the fork surface minimal (memory backend + snapshot path), track upstream v1.10.x otherwise, attempt to upstream the generally-useful MAP_SHARED dirty-tracking.

2. **Keep the FC restore-mode split as-is.** `File` for fast initial boots (chunks resident on the FC host, including after moved tags); `Uffd` for resumes. This is intentional and correct — the only memory-resume change is making prefetch non-blocking on the UFFD path.

3. **Fully retire session evac.** The `evac_resumer` / `evacuate_dead_source` / `Evacuating`-driven re-home is the source of the hardest, weirdest bugs (the single-threaded scanner starving on an unbounded dead-host RPC; the resume-from-idle wedge; the deploy-storm cascade). **Delete it.** Replace planned host-removal with drain-first deploys (Phase 3), and unplanned host loss with lazy-resume-from-the-last-durable-snapshot on next access. Keep cordon/uncordon as an admin tool (the drain-prep primitive).

4. **Reuse, don't rebuild.** The working-set recorder/trace, the shared `ChunkStore`/`ChunkCache`/manifest, the diff-checkpoint, and the warm-NVMe-primary read path all stay. Full disk+memory device-`Provider` unification (silo's lesson) is *not* adopted — our storage substrate is already shared, and memory has no live write/dirty primitive that would make a unified `WriteAt` device pay off.

## Phases

Detailed seams + validation live in the execution plan; the decision-relevant shape:

| Phase | What | Fork? | Ships |
|---|---|---|---|
| **0** | Fail-fast: NBD device-open `NBD_SET_TIMEOUT`/EIO; deadline on the resume restore-RPC | no | now |
| **1** | Resume immediately: make UFFD-resume memory prefetch non-blocking (host-agent + handler) — **highest leverage**, and the precondition for post-copy | no | now |
| **2a** | Relax periodic-checkpoint cadence (event-driven; keep checkpoint on drain + idle-evict) | no | now |
| **3** | Drain-first deploys (coord-gated) → **retire evac** (delete the code) → stable host-id + VM-detach (`kill_on_drop` removal) → version-pinning | no | now |
| **4** | **Firecracker fork decision gate** (recommend fork; minimal surface) | — | gate |
| **5** | Post-copy migration + GCS durable fallback — the differentiator | yes | gated |
| **6** | `MAP_SHARED` continuous flush, wired into the disk flush-scheduler interface | yes | gated |

Dependencies: Phase 1 is the architectural precondition for Phase 5 (post-copy = "resume immediately, fault from elsewhere"; the source becomes another producer at the one `fetch_chunk` seam). Phase 3 builds drain-first *before* deleting evac. Phase 4 gates 5 and 6.

## Alternatives considered (and rejected)

- **Put guest memory on a persistent network disk** (Hyperdisk/NVMe-oF) as the per-fault source. Rejected as the *hot-path* source: Hyperdisk random-4 KB latency is ~ms (vs local NVMe ~μs), so naive faulting is untenable without prefetch; per-instance attach limits cap density. It remains viable as a *durable backstop / reattach-on-host-loss* tier, but that's not better than the GCS chunk store we already have. (ADR 0042 evidence; FluidMem feasibility, REAP prefetch necessity.)
- **Unify disk+memory behind one `Provider`/`ReadAt`/`WriteAt` device** (silo's model). Rejected as low-value: our storage substrate is already shared; memory has no live write/dirty primitive, so the device layers differ for good reasons.
- **Keep session evac** (just harden it with deadlines + concurrency). Rejected: the machinery is a recurring bug source and its job is better served by drain-first (planned) + lazy-resume (unplanned). Deletion removes more risk than it adds.
- **Fork Firecracker now** as the foundation. Rejected for sequencing: the no-fork phases deliver the bulk of the value and de-risk the fork (Phase 1 turns post-copy from a re-architecture into a one-seam producer swap). Fork once they prove out.
- **Far / disaggregated memory** (RDMA paging, CXL.mem): research-grade, needs fabric GCP doesn't standardly offer — parked.

## Invariants / tradeoffs (the landmines)

- **Evac-retirement crash window.** With evac gone *and* checkpointing relaxed, an *unplanned* host crash recovers from the last checkpoint on next access, not a proactive re-home. Keep checkpoints on drain + idle-evict + a sane ceiling so crash-loss stays bounded. Most host removals are *planned* (deploys), which drain-first handles cleanly; Phase 5 later makes planned moves live. This is a deliberate move toward E2B's "explicit-durability" posture for the unplanned case — accepted because the evac machinery cost more than the durability it bought.
- **`kill_on_drop` removal (VM-detach).** Trades a clean-shutdown guarantee for an orphan-reap obligation; host identity must be pinned to the instance first (per-process `HOST_ID` breaks pidfd-reattach), and the reattach taxonomy exhaustively tested, before the SIGKILL backstop is dropped.
- **Post-copy source-death race (Phase 5).** The window where the destination is live but still faulting from a source that dies is the hard case — and exactly where our durable GCS snapshot is the fallback silo/Drafter lack. Make GCS-fallback the *tested default* of the fault path, not an afterthought.
- **AGPL.** silo/Drafter are AGPL — design reference to *reimplement* (`AlternateSource` + `WriteCombinator` post-copy-with-parallel-cold-pull), never import.

## Status

Proposed. No-fork phases (0–3) execute now (one PR each); the fork gate (4) and fork-only phases (5–6) are revisited after 0–3 prove out in prod. Builds on ADR 0042 (the prior-art survey + evidence).
