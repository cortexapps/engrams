# 0078 — GCS-free resume: authoritative host-affinity + peer-first divergence fill

Status: Proposed (2026-07-06)

Issue: #548 (2026-07 core-ops overhaul, Tier 2). Depends on #528
(chunk-cache-disk-budget — merged #557; phase 2 imports its disk ceiling
constant, does not mint a second) and #529 (host-durable eviction
finalize — merged #558; phase 4 hooks its heartbeat row-landing
reconcile, since the old `idle_evictor` finalize task is gone). Rides
#547 (substrate single-writer populate seam — the on-demand peer tier is
added in exactly one populate owner). Absorbs #545 **rung 3
(parked-local)**: rung 3's "destroy the VM but keep its divergence on
local NVMe for a host-pinned resume" IS this epic's authoritative
affinity + local-first fill, so it lands here reusing `park_rung=3`
rather than as separate machinery in the parking ladder.

## Problem

Resuming an idle session should be a local-NVMe operation — the
substrate was built so guest state = shared immutable base + a private
divergence overlay, with GCS as durability/transport only, never
latency-bearing. In prod today GCS sits on the resume path ~100% of the
time: every resume that lands off a warm cache pays ~190 ms per 16 MiB
chunk of GCS RTT across the session's divergent set, which is exactly
where the `agent_handshake` tail lives (p50 1.34 s but p90 37.1 s, p95
60.2 s, max 299 s; 26% of handshakes >10 s, n=66, 7 d @ 2026-07-01). The
92 s cold-recovery resume (session ca82a6f1, 2026-06-25) is the
canonical member.

Three mechanisms all fail toward GCS:

1. **Host-affinity is a soft preference that silently degrades.** The
   tier meant to make it strong — `local_snapshots` snapshot-affinity —
   is **dead wire**: the host-agent has never populated it
   (`local_snapshots: Vec::new()` hardcoded in the heartbeat loop), so
   placement's tier-1 match can never fire, and the tier-2 `prefer_host`
   fallback to "any host" is silent (no metric, no event).
2. **Peer-to-peer chunk pull is teleport-only.** `MigrationFetch` +
   hot-first 8-stream source pull exist and are prod-proven, but are
   wired exclusively into the live-migration export path. An ordinary
   cross-host resume reconstructs everything from GCS even when the
   previous host is alive with a fully warm cache one LAN hop away.
3. **Two write-through holes on the flush path** mean even the
   *capturing* host's cache isn't guaranteed to hold what it just
   uploaded, and every flush pays one GCS HEAD per dirty chunk for
   nothing.

**Fleet-stability precondition (bake in, do not soften):** the headline
"the 26%-of-handshakes-\>10 s class collapses" is a property of a
*post-#528 stable fleet*. At evidence time the fleet was 1–2 live hosts
with node churn discarding NVMe caches; on such a fleet there is
frequently no live peer, and peer-first fill must degrade to GCS
gracefully — one bounded dial, never a per-chunk retry ladder. A missing
peer must cost ~0, not a latency cliff. Never quote the impact numbers
without this precondition.

## Root cause

The substrate's locality invariant ("the bytes you need are on the NVMe
next to you") was implemented as a stack of *soft preferences and
per-path optimizations* instead of a placement + transport contract.
Placement prefers the warm host but silently falls back; the heartbeat
field that would let it prefer accurately was scaffolded and never wired
to a producer. When the fallback happens nothing moves the divergence to
the destination, because the peer transport that solves exactly this was
scoped to a live-migration export. And the write-through/HEAD defects
mean even "same host, warm cache" leaks.

## Decision — the locality invariant becomes a contract

**A resume's divergent chunk set is served from NVMe (local, or one live
peer hop) whenever any live host holds it; GCS is reached only when no
live host does.** Five moves, leverage order:

1. **Authoritative host-affinity with observed fallback** (coordinator).
   Retire the never-populated `local_snapshots` mirror end-to-end (wire,
   `HostRecord`/`HostHeartbeat`, PG column, rank tier). `ScheduleContext`
   drops `prefer_snapshot_id`, gains `snapshot_host: Option<HostId>`
   filled from `SnapshotRecord::host_id` — the ONE owner of the fact
   (PG). `pick_from` tier 0: if `snapshot_host` is Some, schedulable, has
   disk headroom under #528's ceiling constant, and fits RAM/CPU → pick
   it, always. Every veto increments
   `engram_resume_affinity_fallback_total{reason}` and writes a session
   event — the fallback is never again silent. Feed the resume path real
   RAM/CPU budgets (today it builds the context capacity-blind).

