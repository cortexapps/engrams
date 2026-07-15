# 0095 — Peer fill of disk chunks: one fleet transport for resume, warmup, and prestage

Status: Accepted (2026-07-15) — prod-validated same day; see §Prod
validation. Commit chain: #677 (ADR + tier + wirings + chart striping,
WIRE 16), engrams-internal#74 (2×375 GB local-NVMe RAID0 kvm pool),
#679 (fresh-node init/mount-race fix), #680 (bundle-supervisor retry +
prefetch in-flight guard). Validation record: issue #678.

Issue: successor to #548 (GCS-free resume, ADR 0078 — phases 1+2 landed
via #587/#601; this ADR absorbs the unbuilt remainder and broadens it).
Prerequisite workstream: local NVMe for the KVM node pool
(engrams-internal; the ADR 0044 K5 "local SSD at /var/lib/engram"
follow-up — see §Hardware).

## Problem

Three chunk-distribution paths reach GCS when the bytes are already on a
live host's disk one LAN hop away:

1. **Idle → resume, cross-host.** When tier-0 affinity (ADR 0078 P2) is
   vetoed — snapshot host full on disk/RAM/CPU, cordoned, or gone — the
   resume lands elsewhere and reconstructs the divergent set from GCS,
   one round-trip per chunk, even while the snapshot host is alive with
   every chunk cache-resident. This is the original #548 case and the
   `agent_handshake` tail.
2. **New-host warmup.** A fresh host's prefetch supervisor pulls every
   enabled image's base working set (~46k objects, ~50 GiB for the
   current set) from GCS, one GET per chunk through a 16-permit
   semaphore — while sibling hosts hold every byte.
3. **Enable prestage.** After a capture, every host independently pulls
   the full base set from GCS (~22 min for dev-brain, measured via
   `enable_jobs`) — even though the capturing host holds the complete
   set on local disk the moment the upload finishes (ADR 0078 P1
   write-through). GCS reads scale N× with fleet size for 1× of unique
   bytes.

The cost is not egress dollars (same-region GCS→GCE egress is $0; op
cost is negligible). The cost is wall-clock × N-host amplification:
scale-up waits ~30 min for a warm host, an enable waits ~22 min for the
fleet, and a cross-host resume waits ~10 s for ~1.3 GiB.

## Hardware analysis (what actually bounds throughput)

Fleet: n2-standard-16 KVM nodes (Cascade Lake, 32 Gbps NIC ≈ 4 GB/s,
single TCP flow ≈ 1 GB/s intra-VPC).

- **The cache disk is the wall.** `/var/lib/engram` lives on a 300 GiB
  pd-ssd boot disk: **~144 MB/s** sustained read AND write
  (0.48 MB/s/GiB), 25× below the NIC — and shared with live guests' NBD
  IO (the "prestage straggler" observation is this contention). The
  node-pool TF has flagged local NVMe as a follow-up since ADR 0044 K5;
  this ADR makes it a prerequisite. Striped local NVMe ≈ 700 MB/s
  write / 1.3 GB/s read.
- **The landing path has no fsync** (correct: GCS is durability; the
  local cache is rebuildable). Landings are buffered write + rename, so
  a resume-sized set (1–6 GiB) is absorbed by the page cache at network
  rate even on pd-ssd; only ~50 GiB bulk pulls hit dirty-writeback
  throttling and run at disk rate. Local NVMe is therefore the lever for
  the two bulk paths and for serve-side reads; the resume path is
  network- and CPU-bound.
- **The teleport transport's ~20 MB/s/stream is a config artifact, not
  a gRPC property**: tonic's default 64 KiB HTTP/2 stream window, 1 MiB
  frames, and 8 streams multiplexed over ONE TCP connection (one
  `Channel`), capping the whole pull below a single flow. The fix is
  connections and windows, not a new protocol — subject to the
  loopback bench gate below.
