# ADR 0043: Substrate evolution — fail-fast, immediate-resume memory, relaxed checkpointing

Status: 2026-06-08 — **Accepted.** The no-fork substrate hardening shipped: P0 fail-fast (#115), P1 immediate-resume / non-blocking UFFD prefetch (#116), P2a relaxed checkpoint cadence (#118). The Phase 3 deploy/detach work was delivered by **ADR 0044** (K2 VM-detach; K3 drain-gated operator + version-pin). The evac retirement (originally Phase 3b) and the fork-gated golden state (immediate-resume's endgame — post-copy live migration + `MAP_SHARED` off-pause flush) moved to **ADR 0045**.

## Context

A run of prod incidents — disk-flush chunk-drop (#109), single-flight poison-on-cancel (#111), the NBD device-open hang, the evac/resume hangs and deploy storms we chased live — showed that almost our entire substrate bug surface clusters in one layer: **chunk guest memory+disk into content-addressed objects in GCS and reconstruct per host.** That layer exists to make a snapshot *portable*. Its worst symptom is that **we ship gigabytes of guest RAM to/from GCS on the resume hot path**, which dominates cold-resume latency; its worst *bugs* came from the periodic-checkpoint + idle-evict + session-evac durability machinery layered on top.

ADR 0042 surveyed the field (E2B read from source; Replit/CodeSandbox/Modal + Drafter/silo via two adversarially-verified research passes) and a code survey mapped our gaps. Two findings framed this ADR:

- **We are ~60–70% of the way to the target memory model already** — working-set prefetch (REAP-style), a shared content-addressed chunk store for disk+memory, NVMe-warm-primary reads, and diff-checkpoints all exist and work in prod. The remaining memory gap was narrow: **we blocked resume on prefetch** — closed in Phase 1.
- **Fast cross-host migration is not a storage-tier difference; it's post-copy** — resume the destination immediately and demand-fault the tail, with the GCS reconstruct off the downtime-critical path. That endgame (post-copy + a durable fallback) is the subject of **ADR 0045**; this ADR shipped its architectural precondition (non-blocking resume), so post-copy becomes a producer swap at one seam rather than a re-architecture.

## Decision

Evolve the substrate toward the "golden state" in phases, ordered so the highest-leverage, no-fork wins ship first. **This ADR covers the no-fork phases (0–3); the Firecracker-fork golden state moved to ADR 0045.**

**Key sub-decisions (no-fork):**

1. **Keep the FC restore-mode split as-is.** `File` for fast initial boots (chunks resident on the FC host, including after moved tags); `Uffd` for resumes. This is intentional and correct — the only memory-resume change is making prefetch non-blocking on the UFFD path (Phase 1). (ADR 0045 Phase D builds on this split: `MAP_SHARED` continuous flush is a *write*-path change on the live memfile and must not leak into this restore *read* path.)

2. **Reuse, don't rebuild.** The working-set recorder/trace, the shared `ChunkStore`/`ChunkCache`/manifest, the diff-checkpoint, and the warm-NVMe-primary read path all stay. Full disk+memory device-`Provider` unification (silo's lesson) is *not* adopted — our storage substrate is already shared, and memory has no live write/dirty primitive that would make a unified `WriteAt` device pay off.

The fork gate, post-copy migration, and `MAP_SHARED` sub-decisions moved to **ADR 0045**. The evac retirement (originally Phase 3b — "fully retire session evac") also moved to **ADR 0045 Phase A**, where it is reconciled with reality: ADR 0044 K3 drain now *depends* on the `Evacuating` machinery, so 0045 retires the *reactive* evac (dead-host auto-evac + NBD-loss triggers, the documented bug source) while keeping the *drain-driven* evac.

## Phases

| Phase | What | Fork? | Status |
|---|---|---|---|
| **0** | Fail-fast: NBD device-open `NBD_SET_TIMEOUT`/EIO; deadline on the resume restore-RPC | no | shipped #115 |
| **1** | Resume immediately: make UFFD-resume memory prefetch non-blocking (host-agent + handler) — **highest leverage**, the precondition for post-copy | no | shipped #116 |
| **2a** | Relax periodic-checkpoint cadence (event-driven; keep checkpoint on drain + idle-evict) | no | shipped #118 |
| **3** | Drain-first deploys → retire evac → stable host-id + VM-detach → version-pinning | no | delivered via ADR 0044 + 0045 (below) |

**Phase 3 — delivered by ADR 0044 + ADR 0045:**
- **3a (drain-first)** + **3d (version-pin)** → shipped as ADR 0044 **K3**: the drain-gated, digest-pinned, node-by-node operator rollout (MIG `max_surge` mechanics reborn as operator logic).
- **3c (VM-detach / `kill_on_drop` removal)** → shipped as ADR 0044 **K2**: detach + pidfd-reattach + stable HostId (more load-bearing on K8s, where pod restarts are routine).
- **3b (retire evac)** → moved to **ADR 0045 Phase A**, reconciled to "retire the *reactive* evac, keep the *drain-driven* evac" (see Decision above).

## Alternatives considered (and rejected)

- **Put guest memory on a persistent network disk** (Hyperdisk/NVMe-oF) as the per-fault source. Rejected as the *hot-path* source: Hyperdisk random-4 KB latency is ~ms (vs local NVMe ~μs), so naive faulting is untenable without prefetch; per-instance attach limits cap density. It remains viable as a *durable backstop / reattach-on-host-loss* tier, but that's not better than the GCS chunk store we already have.
- **Unify disk+memory behind one `Provider`/`ReadAt`/`WriteAt` device** (silo's model). Rejected as low-value: our storage substrate is already shared; memory has no live write/dirty primitive, so the device layers differ for good reasons.
- **Keep session evac** (just harden it with deadlines + concurrency). Superseded by the **ADR 0045 Phase A** reconciliation: retire the reactive triggers (the recurring bug source), keep the drain-driven path (which ADR 0044 K3 depends on).

The "fork Firecracker now" and "far / disaggregated memory" alternatives are decided/parked in **ADR 0045**.

## Invariants / tradeoffs (the landmines)

- **Evac-retirement crash window.** With reactive evac gone *and* checkpointing relaxed, an *unplanned* host crash recovers from the last checkpoint on next access (lazy resume), not a proactive re-home. Keep checkpoints on drain + idle-evict + a sane ceiling so crash-loss stays bounded. Most host removals are *planned* (deploys), which drain-first (ADR 0044 K3) handles cleanly. The detailed reconciliation + the lazy-resume routing live in **ADR 0045 Phase A**.
- **`kill_on_drop` removal (VM-detach).** Trades a clean-shutdown guarantee for an orphan-reap obligation; host identity must be pinned to the instance first, and the reattach taxonomy exhaustively tested, before the SIGKILL backstop is dropped. Delivered in **ADR 0044 K2**.

## Status

**Accepted.** No-fork phases shipped: **P0** fail-fast (#115), **P1** immediate-resume / non-blocking UFFD prefetch (#116), **P2a** relaxed checkpoint cadence (#118). Phase 3's deploy/detach halves shipped via **ADR 0044** (K2 VM-detach, K3 drain-gated operator + version-pin). Phase 3b (retire evac) and the fork-gated golden state (post-copy live migration + `MAP_SHARED` off-pause flush, plus the true autoscale-down it unlocks) moved to **ADR 0045**. Builds on **ADR 0042** (the prior-art survey + evidence).