2. **Peer-first divergence fill for ordinary cross-host resume**
   (coordinator + host-agent; generalizes ADR 0045 C1). A new
   coordinator-brokered **resume export**: when placement lands a resume
   on `dest != snapshot_host` and the source is alive, the coordinator
   calls `OpenResumeExport { snapshot_id, disk_manifest, memory_manifest }`
   on the source, which registers a **sandbox-less** export (unguessable
   `export_id` + a manifest-scoped allowlist; `migration_fetch` serves
   `Chunk` items only, rejects `StateBin`/`DiskChunkAt`; the existing TTL
   sweep expires it). `SnapshotMetadata` gains `peer_fill:
   Option<PeerFillSource { source_addr, export_id, hot_chunks }>`. The
   destination (a) spawns the existing `pull_chunks_from_source` over the
   divergent set, hot-first, landing via budget-respecting `cache.put`
   (not `put_no_evict` — a resume pull must not blow the disk budget),
   and (b) adds a peer tier to the single-writer populate miss chain:
   **local NVMe → peer → GCS**, with a **one-dial degradation contract**
   (2 s connect timeout; any failure marks the peer lost for the session
   and everything thereafter goes straight to GCS). Source dead / RPC
   error / Unsupported (VZ, Process) / export expired ⇒ `peer_fill: None`
   ⇒ today's GCS path, unchanged and counted
   (`engram_resume_divergence_source_total{tier=local|peer|gcs}`).

3. **Evict-time prestage off a draining host** (coordinator + host).
   After the snapshot row lands (via #529's reconcile), if the capturing
   host is **cordoned**, pick a prestage target (`pick_from`,
   `exclude_host = source`), open a resume export on the source,
   `PrestageChunks` to the target (hot-first `pull_chunks_from_source`),
   and on completion `set_snapshot_host(snapshot_id, target)` so move 1's
   authority lands the future resume where the bytes now are. Strictly
   best-effort; any failure leaves `snapshots.host_id` on the draining
   host and the resume falls back per moves 1+2.

4. **Close the `exists()` write-through hole** (`engram-chunk-store`).
   `put_chunk`'s idempotency early-return runs the `write_local` populate
   before returning, so a chunk remotely present but locally evicted is
   re-warmed by a put. The producing host's cache warms on every put.

5. **Drop the per-dirty-chunk GCS HEAD on flush paths.** Add
   `put_chunk_unchecked(body)` — unconditional PUT (content-addressed
   PUTs are idempotent; re-chunked dirty data is new by construction) +
   unconditional `write_local` — and switch the NBD disk flush + teleport
   catch-up to it. `put_chunk`'s dedup HEAD **stays** for
   capture/enable-scale uploads (there it saves re-uploading tens of GiB;
   this is why the fix is a second entry point, not a behavior change).

## Rung 3 (parked-local) via this epic

The parking ladder's rung 3 = "VM destroyed, but the divergence stays on
the parking host's NVMe for a ~0.5–2 s host-pinned resume." That is
precisely move 1 (authority = the host that holds the bytes) + move 2's
local tier with no peer hop needed. Concretely: a rung-3 park keeps
`snapshots.host_id` = the parking host (already true after capture) and
does NOT evict its divergent chunks from the local cache (a per-session
cache pin released on descent to rung 4 or on resume). No new transport —
resume tier 0 places back on the parking host and the divergent set is a
pure local-NVMe read. Rung 3 lands as a follow-on move here once moves
1+2 are in; `park_rung=3` is its marker.

## Trust model (resume export)

Same posture as ADR 0045 C1 teleport exports: the `export_id` is an
unguessable random token minted per resume; a fetch is authorized ONLY
for `(export_id, chunk_hash ∈ allowlist)` where the allowlist is the
union of the disk + memory manifests' chunk hashes; anything else (a hash
outside the manifests, a non-`Chunk` item kind, a bad/expired
`export_id`) is rejected. No sandbox, no seal, no paused FC — a resume
export is read-only over already-durable chunks. TTL: touch-on-serve,
~10 min idle expiry, coordinator best-effort `CloseResumeExport` after
the resume completes.

## Non-goals (pinned)

