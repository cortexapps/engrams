# 0084 — Capture as a durable host-owned job: `capture_jobs`, cold-base/warm-overlay split, footprint placement

Status: Accepted (2026-07-08) — P1-P4 all landed in this PR's commit chain (see the Divergence log)

Issue: #546 (2026-07 core-ops overhaul, Tier 2 epic). Builds on: ADR
0079 (the sibling durable-row + fenced-writes + scanner pattern — NOT
a build dependency, see decision 4), ADR 0080 (host-side
materialization — which already delivered this issue's original
"coordinator off the data plane" phase), #539/PR #563 (warm-hook
watchdog + streaming progress — which already closed the lease
ping-pong), ADR 0028/0038 (the checkpoint-chain diff machinery the
warm overlay rides), #531 (`fc_snapshot_version` capability probe),
ADR 0075 (the host chunk-cache single-writer the executor's writes
respect by construction).

## Problem, re-measured (2026-07-08)

Issue #546's evidence is from 2026-07-01. Two shipped changes since
then already removed the two loudest failure classes, so this ADR
records the re-measured problem, not the issue's original one:

- **Closed by #539 (PR #563):** the 12.6 h lease ping-pong. Every
  `CaptureProgress` frame (≤30 s cadence, `spawn_leg_keepalive`)
  doubles as claim renewal via
  `update_enable_job_capture_progress`; a peer can only re-claim when
  the owner's stream is dead, which is the same event the owner
  itself observes as failure. The blind renewal ticker is deleted.
- **Closed by ADR 0080:** the coordinator data plane.
  `MaterializeImage` moved GHCR→chunks onto the host;
  `materialize_disk_chunks`/`materialize_chunk_blob` are gone. The
  ADR 0078 disk floor also went into `pick_capture_host`
  (`host_disk_floor_ok`, shared verbatim with materialize).

What remains, verified against main (`fd9dc4ac`):

1. **Stream death duplicates the capture and discards finished
   work.** The host runs `build_base_snapshot` in a detached
   `tokio::spawn` (`grpc_server.rs:639-694`) — a dropped stream does
   NOT cancel it; the capture runs to completion, its terminal frame
   sent into the void. Meanwhile the coordinator classifies stream
   death as retryable (`WarmExecTransport`,
   `grpc_client.rs:619-643` → `classify_capture_error`), re-drives
   from the top next tick, and boots a SECOND capture VM — with no
   anti-affinity, no RAM accounting, and no host-side awareness that
   the first attempt is stale. A coordinator roll mid-capture
   discards an entire (possibly successful) dev-brain capture.
2. **Warm images never reuse anything.**
   `reuse_ok = config.warm.is_none()` (`enabled_images.rs:390`):
   every dev-brain re-bake pays cold kernel boot + full warm hook +
   full 8–24 GiB memory dump + full upload even when nothing
   changed. Memory chunks never dedup across captures (ADR 0036:
   boot-nondeterministic). A failed hook re-pays the cold boot too.
3. **Placement is a boolean disk floor.** `pick_capture_host`
   (`placement.rs:792-819`) has the ADR 0078 floor but: first-fit
   (not max-free-disk), no capture footprint sizing (a capture
   writes ~2×image + mem_mib + slack), no one-capture-per-host
   anti-affinity, no RAM/reservation accounting, and
   `CapabilityRequirements::default()` — it does not pin
   `fc_snapshot_version` even though the capability vector exists
   and restore placement already matches on it.
4. **No stage visibility below the enable row.** `enable_jobs` got
   `capture_phase`/`warm_stage`/`output_tail` (migration 0079), but
   in-flight state still lives in a gRPC stream; a coord roll loses
   it, and the row cannot say "host X, attempt 2, epoch 3".

## Decision

**Capture becomes a durable, host-executed, epoch-fenced job row
(`capture_jobs`), dispatched and reported over the heartbeat; the
`BuildBaseSnapshot` RPC is deleted. Capture splits into a
content-keyed cold base (boot→agentd-ready, Full snapshot) and a
warm overlay (restore base → hook → Diff snapshot), making reuse
valid for ALL images. Placement gains footprint, anti-affinity, and
FC-version pinning.**

The invariant that becomes structural: *long-running work is a
durable job with host-owned execution and epoch-fenced,
any-replica-writable state — never a connection-coupled RPC;
deadlines bound progress, never totality.*

### A. The job model

Migration `0096_capture_jobs.sql`:

