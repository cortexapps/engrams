# ADR 0088: Fleet rolls must not kill in-flight enable work

Status: Accepted

## Context

Every push to `main` auto-rolls the host fleet (ADR 0044 K3: the operator's
drain-gated, node-by-node pod swap). The image roll deliberately does **not**
drain: ADR 0044 K2 keeps the node's microVMs alive across the pod swap
(`hostPID`/`hostNetwork` + pidfd reattach), so `roll_node` cordons and then
deletes the pod immediately — evacuation would rewind sessions for no reason
when the VMs never die.

That reasoning is correct for *session VMs* and wrong for *enable work*. An
in-flight image **materialize** (the streaming `MaterializeImage` RPC: pull →
flatten → pack → chunk, ADR 0080 phase 3b) and a base-snapshot **capture**
(the `capture_jobs` executor + its capture VM, ADR 0084) are the host-agent
*process's own work* — SIGTERM ends them, and nothing in `roll_node` waits.

Prod evidence (dev-brain enable job `83310fad`, 2026-07-10): a deploy rolled
the fleet mid-enable and killed the materialize twice ("h2 protocol error:
error reading a body from connection"), then a third attempt died with the
host-agent ("removed orphaned materialize scratch (previous host-agent died
mid-run)"). Each retry restarts the ~55-min materialize from scratch (only
chunk-store PUTs dedup). The enable took 2h39m; a 2026-07-08 job burned all
5 attempts against roll/incident churn and went terminally `failed`. The
durable-job layer works — it converts "failed" into "slow" — but the attempt
itself has no protection.

What already works (and this ADR does not change):

- **New work is fenced.** All three host pickers (session placement, the
  reserving capture picker, `pick_materialize_host`) funnel through
  `host_is_schedulable`, which excludes `hosts.cordoned` — and `roll_node`
  cordons before deleting the pod. A retry never re-picks a mid-roll host.
- **Scale-down drains sessions.** The wave's `gate_drain` waits for
  `running_sandboxes == 0`, which *incidentally* covers a capture VM (it is
  registered in the backend and counted) — but not a materialize, which
  boots no VM.

The two gaps:

1. **The image roll has no work gate at all** — `roll_node` deletes the pod
   with in-flight materialize/capture on it.
2. **Materialize is invisible to the control plane.** `capture_jobs` carries
   `host_id` + a non-terminal-stage filter, but `enable_jobs` has no host
   column; the materialize placement lives only in the scanner's
   `advance_one` call stack and the host-side `materialize_gate` try-lock.

## Decision

Make in-flight enable work **visible** (PG, operator-queryable) and make both
roll paths **wait for it** (bounded, never wedging a roll).

### 1. Durable materialize placement (`enable_jobs.materialize_host_id`)

Migration 0102 adds a nullable `materialize_host_id UUID` to `enable_jobs`.
`materialize_image_on_host` stamps it (fenced by `claimed_by`, like every
enable-job write) immediately after `pick_materialize_host`, before the
streaming RPC starts — so there is no window where work is running but
unattributed.

**Liveness needs no new machinery**: the host's ≤30 s materialize keepalive
frames already renew the claim (`update_enable_job_materialize_progress` sets
`claimed_at = NOW()`), so *live* materialize on host X is exactly

```sql
state = 'materializing' AND materialize_host_id = X
  AND claimed_at > NOW() - <lease window>
```

A dead stream stops renewing and drops out of the gate within the lease
window (300 s default) — the gate can never wait on a ghost. The column is
never cleared: it is inert outside `state='materializing'` and doubles as a
"where did the last materialize run" breadcrumb.

### 2. Fleet-view surface (`live_materializes` / `live_capture_jobs`)

A new `MetadataStore::live_enable_work_by_host` aggregates, per host:

- **materializes**: the liveness predicate above;
- **captures**: `capture_jobs` rows in a non-terminal stage
  (`stage NOT IN ('done','failed')`, `host_id IS NOT NULL` — WAITING rows
  bind no host). Captures need no freshness filter: the enable scanner's
  stage deadlines already redrive-or-fail a stuck capture row.

Both counts ride `HostView` (api + `app.v1` proto fields 30/31, RUST-WIRE-ONLY
like `ready_images`) so the operator reads them over the same
`FleetService.GetHost` poll the drain gate already uses.

### 3. Operator gates

- **Image roll** (`roll_node`): after the cordon (so no new work can arrive)
  and before the pod delete, poll `GetHost` until both counts are zero, with
  a budget (`spec.enableWorkTimeoutSeconds`, default **5400 s**). Cordoned
  hosts receive no new enable work, so the wait is monotone: at most the tail
  of the one materialize (~55-90 min for dev-brain) or capture (warm timeout
  3300 s + freeze) currently running.
