# 0081 — Capture as a durable host-owned job: `capture_jobs`, cold-base/warm-overlay split, footprint placement

Status: Proposed (2026-07-08)

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

Migration `0095_capture_jobs.sql`:

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
foundation) landed**: migration 0095 verbs wired live, wire v15 heartbeat
fields consumed on both sides, `BuildBaseSnapshot` RPC deleted end to
end (proto/client/server/`HostClient` trait — `SandboxBackend::
build_base_snapshot` unchanged, now called only by the host-agent's own
executor), the `capture_job.rs` executor + durable record + live-sandbox
registry, the claim endpoint, and the scanner's watch-only `Capturing`
rework + stage-deadline scan.

- **`capture_jobs.manifest_digest` added (migration 0096, NOT in the
  original 0095 design)**: the P1a row carried `disk_manifest` (the
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
  This is the ALREADY-DECIDED P1a schema (migration 0095's
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
  Superseded by the migration-0096 fix above — flagging here only
  because it's exactly the kind of divergence this log exists to catch
  before it goes stale.
- **NOT done this commit** (P2/P3/P4, explicitly out of scope per the
  Phases section): footprint/anti-affinity/FC-version-pin placement,
  the cold-base/warm-overlay split, `cold_bases` reuse, the
  determinism test. `pick_capture_host` is called verbatim (first-fit,
  no footprint sizing) everywhere this commit needed a host pick.
