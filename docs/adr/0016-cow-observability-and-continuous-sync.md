# ADR 0016: COW observability and continuous disk sync

Status: 2026-05-24 — **Proposed**, Phase A + Phase A.1 shipped
(§A.1.1–§A.1.7; §A.1.8 reconsidered, not pursued). Phase B
(continuous disk sync) and Phase C (chunk-GC pin-set redesign)
remain pending; the ADR will flip to Accepted once Phase C ships.
Supersedes ADR 0015 M5 "Known regression — chunk-store GC deleted"
(see `docs/adr/0015-system-design-v2.md` §"Known regression —
chunk-store GC deleted") once Phase C lands.

Phase chain (filled in as commits land — same shape as ADR 0014's
M1.x annotations and ADR 0015's per-milestone commit lists):

- **Phase 0** — this ADR, in Proposed status. Commit `eadbd03`.
- **Phase A** — observability surface. Four commits + this update:
  `050b189` (ChunkedDiskBackend accessors), `335f14a` (CowState
  types + trait + gRPC + PooledBackend impl, absorbed the planned
  commit 2), `285adf0` (coord endpoints + 1s-TTL cache), `7324baa`
  (web app integration). **Shipped 2026-05-23.**
- **Phase A.1** — prod follow-ups surfaced by the COW diagnostic
  during a spot-check on session `96392fd3-…` (2026-05-23). Four
  separate code paths, one TF cleanup. Each its own commit; see
  §"Phase A.1 follow-ups" below for the catalog and proposed fixes.
  _(in flight)_
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

## Phase A.1 follow-ups (2026-05-23)

Phase A's COW diagnostic, opened in prod against session
`96392fd3-1d31-468c-b8db-5f9b125226db`, surfaced four real bugs
that have nothing structurally to do with Phase A but were
**invisible until the diagnostic surface existed**. This is exactly
the kind of payoff the observability work was meant to enable — so
catching them here, scoping them as Phase A.1, and fixing them
in-session before Phase B is the right shape. Each is its own
commit; sequencing chosen so observability work lands first and
makes the rest easier to diagnose.

The session was created on `engrams-fc-3drl` at 22:37:17 and showed
"flush 2s ago" in the web UI even though no `snapshots` row was ever
recorded in PG for that session. Pulling the thread led to all four
of the items below.

### Fix order

1. **Observability first** — add `tracing::info!` at the top of
   `idle_evictor::evict_idle_session` and at the host-pushed
   candidate handler. Today neither logs entry; the silence let
   Issues 2/4 hide for an unknown duration. Smallest change, gives
   us data for the rest.
2. **VM-not-resumed-after-failed-snapshot** — the user-facing one.
   A second prompt to the session at 22:40:03 never produced a
   `run_started` event. Hypothesis traced below.
3. **idle-eviction transport error vs heartbeat success** — both
   POSTs use the same `reqwest::Client`; only one fails. With
   the new logs from Step 1 we'll know whether coord receives the
   eviction POST partially or not at all.
4. **`ws://` scheme in fc-host-mig TF** — pure tech-debt
   alignment. Lowest blast radius; do last.

### A.1.1 — silent `evict_idle_session` (observability gap)

**Evidence**: 3 `chunked NBD disk flushed` lines on the host
between 22:38:45 and 22:39:44. Each flush is only emitted from
`PooledBackend::snapshot`, which means coord called
`host.snapshot()` three times. But coord pod
`engrams-coordinator-5c5ff978bf-r6lvp` shows zero log lines for
this session between 22:37:09 and 22:46:40 — no
`evict_idle_session`, no `host-pushed idle eviction failed`, no
gRPC dispatch trace.

**Root cause**: `crates/engram-coordinator/src/idle_evictor.rs:32`'s
`evict_idle_session` has no entry-level log. The downstream `Err`
arm in `api/host_http.rs:447-455` logs a WARN on failure, but
nothing logs on entry or on success — so a coord that's snapshotting
periodically without recording PG rows is structurally invisible.

**Fix**: add a `tracing::info!(session_id, sandbox_id, "idle eviction
pipeline started")` at the top of `evict_idle_session`, and a
matching `tracing::info!` at the success completion. The shape
mirrors the existing logs in `host_http::idle_eviction_candidates`
post-call. ~5 lines of code; no behaviour change.

### A.1.2 — VM left paused on tokio cancellation between pause and resume

**Evidence**: PG events for the session:

```
22:38:09 harness_idle
22:40:03 agent_message (role=user) — user's second prompt
   — no run_started, no agent_message follow-up —
22:44:13 (host log) agentd reports ttyd ready  ← user opened shell
22:46:40 (coord log) shell proxy bridge ended  ← user gave up
```

The user's second prompt landed on coord at 22:40:03,
`POST /sessions/:id/prompt` succeeded (the user-role
`agent_message` event was emitted, which is the side effect at the
end of `api/prompt.rs::prompt`), but the harness never started a
new run.

**First-pass hypothesis (rejected on closer reading)**: that
`PooledBackend::snapshot`'s `self.inner.snapshot(id).await?` (line
1380) failed after pausing without resuming. Wrong: the FC client
at `crates/engram-sandbox-firecracker/src/client.rs:181-205`
already handles this — `create_snapshot` does pause → PUT → resume
and runs the resume *unconditionally* via a `let resume_res =
self.resume().await;` after the create attempt, before propagating
either error. On Err from PUT, the VM is still resumed.

**Actual root cause**: **tokio cancellation between pause and
resume**. If the outer future driving `create_snapshot` is dropped
mid-await (gRPC RPC timeout, coord-side `tokio::time::timeout`,
task abort, panic), the pause-PUT-resume sequence is interrupted.
The PUT future is dropped; the resume call never reaches its
`.await`. Async Drop can't run async cleanup, so the VM stays
Paused. agentd inside the VM continues running off its prior
state (its control vsock survives because vsock channels persist
across pause), but anything that depended on the kernel scheduling
forward — including the harness adapter that needs to wake up and
read the new prompt — wedges.

This is the classic "async cancellation gap" that async-Drop or a
detached-resume-on-Drop pattern closes.

**Fix**: introduce a `ResumeOnDrop` guard in `FirecrackerClient`:

```rust
struct ResumeOnDrop {
    client: FirecrackerClient,
    armed: bool,
}
impl Drop for ResumeOnDrop {
    fn drop(&mut self) {
        if !self.armed { return; }
        // Can't await in Drop. Spawn a detached resume task —
        // the VM will be unpaused even if our future was dropped.
        let api = self.client.clone();
        tokio::spawn(async move {
            if let Err(e) = api.patch_vm_state(VmState::Resumed).await {
                tracing::warn!(error = %e, "ResumeOnDrop: detached resume failed");
            }
        });
    }
}
```

Then `create_snapshot` becomes:

```rust
self.pause().await?;
let mut guard = ResumeOnDrop { client: self.clone(), armed: true };
let create_res = self.put("/snapshot/create", &body).await;
let resume_res = self.resume().await;
guard.armed = false;  // happy path: defuse; we just resumed inline
create_res?;
resume_res?;
Ok(...)
```

On cancellation between `pause` and the explicit `resume`, the
guard's Drop fires (it's stack-local; cancellation drops it), the
detached task resumes the VM. On the happy path, we defuse the
guard after the inline resume so we don't double-resume.

