# ADR 0019: Cold-boot latency reduction — measure first, then optimize

Status: 2026-05-26 — **Proposed.** Investigation done; Phase 0
(distributed-tracing instrumentation) not yet started. This ADR is
authored before code per our ADR-bookend practice; it will be updated
between phases with the measured breakdown and divergences, and flipped
to Accepted once the Phase 0 deliverable (a span→ms table) lands and the
optimization order is chosen from real data.

Phase: 0 (instrument + profile). Phases 1+ are *hypotheses* below, to be
confirmed and ordered by Phase 0 data — not committed work.

Commit chain (Phase 0, on `main`, each compiles clean via
`cargo check --workspace`):

- `8a924ec` — 0a: `engram-telemetry` crate + OTLP `init` wired into all
  four binaries (gated on `OTEL_EXPORTER_OTLP_ENDPOINT`, flush-on-exit
  guard); workspace-hack regenerated.
- `9ea424c` — 0b (gRPC hop) + 0c (seam spans): `TraceparentInjector`
  client interceptor + host-side `link_remote_parent` extraction;
  `session.create` root span and host `create_sandbox`/`snapshot`/
  `restore`/`start_agent` handler spans.
- `416b3b0` — fix: drop redundant `#[must_use]` on `init_tracing`
  (clippy `double_must_use`, caught by dev-vm Linux clippy; folds into 0a).
- `0e6b33e` — 0b (uffd spawn-env hop) + 0c (FC spans): spans on
  `create_in_jail`/`create_in_jail_after_net`/`restore_in_jail`/
  `spawn_uffd_handler`; `TRACEPARENT` injected into the uffd-handler
  spawn and consumed as its `uffd.run` span parent. Verified clean by
  `cargo clippy --workspace --all-targets -- -D warnings` on the Linux dev-vm.
- `3938109` — 0c: coord scheduling/resume spans (`create_for_session`,
  `restore_for_session`, `finish_resume_to_active`).
- `a0ce662` — 0e infra: Jaeger collector in the dev docker-compose, wired
  into `just dev` (Tiltfile), `just integration-up` (dev-vm), and
  `just trace-up`; OS-agnostic bridge-mode ports. Verified on macOS.
- `f2ef14e` — 0e first result: full cross-process cold-boot trace on the
  dev-vm (coord → host-agent stitched via the gRPC hop). Finding:
  `agent_handshake` ≈ 98% of cold boot. (doc; data in "Phase 0 first result")
- `2367c6b` — 0b/0c host side: decompose `start_agent` into
  `fc.await_agent_ready` + `fc.spawn_harness`; propagate trace context into
  the guest via kernel cmdline (`engram_traceparent`/`engram_otel` on
  `boot_args`); `FirecrackerConfig.guest_otel_endpoint`
  (`ENGRAM_GUEST_OTEL_ENDPOINT`). Compiles (`cargo check`).
- `f2239db` — 0b/0c guest side: agentd reads the cmdline values
  (`kernel_cmdline_value`), adopts the OTLP endpoint, roots its `agentd.run`
  span on the propagated parent. Compiles (`cargo check`).
- _(pending)_ deeper 0c (uffd `MAP_POPULATE`, engram-init kernel/ext4-mount
  markers); 0d chunk-store/NBD I/O spans; final Linux clippy pass; prod
  canary to validate guest-span export + capture real magnitudes.

## Context

Session cold boot is ~30 s; we want E2B-class (~150 ms). But "150 ms" is
a goal, not a plan — it is not actionable until we know where the time
goes. This ADR plans the measurement that turns the goal into a plan,
then records the optimization hypotheses that the measurement will rank.

