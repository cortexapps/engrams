# ADR 0016: COW observability and continuous disk sync

Status: 2026-05-23 — **Proposed**, Phase A shipped. Supersedes ADR
0015 M5 "Known regression — chunk-store GC deleted" (see
`docs/adr/0015-system-design-v2.md` §"Known regression — chunk-store
GC deleted") once Phase C lands.

Phase chain (filled in as commits land — same shape as ADR 0014's
M1.x annotations and ADR 0015's per-milestone commit lists):

- **Phase 0** — this ADR, in Proposed status. Commit `eadbd03`.
- **Phase A** — observability surface. Four commits + this update:
  `050b189` (ChunkedDiskBackend accessors), `335f14a` (CowState
  types + trait + gRPC + PooledBackend impl, absorbed the planned
  commit 2), `285adf0` (coord endpoints + 1s-TTL cache), `7324baa`
  (web app integration). **Shipped 2026-05-23.**
- **Phase B** — continuous disk sync (5 commits + ADR update). _(pending)_
- **Phase C** — chunk-GC pin-set redesign (6 commits + ADR final pass). _(pending)_

The ADR is updated at the end of each phase with what shipped, what
diverged from this design, and any pitfalls future implementers should
know about. The final commit at the end of Phase C flips the status to
**Accepted**.

---

## Phase A as-built notes (2026-05-23)

**Divergences from the Phase A design above:**

- **The planned "Phase A commit 2 — `ChunkCache::has()`" collapsed
  to a no-op.** `ChunkCache::contains(hash)` already existed at
  `crates/engram-chunk-store/src/cache.rs:337`, with exactly the
  semantics the design called for (`fs::try_exists` against the
  per-hash cache path; cheap stat). The locality counter in
  `PooledBackend::cow_state_for_entry` uses `contains` directly.
  No new helper added; the commit slot dissolved into commit 3's
  notes.
- **Base-chunk locality cost.** The open question called out in the
  ADR was whether `ChunkCache::has` would be O(1) or O(n). It's
  O(1) — one `fs::try_exists` per chunk hash. For a 256-chunk
  manifest the per-call cost is bounded by FS stat latency (sub-ms
  per stat on NVMe). Total per-request is sub-100ms even at 1024
  chunks; coord caches the response for 1s anyway. No HashSet
  retrofit needed.
- **`last_snapshot_unix_ms` lives on `PooledBackend`, not the inner
  FC backend.** The Phase A design sketched it on
  `SandboxBackend::last_snapshot_at(sandbox_id)`. Putting it on
  `PooledBackend` is cleaner: that's where the snapshot wrapping
  happens, and the stamp is gated on the post-processing
  succeeding (BlobStorage uploads done, before
  `commit_snapshot`). Cleared on `destroy`. Inner backends are
  unchanged; only the wrapper that owns the upload pipeline
  records the timestamp.
- **`HostCowState` not wired to `HostRegistry::unregister`.** The
  design imagined explicit cache eviction on host drop. Skipped
  in this phase: the 1s TTL self-cleans orphaned slots before the
  web app's next poll, and the DashMap is bounded by HostId count
  (slow growth, not unbounded). If host churn ever shows up in
  metrics, wire `cow_state_cache.forget_host(id)` from the
  dead-host detector and the cross-replica pg_listener path.
- **`/sessions/:id/cow-state` reuses the host cache via
  `fetch_for_host`** rather than maintaining a separate
  per-session cache. Two sessions on the same host share the
  refresh budget; the per-session response is a single filtered
  row from the host's vec. Same effect as a per-session cache but
  half the bookkeeping.

**Pitfalls / drive-bys encountered:**

- **Workspace-scale cargo invocations during edits contend with
  rust-analyzer's background check.** A `just check` after a
  multi-crate edit burst took 14m on first try because a stale
  rust-analyzer (from Thursday) + the live one (from the current
  session) both ran `cargo check --workspace`, and the lock
  contention pushed clippy behind them. Captured in the
  `[no_workspace_cargo_mid_edit]` memory; future phases will avoid
  the pattern. Mitigations applied this phase: killed the stale
  rust-analyzer, batched all edits per commit and only ran
  `just check` at commit boundaries.
- **`pnpm install` was missing `@rollup/rollup-darwin-arm64`** in
  the local `node_modules`. The lockfile listed it correctly; the
  prior install had only the Linux variants — must have been an
  install done on the dev VM or in CI that propagated. Vitest
  failed with `Cannot find module '@rollup/rollup-darwin-arm64'`.
  Fix: `CI=true pnpm install` on the macOS host. No lockfile
  change needed; documented in the commit body for `7324baa`.
- **`HostClient::host_for_sandbox` returns `(HostId, SessionState)`
  but not `SessionId`.** The first cut of the per-host endpoint
  used it for the sandbox→session join and silently always
  returned no session_id. Replaced with one
  `list_active_sandbox_assignments_on_host(host_id)` call per
  request (already indexed, used by reconcile); builds a
  per-request HashMap. Now correct + one fewer query per record
  than the original sketch.

**Notes for Phase B implementers:**

- The accessors landed in `050b189`
  (`dirty_chunks_count`/`dirty_bytes`/`current_manifest_ref`/
  `last_flush_unix_ms`) are the inputs the `FlushScheduler` will
  read for its threshold check. `dirty_bytes()` is the one that
  the `tokio::sync::Notify` poked from `ensure_dirty` should
  compare against `dirty_threshold_bytes`. The scheduler doesn't
  need to compute its own bytes-tracker — reuse the accessor.
- `PooledBackend::last_snapshot_unix_ms` is `DashMap<SandboxId,
  i64>`. Phase B's `live_disk_manifest_*` should live in PG (per
  the design), not on `PooledBackend`, because the data must
  survive host-agent restart and be visible cross-replica. Don't
  let the proximity to `last_snapshot_unix_ms` tempt anyone into
  the host-local route.
- The cow_state RPC's `bincode`-in-`bytes` wire shape is the
  pattern Phase B's `PublishLiveManifest` should follow if it
  ends up gRPC. If it ends up HTTP (per the ADR's "open
  questions"), the host→coord channel that heartbeats already use
  is the natural fit — no new auth surface.

---

## Context

ADR 0015 M4 originally framed `Sandbox` as a migratable value:
snapshot on host disappearance, restore on a healthy peer. Working
through the data tiers exposed two structural issues we must fix
before evacuation logic is meaningful.

### Issue 1 — zero visibility into COW state

Today nothing on coord knows how much dirty disk a session is
carrying, when its last flush ran, or what the recovery-point
objective (RPO) would be if its host died right now. `/api/hosts/:id`
reports capacity, running sandbox count, and ready-image digests
(M5), but not a single byte of per-session COW state. We can't:

- Tune snapshot cadence empirically — we have no data on dirty-set
  size over a session's lifetime.
- Decide migration eligibility per session — "Active with a recent
  snapshot" is policy without a measurement.
- Surface RPO to users — "your work is durable as of T-30s" is
  unsayable when T is unknown.

### Issue 2 — snapshot-as-flush is the only path to BlobStorage

Dirty disk chunks live in `ChunkedDiskBackend.dirty: HashMap<usize,
Vec<u8>>` (`crates/engram-host-agent/src/disk_daemon/backend.rs`) —
host RAM only — until the next snapshot runs. There is no background
flush; `flush()` is only called from `PooledBackend::snapshot`
(`crates/engram-host-agent/src/pooled_backend.rs:1350`). Concretely:

- An Active session that has never been Idle has zero recoverable
  bytes in BlobStorage. Reactive evacuation against the current
  pipeline would frequently degrade to `Dead`.
- A session that went Idle once and then resumed accumulates dirty
  bytes on the new host with no incremental durability until the
  next Idle. The longer it runs Active, the worse the RPO.
- Operator drain has to pay the entire snapshot cost synchronously
  per session before the drain can complete.

Reactive evacuation built on this pipeline is a paper tiger. The
right shape is to observe what's where first, then ensure disk state
flows to BlobStorage continuously, then build evacuation on the
established invariant.

### Issue 3 — chunk-GC is gone and the deletion didn't fully think
about ADR 0015 M5

Per the M5 "Known regression" section, the chunk-store GC was
deleted on 2026-05-23. The decision was correct (the prior GC had
two compounding bugs that masked each other; patching it would have
left the same scaffold), but the redesign was deferred. Bucket size
grows monotonically until a new GC ships. Continuous disk sync makes
this materially worse — every flush produces a new manifest version,
each version references chunks no prior version did — so we cannot
ship continuous sync without also shipping the new GC. ADR 0015's
"Design constraints for the next chunk GC" subsection (M5 §"Design
constraints") catalogues what the next GC must get right; this ADR
discharges those constraints concretely.

---

## Decision

Ship three coupled changes as a single milestone:

1. **Phase A — observability surface.** Per-(session, sandbox, host)
   COW state, queryable via a new `CowState` RPC fanned out by coord
   through a 1-second cache and rendered in the web app.
2. **Phase B — continuous disk sync.** A per-sandbox `FlushScheduler`
   that flushes dirty chunks on a cadence (default 30s) or threshold
   (default 256 MiB). Each flush publishes a new disk manifest version
   to a new `sessions.live_disk_manifest_*` pointer in Postgres.
3. **Phase C — chunk-GC pin-set redesign.** A pin set that unions
   `enabled_images` chunks, `sessions.live_disk_manifest_*`,
   and `snapshots WHERE recoverable=true`. Mark-and-sweep with a
   24-hour grace period and a generation barrier against the
   flush-vs-sample race.

Two things are explicit non-goals:

- **Continuous memory sync.** Firecracker doesn't expose
  page-granularity dirty tracking suitable for incremental capture.
  "Continuous memory" would require VMM-level work (live-migration
  pre-copy or CRIU-style dirty-bit tracking) that is out of scope.
  Memory durability stays bounded by explicit snapshot cadence; the
  diagnostic surface makes this honest via
  `last_snapshot_unix_ms`.
- **Migration / evacuation primitive.** Deferred to a follow-up
  milestone (M4.1 of ADR 0015). The observability + continuous sync
  + GC pin-set this ADR builds are the preconditions; evacuation
  becomes a far smaller change once they land.

### Data model — what we can observe per (session, sandbox, host)

Four tiers:

| Tier | Live on host | Durable in BlobStorage | Per-session pointer |
|---|---|---|---|
| Base image chunks (immutable) | NVMe chunk cache (shared cross-session) | Yes | Disk manifest references them by content hash; manifest digest in `enabled_images` |
| Dirty disk chunks | `ChunkedDiskBackend.dirty` (host RAM, 16 MiB/chunk) | **Not until `flush()`** | None today — host-RAM-only. **This ADR adds `sessions.live_disk_manifest_*`** |
| Memory image | FC mmap (live RAM) | **Not until `snapshot()`** | `snapshots.memory_manifest_*` (snapshot-bounded only) |
| `state.bin` + sidecar | FC dir on host disk | After snapshot upload | `snapshots.{state_blob_key, sidecar_blob_key}` |

The "session → reliable chunk set" invariant **after this ADR**:

```
session.id
  ├── live disk:  sessions.live_disk_manifest_id @ version  (continuous, updated per flush)
  ├── base image: enabled_images by image_uri → manifest digest
  └── recoverable point-in-time:  latest snapshots row for session_id
        ├── disk_manifest_id @ version
        ├── memory_manifest_id @ version    (memory only here — no live tier)
        ├── state_blob_key
        └── sidecar_blob_key
```

`sessions.live_disk_manifest_*` is the new bookkeeping primitive.
Every host-side `flush()` (whether snapshot-driven or
`flush_loop`-driven) publishes a new manifest version and updates this
pointer via a coord callback (debounced). On host crash, the pointer
stays valid in PG; chunks in BlobStorage stay live (pinned by the new
GC); a future evacuation milestone can rehydrate disk state from PG
+ BlobStorage without a snapshot row.

### Phase A — observability surface

**Host-agent.** Add `cow_state(sandbox_id)` and
`cow_state_all()` on `PooledBackend`, reading from
`nbd_sandboxes[sandbox_id].backend` (dirty count + bytes + current
manifest ref + last flush timestamp) and `chunk_cache.has()` (base
chunk locality). Memory tier fields come from the inner FC backend's
`last_snapshot_at(sandbox_id)`.

**Protocol.** Two new RPCs in
`crates/engram-protocol/proto/host_service.proto`:

```proto
rpc CowState(SandboxIdMessage) returns (CowStateResponse);
rpc CowStateAll(Empty) returns (CowStateAllResponse);

message CowStateResponse {
  // Disk tier
  string disk_manifest_id = 1;
  uint64 disk_manifest_version = 2;
  uint32 dirty_chunks = 3;
  uint64 dirty_bytes = 4;
  uint64 last_flush_unix_ms = 5;
  uint32 base_chunks = 6;
  uint32 base_chunks_local = 7;   // resident in NVMe cache
  // Memory tier (snapshot-bounded; null between snapshots)
  optional string memory_manifest_id = 8;
  optional uint64 memory_manifest_version = 9;
  uint64 last_snapshot_unix_ms = 10;
}
```

`HostClient` trait grows the two methods; both `LocalHostClient` and
`GrpcHostClient` get impls.

**Coord.** Two new endpoints:

- `GET /api/hosts/:id/cow-state` — fan out `cow_state_all()` to the
  one host, join sandbox_ids back to session_ids, return per-sandbox
  records.
- `GET /api/sessions/:id/cow-state` — resolve `host_for_sandbox`
  (M3), call `cow_state(sandbox_id)`, return one record. For Idle /
  HostLost sessions with no live sandbox, return the durable
  snapshot row's fields with a `tier="durable_only"` marker.

Coord caches per-host responses for ~1 second behind
`HostRegistry::cow_state_for_host(host_id)`. Heartbeat stays lean —
this is a deliberately separate channel so the diagnostic surface
can poll faster than 5s without bloating the heartbeat payload.

**Web app.** New `CowState` component, embedded in `HostManifest`
(toggle-expand per host card) and `SessionDetail` (always-visible
card). Polls at 2s via React Query. Hand-maintained TS types in
`web/src/types.ts`, matching the current pattern.

### Phase B — continuous disk sync

Per-sandbox `FlushScheduler` (new module
`crates/engram-host-agent/src/disk_daemon/flush_scheduler.rs`):

```rust
pub struct FlushSchedulerConfig {
    pub interval: Duration,         // default 30s, ENGRAM_FLUSH_INTERVAL_SECS
    pub dirty_threshold_bytes: u64, // default 256 MiB, ENGRAM_FLUSH_DIRTY_THRESHOLD_MIB
    pub enabled: bool,              // default true; ENGRAM_CONTINUOUS_FLUSH_DISABLED=1 turns off
}
```

Loop:

1. `tokio::select!` between `interval.tick()` and a `tokio::sync::Notify`
   poked from `ChunkedDiskBackend::ensure_dirty` when `dirty_bytes()`
   crosses `dirty_threshold_bytes`.
2. Call `backend.flush()`. Skip the callback if `chunks_flushed == 0`.
3. Invoke `callback.publish(session_id, sandbox_id, manifest_ref)`.

Spawned on `PooledBackend::attach_nbd`, aborted on detach. Snapshot's
`flush()` and the scheduler's `flush()` are linearly ordered via the
existing `ChunkedDiskBackend` flush mutex (or an explicit one added in
Phase B commit 9 if the current mutex doesn't cover all relevant
fields).

**Coord side**: new endpoint `POST /internal/live-manifest`
(host→coord, reusing the existing host→coord HTTP channel + bearer
auth that heartbeats already use). Handler calls
`MetadataStore::update_live_disk_manifest(session_id, sandbox_id,
manifest_ref)`. The query gates on `sessions.sandbox_id == publish.sandbox_id`
so a stale publish from a destroyed sandbox can't clobber a fresh
binding (drop-on-floor with a warn log).

**Schema** (migration `0033_sessions_live_disk_manifest.sql`):

```sql
ALTER TABLE sessions
  ADD COLUMN live_disk_manifest_id UUID NULL,
  ADD COLUMN live_disk_manifest_version BIGINT NULL,
  ADD COLUMN live_disk_manifest_at TIMESTAMPTZ NULL,
  ADD CONSTRAINT live_disk_manifest_both_or_neither CHECK (
    (live_disk_manifest_id IS NULL) = (live_disk_manifest_version IS NULL)
  );
CREATE INDEX idx_sessions_live_disk_manifest
  ON sessions (live_disk_manifest_id)
  WHERE live_disk_manifest_id IS NOT NULL;
```

Partial index keeps the structure narrow as terminal rows accumulate
(same pattern ADR 0015 M3's migration 0032 used for `sandbox_id`).

### Phase C — chunk-GC pin-set redesign

The new pin set:

```
pinned = union(
    enabled_images.chunks(),                                  // base layers
    sessions.live_disk_manifest_*,                            // live disk per session
    snapshots WHERE recoverable=true OR retention_not_expired
        .disk_manifest, .memory_manifest, .state_blob_key, .sidecar_blob_key,
)
```

This discharges ADR 0015 M5 §"Design constraints" #1: the live set
unions every source of live manifest lineages. **`sessions` and
`enabled_images` are both in the union**, where the old GC's
`list_live_disk_manifest_ids`/`list_live_memory_manifest_ids` saw
only `snapshots`.

Implementation (`crates/engram-coordinator/src/chunk_gc/`):

- `PinSet::collect(meta) -> PinSet` — three SELECTs (one per source)
  into a single HashSet of content hashes.
- `PinSet::contains(content_hash) -> bool` — O(1) lookup.
- `gc_sweep_loop` — background task, default hourly
  (`ENGRAM_CHUNK_GC_INTERVAL_SECS`).
  1. Read `chunk_generation` (one-row PG table) into `gen_before`.
  2. `PinSet::collect()`.
  3. List `BlobStorage` chunks under `chunks/` prefix.
  4. For each chunk: if not in pin set, upsert into
     `chunk_gc_candidates(content_hash, first_seen_at, last_seen_at)`.
  5. Read `chunk_generation` into `gen_after`. If `gen_after != gen_before`,
     **restart the sweep** — a flush published a new manifest mid-sweep
     and our pin set is stale.
  6. Separately, sweep `chunk_gc_candidates` for rows where
     `first_seen_at < now() - grace_period` (default 24h) and delete
     those chunks from BlobStorage. Remove the candidate rows.

`MetadataStore::update_live_disk_manifest` bumps `chunk_generation`
in the same transaction so the barrier is structurally consistent
with the new-manifest write.

This discharges constraint #2 (retention works correctly because
candidate promotion is timestamp-driven and the grace period is
explicit in PG, not derived from a backend-specific etag) and
constraint #4 (we keep a true live-set approach but the union
construction makes "forget to extend GC" structurally harder — adding
a new manifest lineage requires either adding it to `PinSet::collect`
or relying on the generation barrier to catch the race).

Constraint #3 (OCI tier-3 fallback in the prefetch chunk store) is a
separate concern — fixed independently if needed; pin set correctness
alone doesn't depend on it.

Admin endpoints (per `[explicit_admin_triggers_for_testability]` —
the GC sweep is an implicit trigger and needs an explicit admin
counterpart firing the same primitive):

- `POST /api/admin/chunk-gc/dry-run` — list what would be deleted.
- `POST /api/admin/chunk-gc/sweep` — run one sweep immediately.
- `GET /api/admin/chunk-gc/candidates` — read the candidate table.

### Why a separate ADR

ADR 0015 is the v2-direction summary; M5 explicitly punted the GC
redesign with "Design constraints for the next chunk GC" but left the
design unwritten. This work is large enough (three new subsystems
coupled together) and structurally distinct enough from ADR 0015's
"abstractions, not scab fixes" framing that an inline section would
inflate ADR 0015 past the point where it scans as one document. A
fresh ADR also makes the supersede relationship with M5 §"Known
regression" clean: M5's section becomes a one-line pointer here once
Phase C lands.

---

## Consequences

### What gets easier

- **Per-session RPO is observable.** The web app can show "your disk
  state was durable as of T-12s; memory state as of T-3h" honestly.
  Snapshot cadence becomes a tuning decision driven by data, not
  guesswork.
- **Active sessions have meaningful durability.** Today an
  Active-never-idle session has zero recoverable bytes in
  BlobStorage. After Phase B, every Active session has a live disk
  manifest in PG pointing at chunks that survive host crash. Memory
  RPO stays snapshot-bounded but the disk RPO becomes "≤ flush
  interval" continuously.
- **Migration becomes a smaller change.** M4.1 (evacuation) can lean
  on `sessions.live_disk_manifest_*` instead of forcing a fresh
  snapshot on the source host. The host is alive → take a fresh
  memory snapshot only (~1–4 GiB) and use the existing live disk
  manifest. The host is dead → restore disk from the live manifest
  and accept memory loss (or use the most recent snapshot row's
  memory manifest if available).
- **GC is correct by construction.** The union+barrier shape rules
  out the M5 §"Known regression" failure class — adding a new
  manifest lineage either updates the union (visible in PR diff) or
  is caught by the generation barrier mid-sweep.

### What gets harder

- **BlobStorage write rate increases.** Continuous flush on 100
  Active sessions at 30s cadence ≈ ~3 flushes/s in aggregate;
  per-flush bytes depend on workload but typical ~16-64 MiB. Cost is
  bounded by `dirty_threshold_bytes` (no flush below threshold
  unless interval ticks). Worst-case: a session with sustained
  write rate at threshold flushes every interval. PG write rate
  ~3 UPDATE/s for `update_live_disk_manifest` — trivial today.
- **Sandbox-rebind race surface area.** A stale `publish_live_manifest`
  from a destroyed sandbox could clobber a fresh `sandbox_id`'s
  pointer. Mitigated by the `sessions.sandbox_id == publish.sandbox_id`
  guard in the UPDATE query; must be exercised by an explicit test.
- **GC false-positive blast radius.** Deleting a still-referenced
  chunk = unrecoverable session. Three independent safety nets:
  the 24h grace period, the chunk_generation barrier, and the
  dry-run admin endpoint. Plan: prod stays in
  `ENGRAM_CHUNK_GC_ENABLED=0` for ≥1 day after Phase B ships, then
  dry-run for ≥1 week before enabling sweeps.
- **Heartbeat-lane contention.** The new `POST /internal/live-manifest`
  uses the same host→coord channel as heartbeats. A live-manifest
  burst could head-block heartbeats → false dead-host. Mitigation:
  separate route + a small bounded mpsc per host so the two lanes
  don't share queue time. Instrument from day one.

### Explicit non-goals

- **Migration / evacuation primitive** — deferred to M4.1 of ADR
  0015. The diagnostic surface + continuous sync + GC pin-set this
  ADR builds are the preconditions.
- **Continuous memory sync** — requires VMM-level dirty page
  tracking or live-migration support that FC doesn't expose. Memory
  RPO stays snapshot-bounded; the diagnostic surface makes this
  visible.
- **Periodic auto-snapshot of Active sessions** — would bound memory
  RPO at the cost of guest-visible pauses. Worth a follow-up design
  once we have COW data to motivate the cadence.
- **OpenAPI codegen** — TS types stay hand-maintained per current
  pattern (`web/src/types.ts` mirrors Rust). Flag as a future
  hygiene pass once the diagnostic surface stabilizes.
- **Per-host chunk-cache eviction policy changes** — out of scope;
  the NVMe cache LRU stays as-is.

---

## Rollout

Each phase ships in its own commit chain. Phase A is independently
useful (it adds visibility without changing behaviour); Phase B
without Phase C grows BlobStorage unboundedly until the GC turns on;
Phase C is the safety net that makes Phase B operationally sane.

Production rollout (gated by env vars):

1. Ship Phase A. Observe diagnostic endpoints in prod for ≥1 day with
   no behaviour change.
2. Ship Phase B with `ENGRAM_CONTINUOUS_FLUSH_DISABLED=1`. Verify the
   code path works in staging.
3. Enable continuous flush in prod. Observe BlobStorage growth
   (PUT rate, total size) for ≥1 day. Without GC, growth is
   monotonic — this is the expected window before GC turns on.
4. Ship Phase C with `ENGRAM_CHUNK_GC_ENABLED=0`. Verify the candidate
   table populates correctly via the dry-run admin endpoint.
5. Enable dry-run sweeps in prod; spot-check candidate set against
   known-live sessions for ≥1 week.
6. Enable sweep deletes. Watch BlobStorage shrink.

Phase boundaries are the natural points to flip this ADR's status:
**Proposed** → annotated with Phase A commits → annotated with Phase
B commits → **Accepted** once Phase C lands and prod is in
steady-state sweep mode.

---

## Open questions

- **`ChunkCache::has` cost.** Depends on whether the cache index is
  hash-indexed (O(1)) or list-walked (O(n)). Phase A commit 2 will
  verify; if O(n), add an LRU-keyed HashSet beside it.
- **Host→coord channel shape.** This ADR sketches the live-manifest
  callback as HTTP POST (reusing the heartbeat channel). If today's
  heartbeat is gRPC in both directions, prefer the gRPC route for
  consistency. Phase B commit 8 will confirm.
- **PG write rate at 10× session density.** ~3 writes/s is trivial
  today; flag for re-tuning if session density grows 10×. Worth
  measuring at the end of Phase B before enabling Phase C.
- **Whether the generation barrier alone is sufficient.** The barrier
  catches mid-sweep races, but a sweep that finishes one instant
  before a flush starts could still mark a chunk-about-to-be-pinned
  as a candidate. The 24h grace period covers this; the barrier is
  defence-in-depth. If we observe candidate churn from this in prod,
  add a "candidate refresh" pass that re-checks against the pin set
  before promoting.