Plus a unit test that constructs the client against a controllable
mock FC API, races `create_snapshot` against a cancellation, and
asserts the mock saw exactly one `pause` + one `resume`.

**Caveat**: `FirecrackerClient` needs `Clone` for this. It's a thin
wrapper over an HTTP unix-socket client; cheap to clone.

### A.1.3 — idle-eviction POST transport error vs heartbeat success

**Evidence**: heartbeat (`POST /api/hosts/:id/heartbeat`) succeeds
— PG `hosts.last_heartbeat_at` is fresh. idle-eviction
(`POST /api/hosts/:id/idle-eviction-candidates`) on the *same*
`CoordClient` instance fails every time with:

```
transport error: error sending request for url
  (http://10.10.0.2:8080/.../idle-eviction-candidates)
```

A manual curl from the same host with the same bearer token to
the same URL returns 200. The bug is somewhere in how reqwest's
HTTP/2 connection pool reuses connections across multiple
request paths.

**Hypothesis**: `coord_client.rs:46-56` uses
`pool_idle_timeout(90s)` on a shared `reqwest::Client`. Coord pods
roll on every push (helm-deploy from CI); the internal LB
(`10.10.0.2`) is stable but the back-end pod IP changes per roll.
A pooled H2 connection bound to a now-gone pod can fail when
reused. Heartbeat fires every 5s, gets reset/re-establishes
quickly; idle-eviction fires less often and is more likely to draw
a stale connection.

Open question: why is heartbeat resilient and eviction not? Same
client, same pool. Maybe heartbeat's smaller payload hits a happy
path; maybe reqwest's connection reuse policy plays differently
with the two URLs.

**Fix shipped**: tighten `pool_idle_timeout` on the shared
`reqwest::Client` from 90s to 10s
(`crates/engram-host-agent/src/coord_client.rs:CoordClient::new`).

The cadence math justifies the value:
- heartbeat fires every 5s, so a pooled connection stays warm
  (re-uses on the next tick before the 10s timer expires).