```sql
CREATE TABLE capture_jobs (
    id                  UUID PRIMARY KEY,
    enable_job_id       UUID NOT NULL REFERENCES enable_jobs(id),
    image_uri           TEXT NOT NULL,
    disk_manifest       TEXT NOT NULL,      -- content-derived ManifestRef of the materialized rootfs
    image_config        JSONB NOT NULL,     -- the ADR 0080 ImageConfig (refs only; never resolved secrets)
    oci_defaults        JSONB NOT NULL,
    host_id             UUID NOT NULL,
    epoch               BIGINT NOT NULL DEFAULT 1,   -- fencing token; bumped on every reassignment
    stage               TEXT NOT NULL DEFAULT 'assigned',
        -- assigned → booting → warming → freezing → done | failed
        -- (cold-base hit path skips booting's cold-boot half; warm-less
        --  images skip warming; freezing includes the write-through
        --  flush — chunks are durable at write per ADR 0078 P1)
    stage_started_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    stage_progress      JSONB,              -- {"detail": "...", "log_tail": "..."} (units per stage)
    last_progress_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    attempts            INTEGER NOT NULL DEFAULT 1,
    retryable           BOOLEAN,            -- terminal classification, written from the host report
    error               TEXT,
    error_stage         TEXT,
    fc_snapshot_version TEXT,               -- stamped by the capturing host (NULL for VZ/Process)
    result_bincode      BYTEA,              -- CaptureJobResult on stage='done'
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE UNIQUE INDEX capture_jobs_active_enable ON capture_jobs (enable_job_id)
    WHERE stage NOT IN ('done','failed');
CREATE INDEX capture_jobs_active_host ON capture_jobs (host_id)
    WHERE stage NOT IN ('done','failed');

ALTER TABLE enable_jobs ADD COLUMN reuse_outcome TEXT;

CREATE TABLE cold_bases (
    content_key         TEXT PRIMARY KEY,
    snapshot_id         UUID NOT NULL,
    disk_manifest       TEXT NOT NULL,
    memory_manifest     TEXT NOT NULL,
    fc_snapshot_version TEXT NOT NULL,
    captured_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

- **Fencing.** Every host report carries `(job_id, epoch)`; every
  coordinator write is
  `UPDATE ... WHERE id=$1 AND epoch=$2 AND stage NOT IN ('done','failed')`.
  Failure recording is a fenced row write **any replica can
  perform** — no lease-holder identity to lose. The enable-job claim
  survives only to dedup short scanner ops; `capturing` becomes
  watch-only.
- **Dispatch/report ride the heartbeat** (ADR 0013-friendly):
  `HeartbeatAck.capture_assignments: Vec<CaptureJobAssignment{job_id, epoch}>`;
  on an unknown assignment the host calls
  `CoordClient::claim_capture_job` (HTTP
  `POST /api/hosts/:id/capture-jobs/:job_id/claim`, sibling of
  `resolve_registry_auth`) — the coordinator **resolves warm env
  refs fresh at claim time** (`resolve_capture_env`, fail-loud) and
  assembles the capture egress policy
  (`assemble_capture_egress_policy`) with a **deterministic
  synthetic session id derived from the job id** (stable across
  retries), returning `CaptureJobSpec{spec, warm, resolved_env,
  capture_egress, cold_base: Option<ColdBaseCandidate>}` over the
  authed host channel. Secrets never ride the heartbeat and are
  never at rest in `capture_jobs` (divergence from the issue's
  `capture_env_sealed` column — ADR 0080 made it unnecessary: the
  row carries refs via `image_config`; resolution is already
  ephemeral + per-attempt).
  `Heartbeat.capture_job_reports: Vec<CaptureJobReport{job_id, epoch,
  stage, progress, terminal: Option<TerminalReport>}>` — terminal
  reports re-advertised every tick until
  `HeartbeatAck.acked_capture_jobs` names them (the
  `CheckpointAdvert`/`acked_checkpoints` pattern verbatim).
  `WIRE_VERSION` 14 → 15 covers the field additions AND the RPC
  deletion.
- **Host durable record.** `CaptureJobRecord{job_id, epoch, stage,
  sandbox_id, cold_base_snapshot: Option<SnapshotId>, terminal}`
  persisted write+fsync+rename at
  `<records_dir>/capture-jobs/<job_id>.json` (the
  `CheckpointRecord::persist` pattern, torn-write-tolerant
  `load_all`). On host-agent restart: `stage <= freezing` → destroy
  the VM if alive, rewind, report the rewound stage; a terminal
  record re-advertises until acked.
- **VM ownership replaces the exempt-list.** The teardown-reconcile
  exemption becomes "sandbox_id appears in a live
  `CaptureJobRecord`" — exempt while the job lives, never forever.
  The `base_captures` DashMap is deleted. A host that receives an
  assignment for a job it's already running at a LOWER epoch
  destroys the stale attempt first — the duplicate-VM class becomes
  unrepresentable host-side, not just unlikely.
- **Per-stage progress deadlines** (coordinator scan, an arm on the
  enable scanner tick): `assigned` 60 s → re-pick + `epoch++`;
  `booting` 300 s absolute; `warming` = watchdog-driven (the #539
  stall/stage budgets; the host reports progress, the scan only
  backstops at `WarmConfig.timeout + 60 s`); `freezing` =
  `snapshot_create_timeout(mem_mib) + 60 s`. Expiry → `retryable`
  failure or reassign (`epoch++`, `attempts++`) under the attempts
  budget.

### B. Cold-base / warm-overlay split

1. **Cold base** = boot to `wait_agent_ready`, pause, **Full**
   snapshot, resume (the exact `capture_phase` sequence periodic
   checkpoints already run — verified: nothing structural blocks it
   for capture VMs; today's code simply always destroys). Content
   key:
   `sha256(disk_manifest.content_ref ‖ canonical(image_config.resources) ‖ fc_snapshot_version ‖ backend_kind)`
   — **env-agnostic** (ADR 0080's verified assumption that base
   snapshots don't depend on session env), and `manifest_toml` is
   gone (divergence from the issue's key: `resources` replaces it,
   matching the ADR 0080 reuse key). The FC `SNAPSHOT_VERSION` MUST
   be in the key (the issue-#160 cross-version corruption class,
   re-armed by aggressive reuse).
2. **Warm overlay** = restore the cold base (checkpoint chain seeds
   from its memory manifest — `seed_checkpoint_chain_sparse`), run
   the `[warm]` hook under the #539 watchdog, take a **Diff**
   snapshot (O(hook-dirtied pages)). Checkpoint manifests are
   full-image chunk lists — the overlay restores with no chain
   replay; the enabled image points at the overlay.
3. Miss path: one VM does cold-boot → Full-at-ready → report cold
   base → **continue the same VM** into warming (no re-boot). Hit
   path: restore the base → warming. Warm-less images: the cold
   base IS the artifact.
4. `reuse_ok = config.warm.is_none()` is **deleted**: warm-less
   images reuse the whole artifact (now FC-version-keyed too); warm
   images reuse the cold base and ALWAYS re-run the hook — env
   rotation still takes effect, preserving what the old rule
   protected.
5. Reuse of a cold base requires the chunk-presence self-heal check
   (`reuse_candidate_chunks_present`) AND a fleet FC-version match:
   when a cold-base candidate exists, `pick_capture_host` pins
   `CapabilityRequirements{fc_snapshot_version}` to the base's.
6. GC: `cold_bases.snapshot_id` joins the pin-set roots (ADR
   0077's reachability model) — no second GC.

### C. Placement

`pick_capture_host` (shared with materialize) gains:
`CaptureFootprint{disk_mib ≈ 2×image + mem + 4096 slack}` vetoes,
one-active-capture-per-host anti-affinity
(`capture_jobs_active_host`), max-free-disk ranking (replacing
first-fit), and the optional `fc_snapshot_version` pin from B5.
`NoCapacity` stays retryable (the `assigned` deadline re-picks).

### D. Reuse telemetry + determinism

- `enable_jobs.reuse_outcome` stamped on every terminal enable:
  `reused_full | reused_cold_base | recaptured:no_cold_base |
  recaptured:content_changed | recaptured:chunks_missing |
  recaptured:fc_version_changed`; surfaced in the enable-jobs API.
- Determinism gate (adapted: the CI bake workflow is gone, ADR
  0080): a materializer-level test packs the identical tree twice
  and asserts identical `Manifest::content_ref` — the silent
  dedup-collapse regression (`SOURCE_DATE_EPOCH`, mke2fs drift)
  fails a unit lane instead of presenting as "enables got slow".
  (Skip if 3a's fixture tests already assert this — verify at
  implementation.)

## Decisions recorded (from the issue's resolved cross-cutting set)

- **4 — not a session row.** A capture-VM session row would need
  idle-eviction, checkpoint, and prompt-delivery special cases — an
  exempt-list by another name. Own table; the ADR 0079 op-log is a
  sibling pattern, not a substrate (ops are session-keyed,
  epoch-fenced to `sessions.current_epoch`; captures are
  enable-keyed with their own token).
- **5 — no new session-FSM state**; in-flight visibility is the job
  row.
- **9 — `SNAPSHOT_VERSION` in the content key** + fleet match at
  placement.
- **11 — VZ/Process story.** The executor lives in the host-agent
  above the `SandboxBackend` seam (where `build_base_snapshot` sits
  today), so the job model works for FC, VZ, and Process alike. The
  split selects its stage plan via an explicit backend capability
  `supports_diff_memory_snapshots()` (FC true; VZ/Process false —
  they run the single-stage full path, never read or write
  `cold_bases`; `backend_kind` in the key keeps namespaces
  disjoint). Capability mismatch is a hard error, not a fallback.
- **12 — numbers discipline.** Baselines quoted from the 2026-07-01
  survey; the refuted "<5% failure / ~8 min p50" targets are NOT
  restated. Target: orchestration-caused duplicate/discarded
  captures → 0; dev-brain re-bake cost → warm-hook-bound (hook +
  diff freeze + diff upload; no cold boot, no full dump).
- **Data-plane alternative recorded:** CI dual-publish of chunks at
  bake time became moot — ADR 0080 deleted the bake entirely;
  `MaterializeImage` is the data plane and stays an RPC (it is
  idempotent and content-addressed; capture is neither, which is
  why capture gets the job model first).

## What gets deleted

- `BuildBaseSnapshot` RPC end-to-end: proto rpc + messages, the
  streaming client (`grpc_client.rs` inherent + trait impl), the
  server arm + stream type (`grpc_server.rs`), the
  `HostClient`/`SandboxBackend` trait methods' streaming shape
  (replaced by the executor's internal seam), wire-golden coverage
  moves to the heartbeat fields.
- The `base_captures` DashMap + its reconcile exemption (replaced
  by the live-record predicate; regression test ported in the same
  change).
- The enable-scanner capture consumer task
  (progress-frame → `update_enable_job_capture_progress`) — the
  heartbeat reconcile writes the same 0079 columns from
  `CaptureJobReport`s, so the dashboard surface is preserved.
- `classify_capture_error`'s transport-retry arm for captures (the
  row's `retryable` + deadlines subsume it).
- `reuse_ok = config.warm.is_none()`.

NOT deleted (out of scope): `idle_evictor.rs` commit/abort arms +
`inflight_snapshots` (ADR 0077's scope); session checkpoint
machinery; `MaterializeImage` (stays an RPC by decision above).

## Phases

- **P1 — job model** (this PR): migration, metadata verbs, wire v15
  heartbeat fields + RPC deletion, host executor module
  (`capture_job.rs`) with durable records + rehydrate, claim
  endpoint, scanner capturing-leg rework, deadline scan.
- **P2 — placement**: footprint + anti-affinity + ranking +
  FC-version pin.
- **P3 — cold-base/warm-overlay**: `cold_bases`, the split
  executor stage plan, reuse rework, `reuse_outcome`.
- **P4 — determinism test + surfacing.**
  All phases land in ONE PR (standing directive: big-bang per
  issue), but commit-per-phase inside it.

## Acceptance criteria

- A coordinator pod restart AND a WS-tunnel reconnect during a
  synthetic slow `[warm]` hook both leave the capture running; the
  enable reaches `ready` with exactly ONE capture VM ever booted.
- A non-retryable failure is terminal in PG within one heartbeat
  interval regardless of observing replica; `attempts` increments
  once per real attempt.
- No `capture_jobs` row sits non-terminal past its stage deadline +
  one scan interval.
- A capture VM never outlives its job record + one reconcile pass.
- No capture lands on a host lacking footprint headroom or already
  running one.
- A deps-unchanged warm-image re-bake reuses the cold base (no cold
  boot, no full dump); upload is O(dirtied pages). Cold-base reuse
  never crosses an FC `SNAPSHOT_VERSION` or backend-kind boundary.
- `reuse_outcome` non-NULL on every terminal enable.
- e2e enable→ready coverage (the `integration-bake-demo.sh` CLI
  path + `enable_jobs_live_pg` suites) survives the rework in the
  same change.

## Divergence log

**P1b (cutover commit — this PR's second commit, on top of the P1a
foundation) landed**: migration 0096 verbs wired live, wire v15 heartbeat
fields consumed on both sides, `BuildBaseSnapshot` RPC deleted end to
end (proto/client/server/`HostClient` trait — `SandboxBackend::
build_base_snapshot` unchanged, now called only by the host-agent's own
executor), the `capture_job.rs` executor + durable record + live-sandbox
registry, the claim endpoint, and the scanner's watch-only `Capturing`
rework + stage-deadline scan.

- **`capture_jobs.manifest_digest` added (migration 0097, NOT in the
  original 0096 design)**: the P1a row carried `disk_manifest` (the
  content-derived chunked-rootfs ref) but not the OCI `manifest_digest`.
  Two real needs surfaced implementing P1b: (1) the claim handler needs
  it to digest-pin `SandboxSpec.image` (issue #192 — a moving tag must
  not let the host's local OCI cache serve a different bake than the
  coordinator materialized); (2) the enable scanner's watch-only
  `Capturing` re-entry (capture is now genuinely multi-tick — a capture
  VM can run for minutes across many scanner ticks) needs to SKIP
  re-running `materialize_image_on_host` on every tick, and without a
  durable `manifest_digest` on the job row there was no way to
  reconstruct the materialized state without re-pulling. Caught by the
  `enable_reuse_live_pg` regression test asserting "materializes exactly
  once" — it failed (materialized 3x) before this column existed.
- **Executor design: calls `SandboxBackend::build_base_snapshot`
  unchanged, doesn't reimplement its body.** The spec text describes
  "moving the body... structurally intact" into `capture_job.rs`; the
  ADR's own §11 ("the executor lives... above the `SandboxBackend`
  seam") is the more precise statement and is what got built: the
  executor spawns `backend.build_base_snapshot(...)` and drains its
  existing `CaptureProgress` channel into the durable record + heartbeat
  report, exactly the way `grpc_server.rs`'s deleted handler used to
  drain it onto the gRPC stream. `CaptureProgress` gained one field
  (`sandbox_id: Option<SandboxId>`, stamped on the first event) so the
  executor learns the VM's identity without re-plumbing the trait
  method's signature — the type no longer crosses any wire, so this was
  a free change.
- **`CaptureJobProgress` (the wire-facing progress shape) is lossier
  than the old `CaptureProgress`**: it has `{detail, log_tail}`, not the
  old `{warm_stage, warm_stages: Vec<WarmStageRecord>, output_tail}`.
  This is the ALREADY-DECIDED P1a schema (migration 0096's
  `stage_progress` shape), not a P1b regression — but it means the
  `enable_jobs.warm_stage`/`warm_stages` dashboard columns only get a
  best-effort mirror (`report.progress.detail` stands in for the
  specific warm-hook stage name) via the new UNFENCED
  `mirror_capture_progress_to_enable_job` verb, not the rich stage
  history `update_enable_job_capture_progress` used to persist.
- **`update_enable_job_capture_progress` (the old fenced,
  claim-renewing verb) is now DEAD CODE** — nothing calls it (the
  scanner's old consumer task, its only caller, is deleted). Left in
  place (Postgres impl + trait method + its own live-pg test coverage)
  rather than deleted, to keep this commit's footprint bounded; a
  follow-up cleanup commit should remove it.
- **`release_enable_job_claim` (new verb, not in the original design)**:
  the watch-only `Capturing` exit needs the enable-job claim released
  IMMEDIATELY (so the next ~3s tick re-observes the capture_jobs row),
  not left to expire via `claim_enable_jobs`'s 300s lease — the lease
  exists to bound a crashed pod's ownership, not to pace a healthy
  watch loop.
- **`latest_capture_job_for_enable` (new verb, not in the original
  design)**: `active_capture_job_for_enable` (P1a) only returns
  NON-terminal rows — by definition it can never observe a `done`/
  `failed` outcome, so the scanner has no way to see a job that just
  went terminal. This new verb (`ORDER BY created_at DESC LIMIT 1`,
  terminal or not) is what the scanner actually polls.
- **`retry_enable_job` now also clears a stale terminal `capture_jobs`
  row** for the same enable job (a small addition to the existing
  Postgres query, not a new verb) — otherwise a retried enable job would
  see the OLD exhausted-Failed row forever via
  `latest_capture_job_for_enable` and immediately re-fail non-retryable
  without ever attempting a fresh capture.
- **Claim-time secret-resolution failure**: implemented as specified —
  a fail-loud `resolve_capture_env` error inside the claim handler
  writes a synthetic non-retryable `Failed` terminal via
  `record_capture_job_report` (fenced by the row's own current epoch)
  BEFORE returning the HTTP error, so the host never receives a spec for
  a job that's already dead coordinator-side.
- **`reuse_outcome` values landing this commit**: only `reused_full`
  (the existing content/digest reuse hit) and
  `recaptured:content_changed` (any fresh capture, reuse miss). The
  ADR's fuller taxonomy (`reused_cold_base`, `recaptured:no_cold_base`,
  `recaptured:chunks_missing`, `recaptured:fc_version_changed`) is
  meaningless before P3 (`cold_bases` doesn't exist as a concept in the
  capture flow yet) — deferred to that phase as the ADR's own P3/P4
  scoping already implies.
- **Digest-pinning gap, now closed**: an earlier draft of this commit
  left `spec.image` un-pinned at claim time (no `manifest_digest` on the
  row) with a documented "harmless, record-keeping only" rationale.
  Superseded by the migration-0097 fix above — flagging here only
  because it's exactly the kind of divergence this log exists to catch
  before it goes stale.
- **NOT done this commit** (P2/P3/P4, explicitly out of scope per the
  Phases section): footprint/anti-affinity/FC-version-pin placement,
  the cold-base/warm-overlay split, `cold_bases` reuse, the
  determinism test. `pick_capture_host` is called verbatim (first-fit,
  no footprint sizing) everywhere this commit needed a host pick.
- **Review round 1 (same PR): three reconcile-loop fixes.** (a) The
  terminal-report ack rule initially only acked applied-or-
  same-epoch-terminal — a SUPERSEDED-epoch terminal (job reassigned
  away) was never acked, so the old host re-advertised its fenced-off
  record every heartbeat forever; the rule is now the pure
  `should_ack_capture_terminal` (ack iff applied ∨ row-gone ∨
  row.epoch > report.epoch ∨ same-epoch-terminal; a PG lookup ERROR
  never acks — distinct from row-gone, or a blip would delete the only
  durable copy of a finished capture). (b) The dashboard mirror is
  gated on `applied` so a stale-epoch report can't overwrite the live
  attempt's columns. (c) `HeartbeatResponse.capture_assignments`
  became `Option<Vec<…>>` (None = coord read failed = do nothing;
  collapsing the error into `[]` would have made a PG blip read as
  "cancel everything"), enabling the new host-side convergence cancel:
  `cancel_absent` destroys the VM of any running attempt absent from
  an authoritative assignment list (reassigned away/superseded) —
  WITHOUT aborting the executor task, so `run_one`'s terminal
  bookkeeping still runs and the stale report drains via (a).

**P2 (placement — footprint/anti-affinity/FC-version pin) landed**:
`pick_capture_host` gained a pure, unit-tested core
(`pick_capture_host_from`) taking a `CaptureFootprint` and an optional
`required_fc_version`, filtering on the ADR 0078 disk floor + the
footprint's own headroom + `hosts_with_live_capture_jobs` anti-affinity
(the verb P1a already shipped, previously unused) + the existing
`CapabilityRequirements::fc_snapshot_version` gate, and ranking
survivors by max free disk. All four call sites — `ensure_capture_job`,
`materialize_image_on_host`, the capturing-stage retryable-failure
reassign pick, and the stage-deadline-scan reassign pick — now compute
and pass a real footprint.

- **`CaptureFootprint::for_capture`/`for_materialize`/`floor_only`,
  not a bare struct literal at each call site** — the ADR's own formula
  (`2×image + mem + 4096` / `ceil(2.5×image) + 1024`) is centralized
  once in `placement.rs` so every caller states its INTENT (a capture,
  a materialize, or "I have no size signal") rather than restating
  arithmetic.
- **`materialize_image_on_host` uses `floor_only()`, loudly commented,
  not a guess from the OCI manifest's compressed layer sizes.**
  Materialize is what PRODUCES the disk manifest a real size could be
  read from — there is no size signal to read yet at that point, and
  the compressed layer sizes are a poor proxy for the flattened+packed
  ext4 output (verified against `engram-rootfs-materializer`: the
  pack step alone can 2-3x the compressed input). Guessing from a bad
  proxy is worse than admitting "unknown" and falling back to the
  floor-only, still-safe posture.
- **`ensure_capture_job`'s footprint reads `Manifest::total_bytes` via
  a metadata-only `chunk_store.get_manifest` call** (no chunk bytes
  fetched) — the disk manifest is guaranteed present by the point this
  runs (the `materializing` stage already stamped it), so this is a
  cheap, honest read, not a fallback.
- **`required_fc_version` is threaded through every call site's
  signature but is `None` everywhere in P2** — no cold-base concept
  exists yet in this commit to derive a version pin from. Populated in
  P3 for the CLAIM's own placement decision (`ColdBasePlan`); **NOT
  populated for the *pick* itself** — see the P3 entry's "known gap"
  below, which is the more precise and important statement.
- Anti-affinity is applied to the SHARED picker (captures AND
  materializes), not a capture-only veto: both are heavy, disk/CPU-
  bound jobs, and running two on the same host at once is exactly what
  the anti-affinity is meant to prevent, matching the ADR's own
  framing of `pick_capture_host` as "shared with materialize."

**P3 (cold-base/warm-overlay split) landed**: `ColdBasePlan` (a
tri-state enum — `NotApplicable`/`Miss`/`Hit` — computed ONCE,
coordinator-side, in the claim handler's new `resolve_cold_base_plan`),
`CaptureJobResult`/`CapturedColdBase` (replacing the bare
`SnapshotMetadata` `result_bincode` payload), `cold_base_content_key`,
`PooledBackend::build_base_snapshot` restructured around the plan, and
`finalize_capture_job` recording `cold_bases` + the full
`reuse_outcome` taxonomy.

- **Migration 0097 → 0098, not a 0096 edit**: the P1a `cold_bases`
  schema (`content_key, snapshot_id, disk_manifest, memory_manifest,
  fc_snapshot_version, captured_at`) has no way to reconstruct a full,
  restorable `SnapshotMetadata` — several of that struct's fields
  (`state_blob_key`/`sidecar_blob_key` etc) ARE derivable from
  `snapshot_id` alone by convention, but others (`aux_bundles`,
  `paused_at`, `image_version`, `size_bytes`) are not, and
  reconstructing a lossy approximation risked a restore silently
  missing a real field a future capture starts using. Migration 0098
  adds `cold_bases.snapshot_bincode` (nullable, since the table was
  still dormant when 0098 landed — no backfill needed) storing the
  executor's own bincode-encoded `SnapshotMetadata` verbatim; the
  claim handler decodes it straight into `ColdBasePlan::Hit`.
- **`ColdBasePlan` is a tri-state enum, not the ADR text's
  `Option<ColdBaseCandidate>`.** `NotApplicable` (non-FC host, or an
  FC host with no reported `fc_snapshot_version`), `Miss{content_key,
  reason}`, and `Hit{content_key, snapshot}` are three semantically
  distinct outcomes that nested `Option<Option<…>>` would blur — and
  the `Miss`/`Hit` variants both need to carry the SAME `content_key`
  the executor must report its outcome under (whether minting a fresh
  cold base or restoring an existing one), which a bare
  `Option<ColdBaseCandidate>` (candidate-or-nothing) has no slot for on
  the `Miss` arm at all.
- **`backend_kind` is NEVER computed host-side** (the ADR's text
  floated adding a `SandboxBackend::kind()` trait method for this).
  Grepped first, per instruction: `HostCapabilities::backend` already
  exists (`"firecracker" | "vz" | "process"`, populated at host-agent
  startup, reported every heartbeat) and the CLAIM HANDLER already
  reads the claiming host's full `HostRecord` to check
  `fc_snapshot_version` — so it already has `backend` for free. The
  content key is computed ONCE, coordinator-side, and only ever echoed
  back by the executor (`ColdBasePlan`'s own doc). This also sidesteps
  ever needing a `resources: &ResourceHints` parameter host-side — the
  executor operates purely on `SandboxSpec`, which has no `resources`
  field of its own (only the resolved `memory`/`cpu`/`disk` limits
  derived FROM it), so recomputing the key host-side would have needed
  a NEW field on `BuildBaseSnapshotRequest` just to carry it through;
  not computing it there at all is strictly simpler.
- **Reused `supports_diff_checkpoints()`, did NOT add
  `supports_diff_memory_snapshots()`.** Grepped first, per instruction:
  the existing ADR 0028 capability is EXACTLY "does this backend
  produce coherent, O(dirty-set) memory checkpoints" — the same
  question a cold-base overlay asks — and it's already correctly gated
  on FC's `track_dirty_pages` config (not just "is this FC"), which is
  MORE precise than a static per-backend-type flag would be: an FC host
  with dirty-page tracking disabled must ALSO hard-error on a `Hit`/
  `Miss` plan, and `supports_diff_checkpoints()` already encodes that.
  A second, redundantly-named method would have been the same fact
  under two names.
- **A capability mismatch is `CaptureFailureKind::ColdBaseCapabilityMismatch`,
  a NEW variant** (not an existing kind pressed into service) —
  `is_retryable() == false` (deliberately: reassigning to a fresh host
  would often "fix" it in practice, but marking it non-retryable
  surfaces the underlying placement bug instead of quietly
  self-healing via reassignment every time it recurs).
- **Executor stage plan lives ENTIRELY in `PooledBackend::
  build_base_snapshot`, not split out into `capture_job.rs`** (the
  ADR's own fallback: "prefer moving logic into pooled_backend.rs — it
  owns the machinery"). `capture_job.rs`'s `run_one` is unchanged
  beyond constructing a `BuildBaseSnapshotRequest` and bincode-encoding
  `CaptureJobResult` instead of a bare `SnapshotMetadata` — it remains
  a pure job-lifecycle wrapper around whatever
  `SandboxBackend::build_base_snapshot` (default: hard error; real
  impl: `PooledBackend`) returns.
- **No new pause/resume/chain-seed plumbing was written.** `self.
  snapshot(id)` already auto-seeds the checkpoint chain off a Full
  capture's own memory manifest (`advance_checkpoint_state`, ADR 0028
  Fix A machinery, unchanged), and `self.restore(metadata)` already
  seeds it sparse off a restored base's memory manifest (the plain
  session-resume path, unchanged) — so `Hit` is just `self.restore(...)`
  instead of `self.create(...)`, and `Miss`-on-a-warm-image is just an
  EXTRA `self.snapshot(id)` call before the hook runs. Verified: this
  is exactly what the grounding read's "verify against
  `seed_checkpoint_chain_sparse`'s restore path" asked for.
- **Warm-less images: exactly ONE `snapshot()` call, not two.** The
  ADR's own text ("the cold base IS the artifact") is followed
  literally — there is no separate "mint the cold base" step distinct
  from "capture the artifact" for a warm-less image; whatever `Miss`/
  `Hit`/`NotApplicable` plan applies, the single final snapshot's own
  metadata becomes both `CaptureJobResult::snapshot` and (when a plan
  applies) `CapturedColdBase::snapshot`.
- **`reuse_ok` renamed `warm_less`, not literally deleted** — the
  GATE it names (whole-artifact reuse is warm-less-only; warm images
  reuse the COLD BASE instead, via `ColdBasePlan`, and always re-run
  the hook) is unchanged and still real; only the misleading generic
  name (implying "reuse is or isn't OK" as a single axis, when P3 adds
  a second, orthogonal reuse mechanism for warm images) was retired.
- **The FC-version dimension on whole-artifact reuse is enforced via
  a NEW `candidate_fc_version_known` check reading the candidate's
  `snapshots` row directly (`get_snapshot`), not a denormalized
  `enabled_images.base_snapshot_fc_snapshot_version` column.** Grepped
  first: no such column/join exists (unlike `base_snapshot_disk_
  manifest`/`base_snapshot_memory_manifest`, which ARE denormalized).
  Adding one would need a new migration + backfill for a value only
  read on the (rare) reuse-hit path; a plain `get_snapshot` read is
  cheap enough there and needs no schema change. A memory-manifest-
  bearing candidate (FC) whose `snapshots.fc_snapshot_version` is
  `NULL` (a pre-ADR-0068 row, or a host that never reported one) is
  now treated as NOT reusable — such a row could never be placement-
  gated at restore time either (`fc_snapshot_version: None` is ADR
  0068's soft "unconstrained" posture), so silently reusing it would
  re-arm exactly the issue-#160 cross-version corruption class this
  whole ADR exists to keep closed. Disk-only candidates (VZ,
  `memory_manifest: None`) are exempt — no FC/UFFD restore risk exists
  for them at all.
- **`reuse_outcome`'s `Miss` sub-taxonomy (`no_cold_base` /
  `chunks_missing` / `fc_version_changed`) is carried on
  `ColdBasePlan::Miss`/`CapturedColdBase` as a `ColdBaseMissReason`
  enum, computed ONLY by the claim handler** (which already ran the
  `get_cold_base` lookup + the chunk-presence self-heal + the new
  `cold_base_fc_version_changed` query) **and echoed back unchanged by
  the executor** — the executor's own view genuinely cannot distinguish
  these three ("no candidate" vs "candidate existed but failed
  presence" vs "candidate existed under a different version" all look
  identical from inside `build_base_snapshot`: "boot fresh"). A new
  metadata verb, `cold_base_fc_version_changed(disk_manifest,
  current_fc_version) -> bool`, answers "does a `cold_bases` row exist
  for this rootfs under ANY OTHER version" — it matches on
  `disk_manifest` alone (the table has no `resources` column to refine
  further, and every row is FC's by construction, since only FC ever
  writes one), which is a sound approximation for a TELEMETRY label,
  explicitly not treated as a correctness gate anywhere.
- **GC gets a 7th chunk pin-set source (`cold_base_manifest_refs`,
  `PinSet::collect`) in ADDITION to the ADR's own ask
  (`cold_base_snapshot_ids` joining the snapshot-blob pin-set).** A
  cold base's disk/memory MANIFEST chunks have no OTHER root — unlike
  the overlay it seeds, a cold base never gets its own `enabled_images`
  row or a `recoverable=true` reason to be found by the existing 6
  sources — so without this 7th source, a live `cold_bases` row's
  chunks would be silently reaped by the chunk-GC sweep even while its
  `snapshots/<id>/{state.bin,sidecar.json}` blobs stayed correctly
  pinned by the (ADR-specified) `cold_base_snapshot_ids` addition to
  `snapshot_blob_pin_set`. Both are unconditionally-called, no second
  GC, matching the existing sources' shape exactly.
- **KNOWN GAP, deliberately not closed this phase: placement does NOT
  yet pin `fc_snapshot_version` toward an EXISTING cold base.** The
  ADR's §B5 ("when a cold-base candidate exists, `pick_capture_host`
  pins `CapabilityRequirements{fc_snapshot_version}` to the base's")
  describes routing a capture toward whichever host's FC version
  already HAS a reusable cold base — but the cold-base LOOKUP
  (`resolve_cold_base_plan`) only runs inside the claim handler, i.e.
  AFTER a host is already picked (`pick_capture_host` in P2 always
  passes `required_fc_version: None`). The consequence: a capture only
  gets a `Hit` when the RANDOMLY-picked host happens to report the
  same `fc_snapshot_version` a prior cold base was captured under; on
  a fleet with a mixed/rolling FC version, an otherwise-reusable cold
  base can be missed simply because placement routed to the "wrong"
  host. `reuse_outcome`'s `recaptured:fc_version_changed` telemetry
  still correctly LABELS this when it happens (so it's visible, not
  silent) — but the OPTIMIZATION this ADR section describes (steering
  placement toward a hit) is not implemented. Closing it properly
  needs `ensure_capture_job` to look up `cold_bases` BY
  `(disk_manifest, resources)` across every version BEFORE calling
  `pick_capture_host` (a query the current schema/verb surface doesn't
  have — `get_cold_base` is keyed on the full content key, which
  already bakes in one specific version) and thread the result's
  version through as the `required_fc_version` pin. Left for a
  follow-up; flagging here rather than leaving it silently unaddressed.
- **Live-PG coverage for `finalize_capture_job`'s own `cold_bases`
  upsert + `reuse_outcome` column write is NOT independently exercised
  end-to-end.** `finalize_capture_job` is `pub(crate)`, reachable from
  an external `tests/*.rs` binary only through the full enable-scanner
  pipeline (materialize simulated, scanner ticks, a fake capture-job
  simulator) — the same heavy shape `enable_reuse_live_pg.rs` already
  uses for the DIFFERENT regression it guards. Its three constituent
  layers ARE independently live-PG-verified (the `cold_bases`
  store surface in `capture_jobs_live_pg.rs`; the claim handler's
  `ColdBasePlan` resolution in `claim_cold_base_live_pg.rs`; the
  executor's stage-plan → `CaptureJobResult` shape in
  `pooled_backend.rs`'s unit tests) — but the specific wiring inside
  `finalize_capture_job` that reads a `CaptureJobResult`, maps it to a
  `reuse_outcome` label, and calls `upsert_cold_base` is only compile-
  and-review-verified, not exercised by a running test. A follow-up
  should extend `enable_reuse_live_pg.rs`'s simulator (or a sibling) to
  drive a warm-image enable through a cold-base MISS and assert both
  the `enable_jobs.reuse_outcome` value and a resulting `cold_bases`
  row.

**P4 (reuse-telemetry cleanup + materializer determinism gate)
landed**: the dead `update_enable_job_capture_progress` metadata verb
(trait default + Postgres impl + its `enable_jobs_live_pg.rs` test
coverage) is deleted; `mirror_capture_progress_to_enable_job` (P1b) is
confirmed as its sole replacement (unfenced, dashboard-only — see that
verb's own doc for why fencing/lease-renewal no longer apply once
`capture_jobs` owns execution/fencing).

- **The determinism gate already existed — no new test added.**
  `engram-rootfs-materializer`'s `materialize_is_deterministic_and_
  scrubs_scratch` (`tests/materialize.rs`) already runs the FULL
  pipeline (pull → flatten → inject → pack → chunk) twice over the
  same fixture and asserts `first.disk_manifest == second.disk_manifest`
  — a `ManifestRef` equality check IS the content-derived-ref
  determinism gate the ADR asks for (`Manifest::content_ref()` is what
  produces that ref). Per this ADR's own instruction ("skip if 3a's
  fixture tests already assert this — verify at implementation"): this
  is that assertion, verified present and still green.
- **`progress_state_failure_and_retry_round_trip`
  (`enable_jobs_live_pg.rs`) was adapted, not left calling the deleted
  verb**: it now seeds capture-progress state via `mirror_capture_
  progress_to_enable_job` (the still-live replacement) to keep proving
  its actual point (retry clears stale progress columns); its
  assertions on `warm_stage_started_at`/`warm_stages` were dropped
  rather than kept as a vacuous "still None" check — the new mirror
  verb never populates those two columns at all (P1b's already-
  documented lossier `CaptureJobProgress` wire shape carries no stage
  HISTORY), so nothing production-real exercises them anymore; a
  future cleanup could drop the columns/fields themselves, out of
  scope here.
- **`capture_progress_is_fenced_renews_lease_and_survives_failure`
  was deleted outright, not adapted** — its entire premise (a claim-
  fenced write that renews the claim lease) is the deleted verb's
  specific behavior; the replacement verb is deliberately UNFENCED
  (own doc: `capture_jobs` owns fencing now), so there is no
  equivalent behavior left to re-test under a different name.
- **Review round 2 (bloat audit, same PR).** An adversarial size audit
  of the full diff (7,443 insertions) judged it fundamentally justified
  (~31% tests, ~10% this ADR, the rest the RPC→job swap + the §B split)
  but surfaced real trims, applied here: (a)
  `active_capture_job_for_enable` was DEAD — `insert_capture_job`'s
  insert-or-get inlined its own read and nothing else called it; the
  same shape of leftover the P4 cleanup caught for
  `update_enable_job_capture_progress`, missed in that pass. Deleted.
  (b) `CaptureJobRecord`'s persist/load_all/delete_acked trio was a
  near-verbatim copy of `CheckpointRecord`'s — both now delegate to a
  shared `durable_record` module (one owner for the
  write+fsync+rename / torn-write-tolerant-load / idempotent-delete
  contract). (c) `engram_protocol::heartbeat::HeartbeatAck.
  capture_assignments` still carried the pre-review `Vec` shape (and a
  round-trip test asserting it) after review round 1 Option-ized the
  production HTTP mirror — aligned to `Option<Vec>` so the vestigial
  protocol type can't teach the wrong none-vs-empty semantics. (d)
  Three stale doc comments fixed (`req.cold_base` → the
  `cold_base_plan` tri-state; the types module's "dormant" claim; a
  misplaced footprint doc). Not trimmed, recorded as acceptable: the
  per-test-binary `TestEnv` duplication in the FC tests (each
  `tests/*.rs` is its own crate; only `common/` is shared) and the
  `self_ref`/`strong_self` pattern shared with `PooledBackend` (a
  generic helper would cost more ceremony than it saves).
- **Review round 3 (CI, real-FC + e2e failures — the joins unit mocks
  can't see).** (a) `finalize_capture_job` hard-requires
  `fc_snapshot_version` on any capture that produced a cold base, but
  NOTHING host-side ever produced it — all three report-construction
  sites stamped `None`, so every warm FC capture failed at finalize
  ("stamped no fc_snapshot_version", both e2e lanes). The executor now
  probes `firecracker --snapshot-version` once at construction (the
  binary can't change under a running host-agent; `None` on
  VZ/Process) and stamps it on every report — the host that runs the
  VMM is the authority, not the heartbeat-lagged capabilities row.
  Regression-tested incl. across a restart rehydrate. (b) The
  cold-base HIT path seeded the checkpoint chain SPARSE (via
  `restore()`'s session-resume default) — at the cold base's own
  shared lineage, so the second capture's overlay diff collided with
  the first's (`put manifest base_id@v2 … latest is v2`, the FC-lane
  failure). The Hit arm now replaces the sparse seed with a FORKED
  one — the same each-consumer-owns-a-fresh-lineage rule
  session-create off a shared base template already documents.