Content-defined chunking as chunk identity (deferred — little over the
reproducible bakes already invested); CI-time chunk publish + graded
readiness (#538 / #546); warm-VM pre-spawn (deferred; not referenced);
the ADR-0022 File-mode path (deleted by #530/generation-purge, not
touched here). The teleport path's `MigrationSourceInfo` / seal /
critical-section semantics are **reused, not modified** — move 2 calls
`pull_chunks_from_source` / `migration_fetch`, it does not change their
teleport contracts.

## Phase plan

- **Phase 1 — write-through floor** (moves 4+5; `engram-chunk-store` +
  the two flush call sites). Fully mechanical, backend-agnostic, no wire
  change. One PR.
- **Phase 2 — authoritative affinity + retire `local_snapshots`** (move
  1; `placement.rs` + the mirror deletion end-to-end + migration 0082 +
  WIRE 10). One PR.
- **Phase 3 — peer-first fill** (move 2; host-agent resume export +
  `OpenResumeExport`/`CloseResumeExport` RPCs + `SnapshotMetadata.peer_fill`
  + dest bulk pull + on-demand peer tier). Rides #547's populate seam.
  One PR.
- **Phase 4 — evict-time prestage on drain** (move 3; `PrestageChunks` +
  `set_snapshot_host` + the #529 reconcile hook) and **rung-3
  parked-local** (the cache-pin + host-pinned resume). One PR.
- **Phase 5 — flip to Accepted** with the commit chain; re-measure the
  §Evidence baselines on a post-#528 fleet and record the deltas with the
  fleet-stability precondition. No unconditional "collapses to X" claim.

## Divergence log

- **Phase 1 (write-through floor) — landed** (`be8f4161`, PR #586): moves
  4+5. `divergence_source{tier}` deferred to phase 3 (where local|peer|gcs
  are all real at the resume miss-chain, not redundant on the cache path).
- **Phase 2 (authoritative affinity + `local_snapshots` retirement) —
  landed.** Move 1. Divergences from the plan:
  - **Tier 0 lives in `pick_from`, not `rank_hosts`.** The plan put the
    affinity prefix in `rank_hosts`, but the authoritative check needs the
    reserved-budget map (for the RAM/CPU + disk-headroom veto), which only
    `pick_from` has. `rank_hosts` now just yields the schedulable set;
    `pick_from` runs the tier-0 `snapshot_host_veto` before the soft
    tiers. `RankedCandidates::affinity_len` is retained (always 0) — it is
    the *create*-path `reserve_placement` capacity-blind-prefix input, an
    orthogonal mechanism that was already 0 in prod (it also keyed on the
    dead mirror); fully removing it is a `reserve_placement` refactor left
    out of this move's scope (documented, not inert-by-accident).
  - **`disk_full` const owner moved to `engram-core`.** The plan said
    "import #528's constant." #528's `DEFAULT_DISK_FLOOR_BYTES` lived in
    `engram-host-agent` (not coordinator-reachable), so the canonical
    `HOST_DISK_CACHE_FLOOR_BYTES`/`_MIB` now lives in
    `engram_core::types::host` and the host-agent floor *aliases* it —
    one owner, no second threshold.
  - **`cpu_full` reason added** alongside the plan's
    `missing|dead|cordoned|wire_skew|disk_full|ram_full` (a snapshot-host
    that fails the CPU budget is a distinct, honest veto).
  - **Resume budget is best-effort.** The plan said "pass the session's
    mem/cpu budgets." The resume path resolves them via
    `resolve_cold_boot_spec` (best-effort — `None` on an un-enabled image,
    preserving the pre-0072 soft posture); the tier-0 *disk* veto (the
    primary locality signal) fires regardless.
  - **`local_snapshots` retired end-to-end**: `HostRecord`/`HostHeartbeat`
    fields, `HostLocalSnapshot`/`LocalSnapshotReport` types, the
    `Heartbeat` wire field, the coord HTTP DTO + PG upsert/select + row
    decode, the fleet-view proto count (`reserved 7`, satisfies buf
    `FIELD_NO_DELETE`) + api DTO + CLI display, and the
    `hosts.local_snapshots` column (migration 0082). WIRE 9→10.

## Degradation & reliability floor

Every new mechanism (export open, bulk pull, on-demand peer tier,
prestage) is best-effort with an explicit GCS fallback and a counter. A
peer failure must never fail or slow a resume beyond the single bounded
2 s dial. No "skip if it looks empty" shortcut on the divergent-set
computation. VZ/Process backends return a typed
`Unsupported`-class error from the new RPCs (never silent `Ok`); the
coordinator counts `reason="unsupported_backend"` and proceeds GCS-bound.

## Numbers discipline

Every baseline quoted from the issue is 2026-07-01, 7–14 d-window
evidence; "LAN beats GCS ~20×" is a code comment, not an independent
measurement. Re-measure at the Phase-5 flip; never present the peer-fill
wins without the fleet-stability precondition.

### Phase-2 review note: affinity asserts locality it cannot verify

`snapshots.host_id` is a capture-time fact with no possession freshness —
the tier-0 pin has no way to know the host's NVMe LRU has since evicted
this snapshot's chunks (the retired advert carried `last_accessed_at`
from the host itself, though it was never populated). A pinned resume on
such a host passes every veto, pages from GCS anyway, and
`engram_resume_affinity_fallback_total` reports no fallback — a locality
"hit" with cold-path latency. Acceptable for phase 2 (the miss is
correctness-free and phase 3's peer/local fill shrinks the window);
possession-freshness belongs to the substrated readiness registry
(ADR 0076) if it proves to matter in prod.