- harness_event fires per in-VM event, often sub-second during an
  active run; pool stays warm during activity.
- idle-eviction-candidates fires every 30s+ (the host's
  `DEFAULT_POLL_INTERVAL` is 10s but the soft-TTL gate before a
  POST happens ≥ 30s after harness_idle); pool entry is always >
  10s old → always opens a fresh TCP. The stale-connection class
  becomes structurally unreachable on this lane.

Per-request overhead: one TCP connect over the internal LB
(~5-10ms). Negligible against the workload.

Also added: a `tracing::debug!` in `push_idle_eviction_candidates`
that logs the reqwest `Error` kind (`is_connect`, `is_timeout`,
`is_request`, `is_body`) on transport-level failures. The
caller's `lib.rs::eviction_task` already logs the higher-level
error; this is the per-class breakdown that lets us distinguish
"stale pool" (is_connect=true) from "DNS" / "request body" / etc.
in future investigations.

A.1.1's `evict_idle_session` entry log on the coord side is the
companion data point — together they let an investigator see
whether a given POST attempt reached coord (entry log + completed
log appear) or never made it past the host (transport-error log
on host, nothing on coord).

**Validation plan** (no prod hotfix): dev-vm can approximate this
by killing+restarting the local coord mid-session. GCP LB
connection-reset on backend roll isn't reproducible on localhost
(no LB in the loop), but the "stale TCP after server restart"
mechanic is the same, and the 10s timeout means recovery is
bounded.

### A.1.4 — `ws://` scheme in fc-host-mig TF

**Evidence**: `engrams-internal/modules/engrams/main.tf:125`:

```hcl
coordinator_endpoint = "ws://${google_compute_address.coord_internal.address}:${local.coordinator_port}"
```

…written to `/etc/engram/host-agent.env` as
`ENGRAM_COORDINATOR_ENDPOINT=ws://10.10.0.2:8080`. The host-agent's
`coord_client.rs::trim_ws_suffix` rewrites `ws://` → `http://` for
reqwest (which rejects WS schemes outright), so this *works* — but
it's pre-ADR-0013 framing in a post-ADR-0013 world. The variable
description (`engrams/deploy/terraform/gcp/modules/fc-host-mig/variables.tf:87`)
still says "ws:// or wss:// URL the host-agent dials. Internal LB
or service mesh entry; never the public ingress."

**Fix shipped**: clean drop, no compat shim. There are no other
deployments using this module path; preserving the
`trim_ws_suffix` helper for backwards compatibility would just
add code that future readers have to wonder about.

Three changes across both repos:

1. `engrams/deploy/terraform/gcp/modules/fc-host-mig/variables.tf`
   — variable description updated to `http://` / `https://` only;
   call out that non-HTTP schemes will be rejected at the first
   request.
2. `engrams/crates/engram-host-agent/src/coord_client.rs` — drop
   `fn trim_ws_suffix` and its three unit tests; replace the
   `base_url: trim_ws_suffix(&coord_url)` with a direct
   `coord_url.trim_end_matches('/').to_string()`. `reqwest::Client`
   rejects non-HTTP schemes at construction with a clear "builder
   error for url".
3. `engrams-internal/modules/engrams/main.tf:125` — `local.coordinator_endpoint`
   changed from `ws://...` to `http://...`.

**Sequencing for prod rollout** (no hotfix, just operator-mediated
order): the engrams-internal TF change must apply (terraform apply
+ MIG roll) before the engrams host-agent change is deployed.
Otherwise a freshly-pulled host-agent will receive the stale
`ws://` env, fail to construct its `reqwest::Client`, and crash
loop. Order:

1. Merge engrams-internal commit; `terraform apply`; wait for MIG
   to roll all FC hosts (new instance template version → new env
   file → new host-agent process reads `http://`).
2. Merge engrams commit; CI builds + pushes new images; coord rolls
   on the next helm-deploy; future host-agent images (next MIG
   refresh) pick up the shim-removed binary.

Between steps 1 and 2 the prod hosts are running the OLD shim
binary against `http://` env — works fine, shim is a no-op on
HTTP schemes. After step 2 they're running the NEW no-shim binary
against `http://` env — also fine. No window of incompatibility.

### A.1.5 — idle-eviction retry storm

