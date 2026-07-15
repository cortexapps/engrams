# 0095 — Peer fill of disk chunks: one fleet transport for resume, warmup, and prestage

Status: Proposed (2026-07-14)

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

## Evidence (2026-07 baselines)

Re-derived at PR time from Cloud Logging + prod PG (`enable_jobs`,
`session_events`, `snapshots`) — the fleet has no retained metrics
backend, so every number is sourced+windowed inline. Headlines: enable
prestage fan-out ≈ 22 min (dev-brain, `enable_jobs` ready-gap);
dev-brain base set = 1,808 × 16 MiB disk + ~42.5k × 512 KiB memory
chunks ≈ 50 GiB; GCS chunk fetch mean ~100 ms (capture-heavy window);
cross-host resume ~10 s class vs 0.61 s affinity-local (ADR 0078 P2
measurement). Fleet-stability precondition applies to all peer-win
claims: on a 1-host fleet there is no peer and the design degrades to
today's path by contract.

<!-- P0 evidence table lands here before the PR opens. -->

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

## Divergence log

- (updates land here as implementation proceeds)