- **Scale-down wave** (`gate_drain`): the drained condition becomes
  `running_sandboxes == 0 && live_capture_jobs == 0 && live_materializes == 0`
  (materialize was invisible to the sandbox count).

**Timeout ⇒ proceed, loudly.** On budget exhaustion (or a small consecutive
RPC-error budget, e.g. the coordinator being down), the gate WARNs and the
roll proceeds — exactly today's behavior. A roll must never wedge on enable
work; the durable-job retry remains the backstop, demoted from the primary
mechanism to the rare fallback. (The scale-down wave keeps its existing
release-the-victim-on-timeout semantics.)

## Alternatives considered

- **Resumable materialize** (persist scratch, resume the stream): the big
  hammer. Most of its value evaporates once rolls stop interrupting; not
  worth the complexity while the gate exists.
- **Heartbeat bit from the host's `materialize_gate` try-lock**: no
  migration, but leaves a pick→RPC-start window where work is running and
  invisible, and puts liveness on the heartbeat path instead of the existing
  claim renewal. The PG binding has neither problem and matches "state in PG
  + scanner" (the house pattern).
- **Long `terminationGracePeriodSeconds` + host-agent SIGTERM handling**
  (finish materialize before exiting): turns every pod delete into a
  potentially-90-min hang for kubelet, invisible to the operator's planner,
  and still doesn't cover the capture VM (whose executor spans heartbeats).
- **Prestage work in the gate**: deliberately excluded — prestage attempts
  are short (1200 s bound), per-host best-effort, and already retried; a roll
  interrupting one costs seconds.

## Consequences

- A fleet roll during a dev-brain enable waits (up to 90 min on the one node
  running the work) instead of destroying 40-90 min of materialize/warm
  progress; enables land in one attempt, and the attempts budget stops being
  consumed by deploys.
- Rolls of *idle* hosts are unaffected (both counts zero → gate passes on the
  first poll).
- `enable_jobs` gains a host column; the fleet view gains two counts — both
  also useful for operability ("what is this host doing right now").
- The 5400 s default budget means a pathological enable can delay (not block)
  a roll by up to 90 min per affected node. Operators can lower
  `enableWorkTimeoutSeconds` (0 disables the gate entirely = today's
  behavior).

## Implementation notes / divergences

- Liveness came out even simpler than proposed: `claimed_at` freshness
  needed **zero** new renewal machinery — the materialize keepalive frames
  were already renewing the claim (`update_enable_job_materialize_progress`).
  The fleet view shares the scanner's default lease window
  (`DEFAULT_ENABLE_JOB_LEASE_SECS`) so gate-release and peer-reclaim happen
  on the same clock.
- `GetHost` deliberately does NOT best-effort the live-work read (unlike
  `reserved`): a failed read rendering zeros would tell the gate "no work"
  and let a roll kill a live materialize. `ListHosts` (view-only) stays
  best-effort.
- The CRD yaml is generated with structural pruning, so
  `enableWorkTimeoutSeconds` had to ride the checked-in CRD too — a CR
  field absent from the schema is silently dropped, which would have made
  the helm knob a no-op.

## UI follow-up (same PR)

The same investigation surfaced that the enable pipeline was nearly
unwatchable ("materializing chunks" for an hour). Riding this ADR's new
surfaces: `enable_jobs.materialize_stages` (migration 0103 — the
materialize twin of `warm_stages`, coordinator-stamped from the progress
frames), chunk-window counts on chunk-stage frames (optional host-proto
fields → the long-dead `chunks_done/chunks_total` columns), the full
stage histories + `materialize_host_id` on the app-proto `EnableJob`,
and a dashboard rework (stage timelines with previous-run ETAs, live
output tail, attempt/host badges, fleet enable-work badges that make the
roll gate observable).

## Commits

- ADR (Proposed)
- `store: durable materialize placement + per-host live enable work` —
  migration 0102, `set_enable_job_materialize_host`,
  `live_enable_work_by_host`, live-PG tests
- `coordinator: stamp materialize placement; expose live enable work on
  HostView` — the fenced stamp in `materialize_image_on_host`, HostView +
  app.v1 proto fields 30/31
- `operator: gate rolls and drains on in-flight enable work` —
  `gate_enable_work` in `roll_node`, the `gate_drain` enable-work leg,
  `enableWorkTimeoutSeconds` (CRD + helm)

## Addendum (2026-07-13): enable-leg latency overhaul