**Evidence (prod, session `6b8e5d83-fcbf-42d7-af25-8c822c4a026d`,
2026-05-24)**: A.1.1's new entry log fired 6+ times for the same
sandbox over ~3 minutes with zero `pipeline completed` lines. The
host's `idle eviction POST` failed every ~30s with `transport
error` (`is_timeout=true`, not `is_connect=true` — so A.1.3 was
*not* the culprit). The session stayed `Active`; a second user
prompt sent during the retry storm went nowhere — the in-VM
harness adapter wedged from cumulative FC pause/resume cycles
each time a new `evict_idle_session` aborted the prior snapshot
via `inflight_snapshots`.

**Root cause**: the shared `CoordClient`'s 30s `reqwest` timeout
is tighter than the worst-case `evict_idle_session` pipeline
duration (memory dump + state.bin upload + chunked memory upload
+ PG inserts can run 60–90s on a session with cold blob storage).
The host's tick loop awaited the POST synchronously; on timeout
it un-set the in-flight bookkeeping (nothing held it across the
timeout boundary) and the *next* tick picked the same idle
sandbox up again. Each retry minted a fresh `evict_idle_session`,
which `inflight_snapshots`-aborted the in-progress one. Pipeline
never reached `record_snapshot`. The eviction was structurally
incapable of completing.

**Fix shipped** (two commits, sequenced):

#### A.1.5a — host-side in-flight gate + fire-and-forget POST + 120s per-request timeout

Three coupled changes in `engram-host-agent`:

1. `HarnessHub::eviction_inflight: Mutex<HashMap<SandboxId, Instant>>`.
   `idle_sandboxes()` now filters any sandbox already present.
   Public methods: `mark_eviction_inflight`,
   `clear_eviction_inflight`, `sweep_stale_evictions(max_age) ->
   Vec<SandboxId>`.
2. The eviction tick (`lib.rs`) marks every candidate, then
   `tokio::spawn`s the POST and returns. The spawned task's
   finally block unconditionally clears each marker (success /
   transport error / timeout). The tick is no longer blocked by
   the POST. A stale-entry sweep at the top of each tick reaps
   markers older than **180s** with `tracing::warn!`, so a
   wedged spawn can't permanently block re-eviction.
3. `coord_client.rs::push_idle_eviction_candidates` gets a
   per-request `RequestBuilder::timeout(120s)` override. The
   shared client's 30s default stays right for heartbeat /
   harness_event. 120s covers worst-case pipeline duration with
   a 30s margin; the host-side gate suppresses the storm even
   if a POST does time out.

Three new unit tests in `harness::tests`:
- `idle_sandboxes_skips_evictions_in_flight`
- `sweep_stale_evictions_reaps_old_entries`
- `mark_eviction_inflight_is_idempotent`

Commit: `21be0c3`. 763/763 workspace tests pass.

#### A.1.5b — coord-side re-entry guard

Belt-and-suspenders for `evict_idle_session`. New
`AppState::inflight_evictions: Arc<DashMap<SessionId, ()>>` and an
RAII `InflightEvictionGuard` at the top of the pipeline:

- `try_acquire` returns `None` if another `evict_idle_session`
  call for the same session is already in flight **on this coord
  pod**. The second caller returns `Ok(())` with an `info!` log
  ("pipeline already in flight on this coord"). The first caller
  owns the work.
- Drop removes the entry — covers success, error, and panic
  paths uniformly.

New unit test
`evict_idle_session_reentry_guard_serializes_concurrent_calls`:
holds the guard manually, asserts the second call is a no-op
(session stays Active, registry not unbound), drops the guard,
runs the pipeline for real, asserts Drop releases on success.

#### Known scope limitation — A.1.5b is per-pod (deferred §A.1.5c)

`inflight_evictions` is in-process per coord replica. With a
single coord replica (today's prod), this is sufficient. With
N replicas the residual race is:

- Pod A receives the host's POST, enters the pipeline.
- The host's *next* tick (rare under A.1.5a's in-flight gate,
  but possible if the spawned task panics and the 180s sweep
  fires) round-robins to pod B.

Cross-pod protection then falls to three existing layers:

1. **Host-side eviction-inflight gate (A.1.5a)** — the primary
   defence. The same host can't emit two POSTs for the same
   sandbox no matter where they land.
2. **Host's `inflight_snapshots` map** (ADR 0014 #1/#2) —
   serializes `host.snapshot(sandbox_id)` at the host. One pod
   wins; the other sees a conflict.
3. **`registry.get(session_id)` guard** at the top of
   `evict_idle_session` — once the winning pod unbinds the
   sandbox, every subsequent caller no-ops.

The residual window is between `host.snapshot()` starting on
pod A and pod A's `registry.unbind()` — a second pod can pass
the registry guard and enter the pipeline. The host's snapshot
serializer prevents double-uploading; pod B then tries
`record_snapshot` / `transition_session`, racing pod A's PG
writes. Most PG ops are idempotent or conflict-rejecting, but
it's untidy.

**Deferred follow-up — §A.1.5c (cross-replica eviction guard)**:
~~when multi-replica coord ships~~ → **promoted to immediate
scope 2026-05-24** after a session-validation discussion. The
DashMap is correct-but-narrow; rather than re-doing this when
we scale replicas, A.1.5c lands in the same M4 milestone. See
§A.1.5c below for the design.

**Commits**: A.1.5a `21be0c3`; A.1.5b `209c5dc`.

### A.1.5c — cross-replica eviction guard (PG leasing row)

**Evidence**: 2026-05-24 validation discussion. The per-pod
A.1.5b guard works for today's single-replica prod but leaves
a gap once we scale: a second coord pod can enter
`evict_idle_session` past the (also per-pod) `registry.get()`
guard while the first pod is still in `host.snapshot()`, then
race on `record_snapshot` / `transition_session` writes. Most
PG ops are idempotent or conflict-rejecting so end state stays
correct, but it's untidy and exactly the class of bug we
shouldn't be filing twice.

**Design — eviction_inflight leasing row**:

New PG table:

```sql
CREATE TABLE eviction_inflight (
    session_id   UUID PRIMARY KEY,
    locked_by    TEXT NOT NULL,           -- coord pod name / hostname
    locked_at    TIMESTAMPTZ NOT NULL,
    sandbox_id   UUID NOT NULL            -- for diagnostics
);
```

At the top of `evict_idle_session`:

```rust
let leased = state.services.meta
    .try_acquire_eviction_lease(session_id, sandbox_id, pod_id())
    .await?;