- **sha256 costs ~0.5–1 GB/s/core here** (Cascade Lake has no SHA-NI;
  `write_local`'s doc comment already records this). At NIC-rate
  landings verification would cost ~4–8 cores next to live guests —
  see §Integrity for the decision.

Per-pair throughput target: **≥1 GB/s sustained** (the post-NVMe disk
rate) for bulk; **line rate into page cache** for resume sets. A seed's
4 GB/s NIC feeds 3–5 pullers at full rate; beyond that the readiness
tree (hosts that finish become seeds) fans out at log depth.

## Decision

**One standing, coordinator-hinted peer-chunk tier.** Every host-agent
serves cache-resident chunks by content hash over a new streaming RPC;
consumers are pointed at peers by coordinator-supplied hints; GCS is
always the floor. No per-purpose machinery: the same tier serves all
three paths. No export lifecycle, no rendezvous/DHT, no gossip —
"coordinator knows, hosts obey."

### The serve side: `PeerChunkGet`

New server-streaming gRPC on the host-agent (`engram-protocol` proto;
NOT an overload of `migration_fetch`, whose MigrationItem framing and
registry lookups are teleport semantics):

- Request: `{ hashes: [sha256; ≤ thousands], scope }`. Batch semantics —
  chunks stream back-to-back on the response; per-object round trips are
  eliminated (decisive for the ~42k × 512 KiB memory-chunk sets).
- **Cache-resident-only.** A locally-missing hash streams a `missing`
  marker and the requester sources it from GCS; a peer can never induce
  a GCS read on the serving host.
- `scope: BaseImage(digest) | Snapshot(snapshot_id)`, validated against
  the serving host's ready∪prestaging set / locally-known snapshots
  (reject on mismatch). Scope exists for observability and rate class,
  not security (see §Trust).
- Serve-side QoS: a host-global concurrent-stream semaphore
  (`ENGRAM_PEER_SERVE_STREAMS`, default 16) returning
  `RESOURCE_EXHAUSTED` as backpressure; reads go through the
  page-cache-friendly path (capture-fresh chunks are RAM-hot on the
  seed).
- Home: `HostServiceImpl` holds the `ChunkCache` handle directly — this
  is host infrastructure, not sandbox lifecycle, so VZ hosts serve too;
  a cache-less Process host returns a typed unavailable.

### The requester side: a tier in the single populate owner

`ChunkCache::get/prefetch(hash, fetch_fn)` (the ADR 0075 single
populate owner: singleflight, one hashing site) gains a tiered fetch:
**local NVMe → peer (if a binding is armed) → GCS**. The hardcoded
`source="gcs"` fill label moves from the cache to the closure seam so
`engram_chunk_fill_total{source=local|peer|gcs}` is honest from birth.

Transport shape (gated by a loopback bench, recorded here at PR time):
2–4 separate TCP connections, adaptive or ~16 MiB HTTP/2 windows, 4 MiB
frames, hardware CRC32C per frame. Requester pipeline: bounded
network → CRC → buffered write stages; no fsync; bulk landings via
budget-respecting writes (never `put_no_evict` — a fill must not blow
the ADR 0070 budget).

### Source selection: coordinator hints

| Path | Hint |
|---|---|
| Resume | `SnapshotMetadata.peer_hints: Vec<HostAddr>` (≤2), stamped by the resume assembler when `dest != snapshots.host_id` and the source is alive, wire-compatible, and uncordoned. The destination computes the divergent set (session manifests ∖ base ∖ locally-resident), orders it hot-first from the working-set trace, and pulls **synchronously pre-resume** (the ADR 0045 C1-proven shape; ~1.3 GiB p50 lands in ~1–1.5 s at line rate), then arms a fault-time peer binding in the populate closures as the safety net for the tail. |
| Warmup | Heartbeat-ack image entries gain `warm_peers: Vec<PeerRef>` (serde-default JSON; no wire bump), assembled per digest from `ready_images ∩ live ∩ ¬cordoned`, excluding the recipient. The prefetch supervisor pulls batches through the peer tier. |
| Prestage | Identical to warmup on the `prestage_images` entries. The capturing host's own prefetch is an all-local stat walk, so it flips `ready_images` within one reconcile tick of the snapshot row landing and becomes the seed automatically; hosts that finish join `warm_peers`, forming the fan-out tree. `enable_scanner`'s prestaging stage (advertise + poll) is unchanged. Per-host start jitter softens the herd's leading edge. |

Memory chunks (512 KiB) ride the tier from day one — they dominate GCS
object count and benefit most from streaming.

## Integrity (a deliberate deviation from ADR 0021)

ADR 0021 established verify-on-populate/trust-on-read: every landed
chunk is sha256-verified once, and a present cache file is known-good.
This ADR **removes sha256 from the peer landing path** (throughput
decision: verification is the only remaining CPU wall between the peer
path and line rate on no-SHA-NI hosts) and compensates off the critical
path:

- Peer frames carry **hardware CRC32C** (SSE4.2; catches wire and
  framing errors that TCP's 16-bit checksum can miss).
- Peer-landed chunks are marked **unverified-origin**. A background
  trickle scrubber sha256s them lazily; a mismatch deletes the file,
  refetches from GCS, and increments an alert counter
  (`engram_chunk_scrub_total{outcome=ok|corrupt}`).
- **A host serves onward only verified chunks**: its own GCS-landed and
  self-produced chunks qualify by construction; peer-landed chunks
  qualify after scrub. Corruption (source-disk bitrot, a serve bug)
  can therefore travel at most one hop and is caught by the scrub —
  it can never propagate through the fan-out tree.
- GCS populates keep sha256 (that path is RTT-bound; the hash is free).

Reversal clause: if hosts ever become multi-tenant across trust
domains, or scrub-corruption counters fire in prod, sha256-at-landing
returns behind the same seam (it is one closure swap).

## Trust model (a deliberate deviation from ADR 0045 C1)

ADR 0045 C1 gated peer chunk serving behind per-export unguessable
tokens + manifest-scoped allowlists. For **durable content-addressed
chunks** inside this fleet boundary that is bookkeeping, not security:

- Every host-agent holds GCS credentials that read every chunk; a
  standing `PeerChunkGet` grants nothing a requester doesn't have.
- `migration_fetch` itself authenticates only by unguessable export_id
  over the flat-cluster gRPC port — there is no transport identity
  today; exports never added cryptographic security, only scoping.
- A sha256 is itself a weak capability: unguessable without a manifest,
  and manifests never reach guests.

What the export machinery actually protects — `StateBin`, sealed RAM
pages, paused-guest artifacts — **stays behind `migration_fetch`
unchanged**. The teleport contracts (registry, seal, TTL, allowlists)
are reused as prior art, not modified.

## Degradation contract (the reliability floor)

- No hints ⇒ zero new work; byte-identical to today's GCS path. An N=1
  fleet never dials anything.
- ≤2 hinted addrs; one 2 s connect dial each per pull window; then the
  GCS floor. Never a per-chunk retry ladder.
- `RESOURCE_EXHAUSTED` = backpressure: the current batch goes to GCS
  and the peer is NOT marked lost. Connect/stream error = peer lost for
  the window (requester-side 30 s health cache).
- A `missing` marker sends that hash to GCS; the peer stays healthy (an
  honest miss is not a fault).
- A peer failure must never fail or slow the consuming operation beyond
  the bounded dials; no "skip if it looks empty" shortcuts in the
  divergent-set computation.
- VZ serves (the tier sits above `SandboxBackend`); Process returns
  typed unavailable; the consumer counts the fallback and proceeds.

## Metrics

`engram_chunk_fill_total/bytes{source=local|peer|gcs, kind, scope}`
(relabeled at the closure seam), `engram_peer_serve_total/bytes{outcome}`
+ in-flight/permit-wait, `engram_peer_dial_total{outcome}`,
`engram_chunk_scrub_total{outcome}`,
`engram_resume_divergence_source_total{tier}` (the counter ADR 0078
deferred to its phase 3 lands here).

## Non-goals (pinned)

- **Evict-time prestage on cordon + rung-3 parked-local** (ADR 0078
  phase 4): deferred, not moved here. With peer fill, a resume off a
  live source is ~1–2 s, and rolls drain via evacuation (which already
  carries chunks); revisit if
  `engram_resume_affinity_fallback_total{reason="dead"}` says the
  source-died window matters. Rung-3 is a cache-pin policy orthogonal
  to transport; it belongs to the parking ladder (ADR 0074) if revived.
- Content-defined chunking as chunk identity; CI-time chunk publish
  (#538/#546); an e2b-style shared-NFS cache tier (we have
  content-addressed chunks + big host NVMe — the fleet IS the cache
  tier); rendezvous hashing (hints cover every named path on this
  fleet's scale; the anonymous-miss case stays GCS and is counted).
- Teleport (`MigrationRegistry`/`migration_fetch`/seal) semantics.

## Evidence (re-derived 2026-07-14/15 UTC; Cloud Logging + prod PG)

The fleet has no retained metrics backend (pod-local `/metrics` only),
so rows are sourced+windowed inline; "unavailable" is stated rather
than estimated. Fleet during collection: 1 live host + churn — the
fleet-stability precondition applies to every peer-win claim.

**Enable prestage** (`enable_jobs`, last 4 enables, 2026-07-15 ~02:30Z):

| image | total | fan-out gap (last capture stage → ready) | disk chunks | base-mem chunks |
|---|---|---|---|---|
| dev-brain | 41.5 min | **22.1 min** | 1,808 (×16 MiB = 28.25 GiB) | 44,338 |
| dev-engrams | 201 s | 14.2 s | 656 | 775 |
| demo | 56 s | 14.3 s | 24 | 309 |
| engineering-blog (reused_full) | 34 s | **0.37 s** | 176 | (reused) |

Readings: the recaptured-image floor is ~14 s nearly independent of
disk chunk count (base-memfile materialization, not chunk fill);
`reused_full` — chunks already resident — collapses the gap to 0.37 s,
the strongest single argument for resident/peer fill; dev-brain's
22 min is memory-snapshot-object-count-bound (44k × ~0.5 MiB) and the
host logs show a **re-prefetch churn loop** ("base chunks no longer
resolvable locally; flipping to not-ready") — the 300 GiB disk
LRU-evicts just-pulled base chunks mid-prestage. The NVMe prerequisite
attacks the churn; the peer tier attacks the refill cost.

**Fetch decomposition** (`engram_chunk_fetch_seconds`, one host, ~2 h
capture-heavy window, n=148k GCS / 341k NVMe): GCS fetch mean 100 ms
per chunk (p50 ~45 ms, p95 ~300–500 ms); NVMe read of a landed chunk
2.0 ms. The histogram times ONLY the fetch closure — landing is
un-instrumented; no dirty-writeback signal is exposed. Verdict: the
paths are GCS-round-trip-bound, not landing-bound, at current scale.

**Resume** (`session_events`, 14 d): resumed 100, recovered 85,
evicted 139. Cross-host vs affinity-local wall-clock is currently NOT
derivable (the `resumed` event is a completion marker; `host_id` nulls
on teardown; coord pods too young) — the trusted anchors remain ADR
0078 P2's prod-measured 0.61 s affinity-local restore vs the 92 s GCS
page-in class. `engram_chunk_fill_total{source=gcs}` on the observed
host: 148k fills / 460 GiB in one enable burst.

**Transport bench** (loopback, release build, Apple dev box — the
acceptance gate; re-measure on a Linux host at flip time): the tuned
shape (4 conns × 16 MiB windows + adaptive, 4 MiB CRC32C frames)
sustains **1,112 MiB/s** end-to-end through the real serve arm, CRC,
and cache landing (1,323 MiB/s at 8 conns; raw-TCP loopback ceiling
3,694 MiB/s). The pre-0095 teleport shape's ~20 MB/s/stream artifact is
confirmed config, not gRPC physics. **Verdict: tuned tonic ships; a raw
TCP data plane buys nothing under the NVMe write cap** (§Hardware) and
would add a second listener + protocol surface.

## Acceptance criteria

1. Enable prestage ≤3 min wall (stretch 1.5–2 post-NVMe); GCS reads per
   enable ≤1.1× unique bytes (from N×).
2. New-host warmup ≤3 min for the ~50 GiB enabled set post-NVMe; GCS
   bytes ≈ only chunks no live peer holds.
3. Cross-host resume p50 ≤1.5 s / p90 ≤5 s with a live source;
   `divergence_source{tier=gcs}` ≈ 0 there; affinity-local unchanged.
4. Dead/absent peer: ≤ one 2 s dial per window; N=1 fleet
   byte-identical (asserted by the unchanged e2e lane).
5. Transport bench artifact recorded here: chosen shape sustains ≥ the
   local-NVMe write rate on loopback.
6. Scrub: a synthetically corrupted peer-landed chunk is caught,
   deleted, refetched, counted; serve-onward never streams an
   unverified chunk; backlog drains within ~10 min of a bulk pull.

## Prod validation (2026-07-15, 2-host NVMe fleet — the Accepted basis)

Measured the day the stack deployed (#677 → internal#74 → #679 → #680);
full log-line evidence on issue #678.

| Acceptance criterion | Result |
|---|---|
| 1. Prestage ≤3 min, GCS ≤1.1× unique | **~3.2 min** (vs 22.1 min = ~7×, and BOTH hosts vs one); GCS ≈1× + a ~200-chunk upload-race tail. Enable total 41.5→20.5 min. |
| 2. Warmup GCS ≈ only-unheld chunks | Organic window: 15.1 GiB @ 523 MiB/s, `missing_on_peer=0`. A no-live-seed bringup (both nodes fresh simultaneously) correctly ran GCS-parallel — the contract's degenerate case, observed. |
| 3. Resume p50 ≤1.5 s with live source | Affinity-local full resume AND rung-2 ascent: **<1 s** incl. an md5 integrity check over the divergence; data intact across 4 park/resume cycles. The isolated CROSS-HOST wall-clock stayed unmeasured — every controlled attempt was absorbed by a faster designed path (ascent, affinity) — see divergence log. |
| 4. Dead/absent peer bounded | Exercised organically: readiness-flip scope rejects, upload-tail misses, and backpressure all degraded to GCS per contract, no session impact. |
| 5. VZ serves / Process unavailable | CI lanes (loopback suite runs on all platforms; e2e Process = the no-hint arm). |
| 6. Transport ≥ NVMe write rate | Superseded by prod: **28.7 GiB @ 692 MiB/s** and 21,870×512 KiB @ 283 MiB/s through real fleet NICs/NVMe (loopback bench: 1,112 MiB/s release, macOS). No raw-TCP plane needed. |
| 7. Scrub catches corruption, serve-gate holds | Unit/integration-pinned; prod counters during the (unrelated) prefetch storm: `ok=8,731, missing=1,023, corrupt=0` — `missing` = chunks LRU-evicted before their scrub under storm pressure, the safe direction. |

Incidental finds fixed forward during validation: the fresh-node
init/mount race (#679), the bundle-supervisor retry wedge (#680), and
the **prefetch redo-loop storm** (#680) — no per-digest in-flight guard
let one false `still_warm` flip stack unbounded concurrent 24 GiB
memfile re-materializations, whose fs-headroom churn evicted
freshly-landed unpinned chunks and kept the recheck false (77k
evictions, ~1 TB/hr NVMe reads, idle fleet). Very likely the true
mechanism behind the historical "dev-brain re-prefetch churn".

## Divergence log

- **Cross-host resume wall-clock: measure on the next organic
  occurrence.** Three deliberate attempts all landed on faster designed
  paths (placement-race → affinity local; rung-2 ascent under cordon —
  correct: cordons don't kill parked VMs; the D5 async destroy leg —
  Idle ≠ VM-gone), and the pressure reaper rightly holds rung-2 parks
  on a headroomed fleet. The mechanism is prod-proven by the prestage /
  warmup windows (same hints → dial → CRC landing → GCS floor); any
  host roll or evacuation with idle sessions will emit
  `peer resume pre-pass window complete` on the destination — fold that
  wall-clock in here when it lands.

- **Fault-time peer tier: built but not wired.** The plan called for a
  fault-time peer binding in the populate closures as the resume tail's
  safety net. The dest-side pre-pass is SYNCHRONOUS over the full
  divergent set (the ADR 0045 C1-proven shape — a background pull loses
  the wake-up race), so by construction there is no post-restore fault
  window to serve; a mid-pull peer death leaves the remainder to the
  fault path's existing local→GCS chain, which is today's behavior.
  `PeerBinding` (peer_fill.rs) is the ready building block if p90
  divergent sets ever justify a hot-sync/tail-async split; wiring it
  into the populate closures via `ChunkCache::get_with_source` (which
  sha256-verifies fault-time singles — at one chunk per fault the hash
  is noise) is deliberately deferred until measured need.
- **`Snapshot` scope is observational, not validated.** The serving
  host has no authoritative local index of snapshot ids (the
  coordinator only hints requesters at the capturing host), so the
  serve arm validates `BaseImage` against its ready set and counts
  `Snapshot` as-is — hash-capability + resident-only remains the gate.
- **BaseImage scope checks `ready`, not ready∪prestaging** — a
  prestaging host may not hold the chunks yet, and the coordinator only
  seeds from `ready_images` anyway.
- **Evacuation gained hints too**: the snapshot-rehome leg stamps its
  draining (cordoned-but-alive) source — `host_can_serve_chunks`
  deliberately ignores the cordon bit, which also lets mid-drain hosts
  keep seeding base images during rolls.
- **Backpressure gets one jittered retry** in the prefetch pre-pass
  (2–5 s, pid-keyed) before falling to GCS — a saturated seed at enable
  fan-out is busy, not dead; still bounded, never a ladder.
- **CI shape**: the tier's contract is pinned by a cross-platform
  loopback integration test (`tests/peer_fill_loopback.rs` — real gRPC
  server, real pull machinery, scrub + serve-gate + backpressure +
  bounded-dial assertions) that runs in the ordinary Rust lanes on
  every platform, instead of a new KVM-gated FC test: the tier is
  VM-independent by design, and a booted FC guest would add wall-clock
  without adding coverage of any property above (AGENTS.md: size to
  the property). The bench (`tests/peer_transport_bench.rs`) is
  `#[ignore]`'d/manual.
- **Wire posture**: the heartbeat-ack `warm_peers` field and the
  `PeerChunkGet` RPC are independently mixed-roll-safe; the WIRE 15→16
  bump is pinned to the `SnapshotMetadata.peer_hints` bincode addition
  and makes the whole feature's deploy posture explicit (lockstep
  coord+host roll, skewed hosts drain via `host_wire_version_ok`).