This ADR made enable jobs *survive* infrastructure churn; the follow-up
question was why a single healthy attempt still costs ~2 h for a
dev-brain-class image. Prod benchmark (2026-07-13, cold recapture,
28 GiB rootfs / 24 GiB VM):

| leg | measured | mechanism |
|---|---|---|
| pull | 2m25–2m56 | strictly sequential per-layer downloads |
| flatten | 51m39–**74m24** | single-threaded tar→tree; syscall-bound tiny-file creation; pure-Rust decoders |
| pack | 2m03–4m17 | deterministic `mke2fs -d` (fine) |
| chunk+upload | 3m46–7m31 | stop-and-wait 32-chunk windows; ~60–80 MiB/s effective to GCS |
| capture: memory seed | 40s–**11m29** | pre-hook full 24 GiB dump; dedup-miss uploads ~all of it, contending with the still-running rootfs upload (independent 32-wide pools, no shared budget) |
| capture: NBD page-in | 1–2m | capture host ≠ materialize host ⇒ re-fetch from GCS instead of local NVMe |
| warm hook | 10m44–20m53 | real work (dev-brain's own stack; out of scope here) |

One PR (commit chain below) attacks every engrams-side leg:

- **Parallel fd-scoped flatten.** The reader thread keeps *all* namespace
  operations (whiteouts, opaque dirs, dir/symlink/hardlink creation,
  unlink-before-create) in exact tar order; a bounded worker pool receives
  already-open file descriptors and does only fd-scoped work (write,
  fchmod, fsetxattr, futimens). Workers never touch paths, so overwrite /
  whiteout / readdir races are impossible by construction and the final
  tree is bit-identical to sequential apply — mke2fs determinism and
  rebake chunk-dedup are preserved. Ancestor-symlink resolution is
  memoized (invalidated at `remove_entry`); decompression moves to its
  own pipelined thread.
- **Pull/flatten pipeline.** Layer downloads run 3-wide with in-order
  readiness, feeding the flattener as each layer lands — pull wall-time
  hides under flatten.
- **flate2 `zlib-rs` backend.** zlib-ng-class inflate, pure Rust (the
  C-backed decoders stay banned: zstd-sys breaks the musl cross lane).
- **Streaming chunk upload.** The read-32-then-drain-32 window becomes a
  continuous reader → 64-wide `buffer_unordered` pipeline. The per-chunk
  HEAD dedup stays — a host-local "known present" cache is unsafe against
  chunk-GC's grace/generation windows.
- **Host-global upload budget.** One FIFO semaphore (default 96 permits,
  `ENGRAM_UPLOAD_BUDGET_PERMITS`) gates every ChunkStore PUT on the host,
  so concurrent workloads (materialize + memory seed + disk flush) share
  the NIC instead of stacking on it. Solo workloads never queue.
- **Capture co-location.** Capture placement prefers
  `materialize_host_id` (two-call placement: singleton candidate first,
  full candidate set on miss) so the capture VM pages the fresh rootfs in
  from local NVMe write-through cache, not GCS. Not applied to
  reassign/redrive — a failed attempt shouldn't prefer its way back.
- **Deferred seed finish.** The pre-hook cold-base seed still dumps
  synchronously (the dirty-bitmap boundary must precede the hook), but
  its `finish()` (disk flush + memory chunk upload + artifacts + chain
  seed) runs on a spawned task joined *after* the warm hook — the
  40s–11.5m upload leg leaves the critical path entirely. Join-before-
  final-snapshot keeps the final capture a Diff and keeps the durability
  barrier ahead of `finalize_capture_job`, exactly as today.
- **Balloon-shrunk seed.** A virtio-balloon device (guest kernel already
  has `CONFIG_VIRTIO_BALLOON=y`) inflates before the seed dump —
  ballooned pages are host-`MADV_DONTNEED`ed, the dense dump reads zeros,
  and the existing all-zero 512 KiB elision drops them from the manifest
  (~24 GiB → ~1–2 GiB). Fail-open on inflate (dense seed is merely slow),
  fail-loud on deflate (a hook in a starved guest is a guaranteed slow
  failure). Kill switch `ENGRAM_FC_BALLOON=0`. No `fc_snapshot_version`
  bump: device topology rides inside `state.bin`, and legacy balloon-less
  cold bases stay valid Hits (deflate tolerates their absence).

Explicit non-goals, considered and rejected:

- **New materialize wire stages** (e.g. a separate `upload` leg): the
  coordinator drops unknown stage strings and the host keepalive re-sends
  the last frame, so a mid-roll skew window would starve claim renewal.
  The four-stage vocabulary stays; upload visibility comes from the new
  per-stage histogram + `chunks_done` frames.
- **HEAD-skip presence cache**: widens the HEAD→manifest-commit window
  that chunk-GC's generation barrier covers; a cache hit on a
  GC-reclaimed chunk yields an unbootable image.
- **Capture before materialize completes**: needs a new job-creation path
  and a not-yet-durable claim mode; co-location + the upload budget
  remove most of the win.

Targets: materialize ≤15 min, capture wall ≈ warm hook + ~5 min for the
dev-brain class; small images (~1.5 min end-to-end) must not regress.

### Implementation notes / divergences (at close)

- **Upload budget landed inside `ChunkStore`, not as a `BlobStorage`
  decorator**: a permit wraps only the chunk PUT body (checked and
  unchecked flavors), acquired after the dedup-HEAD short-circuit —
  which makes "deduped puts consume nothing" and "manifest PUTs are
  never queued" true by construction instead of needing a small-body
  bypass heuristic.
- **The capture timeline is synthesized executor-side** (`capture_job::
  advance_capture_legs`) from the progress frames `build_base_snapshot`
  already emits, rather than a new `CaptureTimeline` type inside the
  backend — the frame transitions (boot / cold-base memory dump / warm
  hook / cold-base upload / final snapshot) were already the leg
  boundaries. `CaptureJobProgress` gained a `serde(default)`
  `warm_stages` field (JSON-only wire), and the heartbeat mirror now
  COALESCEs it into the previously-orphaned `enable_jobs.warm_stages`.
- **Balloon needed no FC-fork or kernel work**: the vendored guest
  configs already carry `CONFIG_VIRTIO_BALLOON=y`; the change adds the
  symbol to `build-fc-kernel.sh`'s required-config gate so a base-config
  re-sync can't silently drop it. A balloon deflate failure after the
  seed dump settles the deferred seed BEFORE aborting (the
  await-never-abort contract holds on every path).
- **Flatten parallelism shipped as fd-scoped workers exactly as
  designed**; the pull/flatten pipeline additionally REDUCED peak
  scratch (lookahead + 1 layer vs the old all-layers-then-flatten).
- Everything landed with **zero migrations and zero wire-stage
  changes**, as planned.

### Future direction: a streaming packer

The remaining materialize cost after this overhaul is structural: the
tree is written TWICE (flatten extracts tar entries into a directory
tree; `mke2fs -d` then re-reads the whole tree and copies it into the
ext4 image). A streaming packer — our own deterministic tar→ext4
writer that builds the filesystem incrementally as layers apply —
would eliminate the intermediate tree, the second full I/O pass, and
most of the pack leg in one move.

Two constraints make this ADR-sized, not a quick win:

- **Determinism must be preserved by construction.** Chunk-level
  rebake dedup, cold-base reuse keys, and roll-kill retry cheapness
  all hang off byte-identical output for identical input (see the
  1%-dedup incident in `ext4.rs::recommended_size`'s comment). Being
  our own writer, a streaming packer CAN be deterministic — fixed
  geometry, fixed allocation order — but that property has to be
  designed in, not recovered later. The `golden_diff` / double-run
  manifest-equality tests are the gate.
- **OCI layer semantics fight streaming.** Whiteouts, opaque dirs, and
  later-layer overwrites mutate earlier-layer state, so blocks can't
  be finalized until the last layer lands (or the writer needs an
  ext4-aware delete/rewrite path). A candidate shape: flatten into an
  in-memory/indexed staging form (inode table + extent plan), then a
  single sequential materialization pass — one write of the image
  instead of tree-write + tree-read + image-write.

Explicitly NOT the path: dropping determinism to "simplify" a
streaming writer — that trades a 2-4 min pack leg for tens of GiB of
re-upload per rebake, fleet-wide cache invalidation, and the loss of
every content-keyed reuse path (evaluated at close; see the
determinism consumers above).

Commit chain: `ADR 0088 addendum (open)` → `coordinator: per-stage
materialize histogram` → `materializer: parallel fd-scoped flatten
engine` → `materializer: pipeline layer downloads with the flatten` →
`deps: flate2 zlib-rs backend` → `chunk-store: streaming read->upload
pipeline` → `chunk-store: host-global UploadBudget` → `coordinator:
prefer the materialize host for capture placement` → `host-agent:
defer the cold-base seed finish behind the warm hook` → `firecracker:
virtio-balloon device + capture-time seed shrink` → `capture-leg
telemetry + warm_stages re-wire (this commit, addendum closed)`.