let _guard = match leased {
    Some(g) => g,                          // RAII; release on drop
    None => {
        tracing::info!(... "eviction skipped: lease held by another pod");
        return Ok(());
    }
};
```

`try_acquire_eviction_lease` is `INSERT INTO eviction_inflight
(session_id, locked_by, locked_at, sandbox_id) VALUES (...)
ON CONFLICT (session_id) DO NOTHING RETURNING locked_by`.
Returns `Some(EvictionLease)` if the row was inserted, `None`
if a conflict (another pod holds it). The returned guard's
`Drop` impl spawns a background task that runs `DELETE FROM
eviction_inflight WHERE session_id = $1`.

Choice of leasing row over `pg_try_advisory_lock`:

1. **Connection-pool friendly**. `pg_advisory_lock` is
   connection-scoped; with sqlx's pool the acquire and release
   must run on the *same* pooled connection, which is fragile
   (the pool can hand the connection out between acquire and
   release). A row is global state.
2. **Debuggable from PG**. `SELECT * FROM eviction_inflight`
   tells an operator which session is mid-eviction on which pod.
   Advisory locks are visible only via `pg_locks`, which is
   noisy.
3. **Reusable shape**. The same pattern fits future
   one-at-a-time-per-session operations (resume, drain, evac).
4. **Stale-lease reaper**. A background task can sweep entries
   older than N minutes with a warn log, analogous to A.1.5a's
   180s sweep. Advisory locks have no comparable hook.

Stale-lease policy: any row older than **180s** (same threshold
as A.1.5a's host-side sweep) is reaped by a coord-side
background task with a `tracing::warn!` carrying `locked_by`
and `locked_at`. Pipeline duration is observed at 60-90s today,
so 180s is a generous margin without leaking forever.

**Replaces**: `AppState::inflight_evictions: DashMap` from
A.1.5b. The DashMap deletion lands in the same commit as the
leasing-row introduction so there's no period where both exist.

**Unit + integration tests**:

- Unit: trait-level `try_acquire_eviction_lease` on `MiniMeta`
  with an injected conflict; assert second caller returns
  `None`, first caller's RAII drop releases.
- Integration (Postgres-gated): two concurrent
  `evict_idle_session` calls against the same session; one
  runs the pipeline, the other early-returns. Verify
  `eviction_inflight` is empty after both complete.
- Stale-lease reaper unit test with a fake clock.

### A.1.6 — reconciler/eviction race in `evict_idle_session`

**Evidence (prod, session `1edf09a3-8d5c-4295-a9f8-8676787c2474`,
2026-05-24)**: with A.1.5a + A.1.5b deployed, the host-side
retry storm was closed (single `pipeline started` log).
However the pipeline never reached the matching `pipeline
completed` log; instead, ~100s later the reconciler (ADR
0009) logged `orphaned session moved through HostLost
final_state=Idle recoverable=true` and the session reached
Idle via the recovery path.

**Root cause**: the eviction pipeline ordering in
`evict_idle_session`:

```
1. snapshot()             ← 60-90s
2. record_snapshot()
3. registry.unbind()
4. destroy()              ← host removes sandbox from running_sandboxes
5. assign_session_sandbox(None)
6. transition_session(Idle)  ← PG status flips
7. emit events
8. commit_snapshot()
9. log "pipeline completed"
```

Between step 4 and step 6, the host's NEXT heartbeat (every
5s) carries an empty `running_sandboxes` for this session.
The reconciler runs on every heartbeat in `api/hosts.rs`,
observes `session.sandbox_id IS NOT NULL` AND `host.running_sandboxes`
does not contain that sandbox → marks the session as orphaned,
runs `HostLost → Idle` because a recoverable snapshot row
exists. The pipeline's eventual `transition_session(Idle)`
then fails (state machine rejects Idle→Idle), the function
bubbles `EvictError::Meta`, and `abort_inflight_snapshot`
fires spuriously.

End state is correct (recoverable snapshot, session is Idle)
but reached via a side-door path, with misleading logs and a
wasted abort_snapshot RPC.

**Fix — reorder the pipeline**: do `transition_session(Idle)`
BEFORE `destroy()`. Reconciler's existing guard already skips
sessions whose status is non-Active (the orphan detector keys
on Active sessions only — non-Active means "another path is
handling lifecycle"), so once Idle is committed to PG, the
reconciler will no-op on its next pass.

New ordering:

```
1. snapshot()
2. record_snapshot()
3. registry.unbind()
4. assign_session_sandbox(None)
5. transition_session(Idle)  ← reconciler now sees Idle and no-ops
6. destroy()                  ← can run after; host running_sandboxes drain is benign
7. emit events
8. commit_snapshot()
9. log "pipeline completed"
```

**Why this is safe**:

- `destroy()` already runs best-effort with a warn log on
  failure; moving it after the PG state flip doesn't change
  failure semantics.
- The host's `running_sandboxes` and the PG `sessions.sandbox_id`
  diverging briefly between step 5 and step 6 is exactly the
  state the reconciler is designed to ignore (status≠Active).
- Failed `destroy()` post-transition is a host-local leak;
  the host's `orphan_reap` background task (existing) cleans
  up.

**Unit test**: extend the existing
`evict_idle_session_runs_full_pipeline` test with an injected
reconciler-style "set status Idle after step 3" callback;
assert the eviction pipeline still reaches its completion log
and does NOT call abort_snapshot.

### A.1.7 — egress `SessionEgressPolicy` not rebuilt on resume

**Evidence (prod, session `1edf09a3`, 2026-05-24)**: after a
clean resume from snapshot (`af98f6fb` succeeded `8694b431`),
the in-VM harness's DNS queries to `api.anthropic.com` were
denied with `reason=UnknownGuest`. After ~3 minutes of retries,
the harness gave up and emitted `"API Error: Unable to connect
to API (ConnectionRefused)"` as its assistant response.