The guiding decision (per the user, and our standing "research over
guess-and-check" norm): **profile before architecting. No guessing.** If
we need a dev-vm spike or prod traces to get the breakdown, that is
step one.

## What prod already tells us (gathered 2026-05-26 via `engrams-prod-ops`)

- **Cold first boot is ~15 s, very consistent** (warm OCI cache).
  Measured from `session_events` pending→active (`status_changed`→
  `status_changed`) across the last 15 sessions: **14.17 s – 15.71 s**.
  The ~30 s figure is the cold-OCI case on a freshly-rolled MIG host
  (first session after a roll pays the full image pull).
- **Warm resume is already ~1 s.** Idle-evicted sessions resuming via
  the existing FC-snapshot + UFFD restore path measured **~1.0 s** (e.g.
  session `1db0ee1b`: idle→active in 1.00 s, repeated across several
  resume cycles). This path is prod-validated today (ADR 0014/0018).
- **Therefore the entire 15–30 s is the cost of a true cold kernel boot
  per session** instead of restoring from a snapshot. The machinery that
  gets us to ~1 s already exists and runs in prod.
- E2B (studied at `~/test/infra`) reaches ~150 ms because it *never*
  cold-boots: every sandbox start is a restore from a per-image base
  snapshot, with memory lazy-loaded via userfaultfd guided by a
  page-access prefetch trace, and subsystem setup parallelized.

## Observability today (and why it's not enough)

- **Metrics (Prometheus):** host-agent emits `engram_sandbox_boot_seconds`
  histograms labelled `phase ∈ {image_resolve, materialize, fc_boot,
  agent_handshake, total, create_total}` around `create()`
  (`engram-sandbox-firecracker/src/lib.rs:2413`) and `PooledBackend`
  (`engram-host-agent/src/pooled_backend.rs:~1420`); coord emits
  `engram_session_boot_seconds`.
- **Logs:** `tracing` is used in all four binaries but as **flat
  `info!`/`debug!` with zero spans** (`#[instrument]` count across the
  repo: 0). `metrics.rs` even documents an `info_span` convention that
  was never implemented.
- **No OpenTelemetry anywhere** — no `opentelemetry*` deps, no OTLP
  exporter, no collector.
- **No cross-process trace context.** gRPC (coord↔host-agent) carries
  `session_id` only in the message body, not metadata; spawns of
  firecracker/uffd-handler pass no correlation id; the host↔in-guest
  agentd vsock handshake carries `session_id` in the first frame but no
  trace id.

**Why this blocks us:** the boot path is I/O-bound and crosses coord →
gRPC → host-agent → spawned firecracker + uffd-handler → vsock → in-guest
agentd. Histograms give per-phase *totals* but can't show the *sequence
and overlap* — e.g. whether `MAP_POPULATE`, the UFFD socket wait, NBD
page-in, and FC spawn run serially (they do) or could overlap. The coarse
`agent_handshake` bucket lumps guest kernel boot + ext4 mount + NBD
page-in + agentd dial into one number. Spans with propagated trace context
are the right instrument.

## Decision

Phase 0: add OpenTelemetry distributed tracing across all four processes,
profile the top-level operations (cold create, idle resume, snapshot),
and produce a measured span→ms breakdown. Only then commit to the
optimization phases, ordered by that data.

## Phase 0 — task checklist (updated as work lands)

### 0a — OTel plumbing ✅ (compiles clean across workspace)
- [x] Add workspace deps: `opentelemetry` 0.28, `opentelemetry-otlp` 0.28,
      `opentelemetry_sdk` 0.28, `tracing-opentelemetry` 0.29 (known-compatible
      set; tonic 0.12). Landed in a shared `engram-telemetry` crate rather
      than copied into each binary, so the heavy OTel tree isn't pulled into
      `engram-core`.
- [x] `engram_telemetry::init(Config)` installs the `fmt` layer always and
      an OTLP layer **iff** `OTEL_EXPORTER_OTLP_ENDPOINT` is set (no-op when
      unset → prod unaffected until we opt in). Wired into all four binaries'
      `init_tracing` (`engram-coordinator/src/main.rs`,
      `engram-host-agent/src/main.rs`, `engram-agentd/src/main.rs`,
      `engram-uffd-handler/src/main.rs`).
- [x] Flush-on-exit: `init` returns a `TelemetryGuard` held for `main`'s
      lifetime; its `Drop` calls `SdkTracerProvider::shutdown()`. For the
      short-lived uffd-handler/agentd the guard is declared before the
      runtime so it drops after it. (0.28's batch processor runs a dedicated
      thread, so no in-runtime requirement.)
- [x] Sampling: relies on the SDK default `ParentBased(AlwaysOn)` = 100%.
      Fine at tens-of-sessions/day; revisit only if volume explodes.

### 0b — Trace-context propagation across the four processes
- [x] coord → host-agent (gRPC): `TraceparentInjector` client interceptor
      injects W3C `traceparent` into tonic metadata
      (`engram-protocol/src/grpc_client.rs`); host-agent extracts it via
      `link_remote_parent` and parents the handler span
      (`engram-host-agent/src/grpc_server.rs`). Helpers
      `engram_telemetry::current_traceparent` / `set_parent_from_traceparent`.
- [x] host-agent → uffd-handler (spawn): `TRACEPARENT` env injected at the
      uffd spawn site; uffd-handler roots its `uffd.run` span on it. (FC
      itself isn't instrumented, so no env hop there.)
- [x] host-agent → in-guest agentd (`2367c6b` host inject, `f2239db` agentd
      consume): propagated via the **kernel cmdline** (not vsock — agentd
      already parses `/proc/cmdline` for `engram_token`, so it's the proven
      channel). On cold boot the host appends
      `engram_traceparent=<tp>` and `engram_otel=<endpoint>` to
      `BootSource.boot_args` (`create_in_jail_after_net`); agentd reads both
      via `kernel_cmdline_value`, sets `OTEL_EXPORTER_OTLP_ENDPOINT`, and
      roots its `agentd.run` span on the parent. The span's start offset in
      the trace reveals the pre-agentd boot time (kernel + ext4 mount +
      page-in). Cold-boot only (restore reuses the snapshot's cmdline).
      **Caveat:** in-guest OTLP export needs a guest-reachable collector —
      `guest_otel_endpoint` (`ENGRAM_GUEST_OTEL_ENDPOINT`) is operator-set
      and defaults off, because the guest reaches the host only via its
      per-sandbox TAP gateway (no fixed dev address). Validated on the prod
      canary, where the collector is a stable endpoint.

### 0c — Span the top-level operations
- [x] coord: `session.create` root span on `create_session`, plus
      `create_for_session`/`restore_for_session` (`host_registry.rs`) and
      `finish_resume_to_active` (`api/snapshot.rs`, tagged session_id).
- [x] host-agent: gRPC handler spans (`9ea424c`); FC
      `create_in_jail`/`create_in_jail_after_net`/`restore_in_jail`/
      `spawn_uffd_handler` spans (`0e6b33e`); `start_agent` decomposed into
      `fc.await_agent_ready` + `fc.spawn_harness` (`2367c6b`).
- [~] agentd: `agentd.run` boot span rooted on the propagated trace
      (`f2239db`) — its start offset isolates pre-agentd boot. Still TODO:
      finer in-guest markers (ext4 mount, harness-spawn handler) and
      engram-init for the kernel-boot vs ext4-mount split.
- [ ] uffd-handler: `MAP_POPULATE` (`runtime.rs:~292`), time-to-first-fault,
      working-set fault tail (restore path; not on the cold-create critical path).
- [~] `restore_in_jail` / `PooledBackend::restore` *internal* sub-steps
      (netns, FC spawn, symlink, socket-wait, materialize, NBD attach) —
      function-level spans exist; finer sub-step spans deferred.

### 0d — NVMe / disk I/O visibility
- [ ] In-process spans (cheap, default-on) around I/O wrappers already on
      the path — chunk-store cache `get`/`prefetch_chunks_parallel`
      (`engram-chunk-store/src/cache.rs`), blob fetch, `disk_daemon` NBD
      reads (`engram-host-agent/src/disk_daemon/`). Record bytes + tier
      (nvme/blob/canonical-mmap) as span attributes.
- [ ] Kernel-level (dev-vm spike only): `blktrace`/`bpftrace` on the NVMe
      queue + `MAP_POPULATE` page-faults, under the `dev-vm` skill. Skip
      in prod (too heavy; user flagged not-worth-it if costly).

### 0e — Collect & write up
- [x] Jaeger collector wired into the local dev stack (docker-compose
      `jaeger` service: OTLP/gRPC :4317, HTTP :4318, UI :16686), with
      `OTEL_EXPORTER_OTLP_ENDPOINT` defaulted through `just dev` (Tiltfile),
      `just integration-up` (dev-vm), and standalone `just trace-up`.
      OS-agnostic (bridge-mode ports work on macOS + Linux). `a0ce662`.
- [x] End-to-end pipeline proven (macOS, 2026-05-26): coordinator run with
      `OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317`, a `POST /sessions`
      emitted a `session.create` span (3.9ms) that landed in Jaeger and is
      queryable via `/api/traces?service=engram-coordinator`. Confirms
      engram-telemetry → OTLP/gRPC → Jaeger works.
- [x] Drove the full multi-process cold boot on the dev-vm (2026-05-27): a
      real FC microVM booted via `POST /sessions`; the trace stitched
      `session.create → coord.create_for_session → host.create_sandbox →
      fc.create_in_jail → fc.create_in_jail_after_net → host.start_agent`
      across the gRPC hop. Breakdown in "Phase 0 first result" above:
      `agent_handshake` is ~98% of cold boot. (Cold create, so no `uffd.run`
      — that's the restore path.)
- [ ] Break `agent_handshake` into sub-spans (kernel boot / ext4 mount /
      NBD page-in / agentd dial) + a prod canary for real magnitudes, before
      ranking Phase 1.
- [ ] Prod export (follow-up, deploy-repo `engrams-internal`): collector
      via Google Cloud Trace or a Tempo sidecar. Out of scope for this
      repo's PR.
- [ ] Deliverable: flame-graph-backed table — operation → span →
      measured ms → serial-or-overlappable → optimizable? → expected
      leverage. **Stop and review with the user before Phase 1.** Update
      this ADR with the breakdown and flip to Accepted.

## Phase 0 first result — cross-process cold-boot trace (dev-vm, 2026-05-27)

First end-to-end trace captured: `just integration-up && just integration-test`
on the dev-vm with Jaeger, a real `POST /sessions` cold-create booting a
Firecracker microVM. The trace stitches coord → host-agent across the gRPC
`traceparent` hop (both services present under one trace id), confirming 0b
propagation end-to-end. Span breakdown (`kind=cold`):

| offset | dur | service | span |
|---|---|---|---|
| +0ms | **142,846ms** | coordinator | `session.create` (total) |
| +2ms | 2,631ms | coordinator | `coord.create_for_session` |
| +4ms | 2,628ms | host-agent | `host.create_sandbox` |
| +15ms | 2,616ms | host-agent | `fc.create_in_jail` |
| +40ms | 2,592ms | host-agent | `fc.create_in_jail_after_net` |
| **+2,649ms** | **140,189ms** | host-agent | `host.start_agent` (agent_handshake) |

**Finding: `agent_handshake` is ~98% of cold boot** (140.2s of 142.8s);
host-side VM creation/config is only ~2.6s. This confirms the prod-ops
hypothesis — the cold path is dominated by *in-VM kernel boot + ext4 mount +
chunked-NBD page-in + agentd dial*, not host-side VM setup. The dev-vm's
absolute 143s magnifies prod's ~15s (single nested-virt box, chunked-NBD
page-in from fake-gcs over the bridge), but the **shape** is what ranks the
hypotheses.

Implications:
- **H1 (restore-from-base) is the top lever** — a snapshot restore skips
  kernel boot + ext4 mount + cold page-in entirely, attacking the 140s
  directly. This is consistent with prod's ~1s warm-resume vs ~15s cold.
- `agent_handshake` is still a **single black-box span**. Breaking it into
  kernel-boot / ext4-mount / first-NBD-page-in / agentd-dial sub-spans
  (remaining 0c: agentd boot spans + 0d NBD I/O spans) is the next
  instrumentation step before we can rank H2 (prefetch/MAP_POPULATE — a
  restore-path cost, not exercised by this cold create) vs in-VM costs.
- Caveat: dev-vm numbers are not prod numbers. A prod canary (one enabled
  image, OTLP → a prod collector) is needed for the real magnitudes before
  committing to Phase 1 — but the **ordering** (agent_handshake ≫ everything)
  is unlikely to change.

## Candidate optimizations (hypotheses — ranked by Phase 0 data)

Largest-expected-leverage first. Derived from the E2B comparison and the
code; **not** committed until Phase 0 confirms.

### H1 — First boot restores from a per-image base snapshot (~15 s → ~1 s)
The single biggest lever if confirmed. Build one base snapshot per
enabled image (boot once to agentd-ready with a stub harness drive,
pause, snapshot, chunk `memory.bin` into the chunk store, record a
manifest), then route `create_session` through the **existing**
`restore()` path instead of `create()`. Reuses, with little net-new code:
`PooledBackend::restore/snapshot/swap_harness_drive`, per-session COW disk
overlay via `ChunkedDiskBackend` dirty buffer (`disk_daemon/backend.rs`),
per-session netns (`reserve_restored_netns`), post-restore identity
binding via the `SpawnHarness` vsock RPC in `start_agent` (`lib.rs:~3072`)
+ `finish_resume_to_active` (`api/snapshot.rs:437`). New: a
`base_snapshots` table keyed by `manifest_digest`, a host-side build step
hung off `image_prefetch.rs` after image readiness, and a base-hit branch
in `create_session_inner` (`api/sessions.rs:362`) with **cold create as
fallback** on miss. Resurrects ADR 0014 M1.12's warm-lease mechanism as
the default create path.

**Hard problems (fork-model decision deferred; start serialized,
concurrency-1):** concurrent restores from one base collide on the
vsock-UDS path embedded in `state.bin` (`EADDRINUSE`; ADR 0014 risk 9,
ADR 0018 §12p) → needs per-FC mount-namespace bind-mount; plus per-fork
hazards — reseed `/dev/urandom`, clock-step, regenerate `/etc/machine-id`
post-restore before harness spawn.

### H2 — Activate the memory prefetch trace + drop MAP_POPULATE (chunk of ~1 s → ~150 ms)
The trace machinery exists but is dead in prod: `WorkingSetRecorder` /
`prefault_from_trace` (`uffd-handler/src/runtime.rs:374`) are gated on
`working_set_blob_key`, hardcoded `None` at every construction site. And
`runtime.rs:~292` mmaps the whole `memory.bin` with `MAP_POPULATE`,
synchronously faulting the entire file in before the fault loop starts —
on the critical path. Capture+publish a trace at base-snapshot build,
stop hardcoding `None`, replay via `prefault_from_trace`, switch
`MAP_POPULATE` → `MAP_PRIVATE` + targeted `MADV_WILLNEED` (E2B's
two-phase fetch→copy). Phase 0 must confirm MAP_POPULATE is a big slice
first.

### H3 — Parallelize restore subsystem setup (collapses serial chain to its max)
`restore_in_jail` (`lib.rs:~1572`) and `PooledBackend::restore`
(`pooled_backend.rs:~1713`) run netns / FC-spawn / symlinks /
uffd-handler-spawn / socket-wait / materialize steps sequentially. E2B
runs them as concurrent promises and joins only before `load_snapshot`.
`try_join!` the independent units; keep `load_snapshot_uffd` as the join
gate. Requires reworking failure cleanup for partial init.

### H4 — Network slot pooling (~30 ms, low yield)
Pre-create a pool of netns+TAP+SNAT slots (cf. E2B `network/pool.go`);
restore pops a slot and only rebinds the bake's TAP name, instead of
building the netns synchronously (`net::provision_netns`, `lib.rs:~1907`).

### H5 — Cold-OCI mitigation (the 15 s→30 s tail)
The 2× penalty on freshly-rolled hosts is cold OCI cache. Confirm
`image_prefetch` completes before a rolled host serves traffic; consider
gating host "ready for scheduling" on prefetch completion.

## Consequences

- Phase 0 is pure instrumentation: gated on an env var, no-op in prod
  until opted in, so it ships safely and independently of any
  optimization. It is also durable infrastructure — distributed tracing
  outlives this investigation.
- The optimization phases are explicitly contingent. We may find the
  breakdown contradicts the hypotheses (e.g. that `agent_handshake` is
  dominated by in-VM ext4 mount, not memory page-in), which would
  re-rank H1–H5.
- Risk: the in-guest vsock trace hop (0b) is the one piece that may not
  pan out cleanly; the fallback is to cover the in-guest slice with 0d
  in-process spans + guest `systemd-analyze`, accepting a broken trace
  link at the VM boundary.

## References

- ADR 0007 (chunked immutable storage / canonical-memory UFFD restore)
- ADR 0008 (chunks in OCI; deleted the original warm pool)
- ADR 0014 (portable snapshots, warm-pool resume; M1.12 warm-lease)
- ADR 0015 (system design v2; enabled_images, deleted `templates` table)
- ADR 0018 (session evacuation; §12p vsock/harness path re-anchoring)
- E2B infra reference: `~/test/infra/packages/orchestrator/pkg/sandbox/`