**Root cause** (already commented in
`crates/engram-coordinator/src/api/snapshot.rs:441-449`):

> "Pre-existing gap (not introduced by ADR 0013): resume
> doesn't re-apply the SessionEgressPolicy on the new sandbox's
> host... the bundled `start_agent` here passes an
> unspecified-IP policy that the host treats as 'no policy
> applied'."

`resume_from_fc_snapshot` builds a `placeholder_policy` with
`guest_ip: Ipv4Addr::UNSPECIFIED` and empty allow-lists, passes
it to `start_agent`. The host-side `notify_session_policy` does
`Registry::register(SessionState { guest_ip: 0.0.0.0, ... })`,
keying the entry under `0.0.0.0`. The actual restored guest IP
(`10.200.0.10`, inherited from the snapshot) has no entry in
the registry → all DNS queries deny with `UnknownGuest`.

**Fix — extract `build_resume_egress_policy` helper**:

New helper in `crates/engram-coordinator/src/api/sessions.rs`
next to `resolve_manifest_secrets`:

```rust
pub(crate) async fn build_resume_egress_policy(
    state: &SharedState,
    session: &Session,
    new_sandbox_id: SandboxId,
) -> Result<Option<SessionEgressPolicy>, ApiError>
```

It does:

1. Look up `enabled_images` by `session.image`. If absent,
   return `Ok(None)` (resume continues with the existing
   placeholder; dev / pre-fix sessions stay on the status quo).
2. Parse the cached `manifest_toml`.
3. Resolve manifest secrets via `services.secrets.resolve(...)`
   — same call `resolve_manifest_secrets` uses, but we keep
   the `SecretBundle` instead of folding into env (we need the
   schema's per-secret `allow_hosts` / `allow_host_patterns`).
4. Layer per-request overrides via `load_session_secrets` and
   reconcile against the bundle's placeholder mapping.
5. `state.services.host.guest_ip(new_sandbox_id).await` →
   parse as `Ipv4Addr`. If absent/unparseable, return `Ok(None)`.
6. Assemble `SessionEgressPolicy { guest_ip, session_id,
   sandbox_id, network_allow_hosts, network_allow_host_patterns,
   secrets, secret_mode }`.

In `resume_from_fc_snapshot`, replace the placeholder block:

```rust
let policy = match build_resume_egress_policy(&state, &session, new_sandbox_id).await {
    Ok(Some(p)) => p,
    Ok(None) => /* existing placeholder; no enabled_images / no IP */,
    Err(e) => {
        tracing::warn!(... error=%e, "egress policy rebuild failed; resume continues with placeholder");
        /* existing placeholder */
    }
};
state.services.host.start_agent(new_sandbox_id, agent, policy).await?;
```

**Why error → placeholder, not bubble**: a transient secret-
store hiccup or enabled_images-row-gone-missing shouldn't block
a resume that would otherwise work. The harness will see an
empty allow-list (deny-by-default), the user can re-`/resume`
after operator fixes the root cause. Honest failure mode.

**Unit test**: `MockHost::guest_ip` returns a known
`10.200.0.7`; `MiniMeta` returns a synthesized `enabled_images`
row with a fake manifest; in-memory `SecretStore` returns
test values. Call `build_resume_egress_policy`, assert the
returned `SessionEgressPolicy` carries the real IP + the
manifest's allow-lists + the resolved secret placeholders.

**Manual prod-ops verification (post-merge)**: create session,
let it idle, send a follow-up prompt. Tail host logs filtered
to `egress bypass complete sni=api.anthropic.com` — the line
should fire on the post-resume API call (didn't on session
`1edf09a3`'s second prompt). Confirm via session_events that
the resume's `run_completed` carries a real assistant response,
not `ConnectionRefused`.

### A.1.8 — egress `Registry` refactor — **reconsidered, not pursued**

**Original motivation** (kept for the audit trail): A.1.7 looked
like a "must remember to register at this step" bug, and the
structural reason it seemed possible was that `Registry` keys
policy entries by `guest_ip`. The proposal was to split into a
`policies` map keyed by `session_id` + a thin `ip_index` keyed
by `Ipv4Addr`, so resume would only need to update `ip_index`.

**Why dropped (decided 2026-05-24, post-A.1.7 ship)**:

1. **Registry already keeps a `by_session` index**. Reading
   `crates/engram-egress-proxy/src/registry.rs:50-96`,
   `Registry::register()` already maintains `by_session:
   HashMap<SessionId, Ipv4Addr>` and explicitly replaces any
   prior entry for the same session_id when called again —
   removing the stale guest_ip mapping in the same critical
   section. The "stale IP left behind after rebind" failure
   mode the refactor was designed to prevent is **already
   impossible** in the current code.

2. **Root cause was the wire format, not the storage shape**.
   The A.1.7 bug was that `resume_from_fc_snapshot` constructed
   a `SessionEgressPolicy { guest_ip: Ipv4Addr::UNSPECIFIED }`
   and passed it through `start_agent` →
   `notify_session_policy` → `Registry::register`. The
   registry registered exactly what it was told: an entry under
   `0.0.0.0`. Splitting the registry into `policies` +
   `ip_index` doesn't change the wire format — anyone
   constructing a `SessionEgressPolicy` with a stale IP would
   still produce the same bug. The fix has to live at the
   policy-construction site (where A.1.7 fixed it).

3. **The optimization is real but not load-bearing**. Sending
   the full policy body on every resume (vs. once-per-session-
   lifetime + an `ip_rebind` callback) is a wire-volume
   optimization, not a correctness improvement. Resume rate is
   measured in tens per minute per coord pod; the body is
   sub-kilobyte. Not worth a wire-format break.

4. **The next-best regression guard already exists**. A.1.7
   ships with two unit tests (`assemble_resume_egress_policy_*`)
   that lock in the policy assembly. A future regression would
   show up there, not in a registry shape test.

**Filed instead as deferred A.1.8b — resume-path integration
test** (Phase B-or-later scope, not blocking M4 close):
coord-side test that asserts the resume hot path queries
`host.guest_ip(new_sandbox_id)` and the policy passed to
`start_agent` carries that IP, not `0.0.0.0`. Higher-value
than the registry refactor; cheaper to write.

**No code change for A.1.8 in this milestone.** Original
proposal kept above in this section's history (commit `5fa045c`)
for future readers who hit a related class of bug.

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

**Migration number bump**: Phase B's migration was originally
spec'd as `0033_sessions_live_disk_manifest.sql`. After
A.1.5c shipped `0033_eviction_inflight.sql` (2026-05-24),
Phase B's migration must take `0034_` (or whichever the next
free number is when Phase B opens). Renumber + rename in the
Phase B commit chain — do not leave a duplicate `0033`.

#### Phase B failure mode to close: resumed sandboxes don't rejoin chunked-disk tracking

**Surfaced 2026-05-24 on session 8588dc5c** (post-Phase-A.1
validation). Two visible symptoms, one root cause.

**Symptom 1 — COW diagnostic silent**: `GET /sessions/:id/cow-state`
returned `state: null` for an Active session that had just
resumed from snapshot. Web UI rendered the misleading copy
"no live sandbox — disk-tier diagnostic unavailable while the
session is idle / lost / pending."

**Symptom 2 — second eviction can't snapshot**: after the
session's first eviction+resume cycle completed cleanly, the
host's `idle_evictor` tick fired every 10s for the next 10+
minutes, each POST returning `accepted=0 failed=1` with coord
warn-logging:

```
host-pushed idle eviction failed ... error=idle evict sandbox:
sandbox vm error: grpc Internal error: snapshot error:
non-canonical jail layout: rootfs canonical symlink missing
at /var/lib/engram/sandboxes/rootfs/<sandbox_id>.dev: No such
file or directory (os error 2)
```

The session stays Active permanently (until host roll or
session terminate) — no data risk, no resource leak, but
log-noisy and operationally surprising. A.1.5a's host-side
gate doesn't suppress this because the failure is fast
(~1ms): the spawned POST finishes immediately, gate releases,
next 10s tick re-emits.

**Root cause**: `cow_state_all` on the host-agent iterates
`PooledBackend::nbd_sandboxes`, which is populated only by the
cold-create path (`pooled_backend.rs:1284` — `nbd_sandboxes.insert`
inside the NBD-attach branch of `create()`). The snapshot
pipeline reads the same nbd-disk path expectations (the
canonical rootfs symlink at
`/var/lib/engram/sandboxes/rootfs/<sandbox_id>.dev`). The
resume path restores via FC's snapshot API into a sandbox
that doesn't go through the NBD-attach branch, so:
(a) `nbd_sandboxes` stays empty for the resumed sandbox →
cow_state_all returns nothing for it; (b) the canonical
chunked-disk layout the snapshot path expects doesn't exist
→ second eviction can't snapshot. Both symptoms collapse to
"resume forgot to rejoin chunked-disk tracking".

**Phase B fix**: when `FlushScheduler` lands (commit 10 in
Phase B's chain), wire it on the **resume path** as well as
on cold-create. The two call sites are:

1. Cold-create: today wires NBD attach + `nbd_sandboxes.insert`
   at `pooled_backend.rs:~1284`. Add: spawn `FlushScheduler`
   for the new sandbox here.
2. **Resume path** (currently broken for COW state): FC
   snapshot restore returns a new `SandboxId` that today is
   not added to `nbd_sandboxes`. Phase B must: (a) ensure the
   resumed sandbox is registered with chunked-disk tracking
   (insert into `nbd_sandboxes` with a `ChunkedDiskBackend`
   that resumes from the snapshot's disk manifest, OR fall
   back to a "no-tracking" sentinel that still appears in
   `cow_state_all` with `dirty=0 / last_flush_at=null`); (b)
   spawn `FlushScheduler` for the new sandbox.

Open question for Phase B implementer: does FC's restore-from-
snapshot use the NBD machinery at all? If the restored disk
is a flat file on local disk (not chunk-NBD-backed), then
"continuous flush" doesn't apply until the disk is converted
back to chunked, which may be its own design problem. The
quick answer for the diagnostic surface: emit a record with
`dirty_chunks=0, dirty_bytes=0, last_flush_at=null` for any
sandbox the host knows about but isn't NBD-tracking. The web
UI then renders something honest ("steady-state since resume;
flush scheduler reattaches on next manifest change") rather
than the current misleading "no live sandbox" copy.

**UI copy fix to land alongside the Phase B work**: change
`web/src/components/CowState.tsx:208-209` from "no live
sandbox — disk-tier diagnostic unavailable while the session
is idle / lost / pending" to a message that conditionally
renders based on actual session status (Idle vs. Active-but-
diagnostic-not-tracked). Conditional gives a UX honest about
which state the user is in.

**Optional defensive measure independent of Phase B**: if
the chunked-disk wiring on resume turns out to be larger than
expected, ship an interim back-off in the host's
`idle_evictor` so a sandbox whose snapshot RPC failed in the
last N seconds (say 60s) skips emission. Closes the log
storm without changing correctness. Filed as a minor knob;
the Phase B root-cause fix supersedes it.

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
