# ADR 0015: System design v2 — abstractions, not scab fixes

Status: 2026-05-22 — overall **Proposed (v2 direction)**; sections **M1
(in-VM service unification), M5 (host-image readiness), M2
(SessionState machine), and M3 (HostRegistry as a TTL'd cache over
PG)** Accepted + shipped. The remaining four sections stay Proposed
pending their own implementation ADRs.

M3 commit chain (in order):
- `6e74e71` — typed surface. `SandboxError::HostLost` joins the
  enum in `engram-core/src/error.rs`; coord adds `ApiError::HostLost`
  with its own `host_lost` slug (same 410 status as
  `ApiError::Gone(_)`, but a distinct machine-readable code so
  clients can tell "host disappeared, M4 may let you resume on a
  peer" from "snapshot lost forever, fork instead"). The
  `SessionState::HostLost` pre-flight in `api/snapshot.rs` switches
  from `Gone` to `HostLost` so the slug is honest before the
  read-through cache even runs. Host-agent's `sandbox_to_status`
  picks up an exhaustive arm — `failed_precondition`, defensive
  since host-agent never originates the variant.
- `7cdc3d9` — PG-authoritative lookup.
  `MetadataStore::host_for_sandbox(sb) -> Option<(HostId,
  SessionState)>` lets the registry resolve "who owns this
  sandbox right now, what state is its session in" in a single
  round-trip. Default impl scans `list_active_sessions` for the
  five in-crate test mocks; Postgres overrides with a single-row
  query. Migration `0032_sessions_sandbox_id_index.sql` adds the
  partial index `(sandbox_id) WHERE sandbox_id IS NOT NULL` —
  M2's transition path nulls `sandbox_id` on HostLost, so the
  partial form stays narrow as terminal rows accumulate.
- `7f1290f` — `HostRegistry` becomes a strict read-through cache
  over PG. New struct: holds `Arc<dyn MetadataStore>` + per-host
  `last_observed_heartbeat: AtomicI64` + configurable TTL
  (`ENGRAM_HOST_REGISTRY_TTL_SECS`, default 60s — under the
  dead-host detector's 30s threshold so the TTL fires only when
  detection itself is paused, the operator-paused case the M3
  open question called out). Synchronous `lookup` becomes async
  `resolve_owner`: fast path on cache hit + fresh host → return
  immediately; slow path drops the stale row, calls
  `host_for_sandbox`, surfaces `HostLost` for HostLost-class
  statuses, repairs the cache for live owners whose host is
  reachable, returns `NotFound` for rows PG doesn't know. New
  `invalidate_sandbox(sb)` primitive for per-session
  invalidation; refactored `unregister(host)` sweeps every
  `sandbox_owner` row pointing at the dying host (the previous
  "leave the row, it's cheap" comment was wrong once
  resolve_owner started returning 410 on stale cache rows). Six
  new unit tests cover the matrix; the existing
  `record_sandbox_owner_lets_existing_id_route_post_restart`
  gets updated to reflect PG-authoritative routing post-restart.
- `ed9f889` — wire `invalidate_sandbox` into every per-session
  HostLost transition site. `reconcile::flip_missing` invalidates
  the cache row *before* clearing `sessions.sandbox_id` in PG
  (drop-cache-then-DB; a racing reader either sees fresh PG
  state or a stale-but-about-to-fail cache hit, never the worst
  case where the cache fast path routes a post-flip exec to a
  HostLost sandbox). `preemption_drain::drain_session` does the
  same between `SandboxRegistry::unbind` and the best-effort
  `destroy`. `dead_host.rs` needs no further wiring — the
  `unregister(host)` sweep from the previous commit covers both
  the local detector and the cross-replica `host_dead`
  LISTEN/NOTIFY path automatically. New
  `reconcile_invalidates_host_registry_cache_on_host_lost`
  integration test proves selective per-sandbox invalidation
  (the flipped sandbox's cache row is gone; the still-alive
  sandbox's row survives).

Verification at the M3 boundary:
- `just check` clean (757/757 workspace tests, fmt + clippy).
- The 410 contract is now end-to-end: PG knows
  `SessionState::HostLost`, `MetadataStore::host_for_sandbox`
  reports it, `HostRegistry::resolve_owner` returns
  `SandboxError::HostLost`, the API mapping renders 410 with
  the `host_lost` slug. The pre-M3 stale-cache window where the
  same condition produced a `tcp connect error` from gRPC or a
  generic 404 is gone.
- The fallback TTL is lazy. There's no background sweeper —
  staleness is checked on the read path inside `resolve_owner`,
  so a paused host's cache entries become consistent with PG on
  the next request, not on a timer. Matches the ADR M3 "fallback
  TTL, not the primary mechanism" framing.

M2 commit chain (in order):
- `b0c8eca` — rename `SessionStatus` → `SessionState`; add `Created`,
  `GuestReady`, `HostLost` variants; add the legality table via
  `SessionState::can_transition_to` / `try_transition_to` +
  `IllegalTransition` carrying both sides. ~30 files ripple the
  rename. Zero behaviour change yet — every existing call site
  still uses `set_session_status` / `create_session_active`.
- `0d928e4` — replace `set_session_status` with the validated
  `transition_session(id, target) -> Result<prev, MetaError>`.
  Postgres impl does `SELECT ... FOR UPDATE` → `try_transition_to` →
  `UPDATE` in a single transaction; the row-level lock prevents two
  callers from validating against the same pre-state. Illegal
  transitions surface as `MetaError::Conflict` carrying the
  rendered `IllegalTransition`. Resume path
  (`resume_from_fc_snapshot`) now goes `Idle → Created → Active`
  instead of the prior illegal `Idle → Active`; if `start_agent`
  fails on resume the session is left at `Created` and the response
  surfaces the reattach failure rather than the prior
  warn-then-mark-Active. DELETE is idempotent for already-terminal
  sessions.
- `aa215f8` — flip the create path. `create_session_active` →
  `create_session_created`: row inserts at `Created` (not `Active`),
  and the handler transitions to `Active` only after `start_agent`
  returns OK. `ensure_active` tightens to return 409 for `Created` /
  `GuestReady`, 410 for `HostLost` / `Dead`, distinct messages per
  state instead of falling through.
- `d69d140` — route host-loss through `HostLost`.
  `mark_host_dead_and_reassign_sessions` → `_orphan_sessions`,
  return shape `Vec<(SessionId, SessionState)>` so callers emit
  honest `from` on `StatusChanged`. `dead_host.rs`,
  `preemption_drain.rs`, and `reconcile.rs` all now drive
  `Active → HostLost → {Idle if recoverable snapshot, Dead
  otherwise}` as two transitions instead of jumping straight to
  `Dead`. Also extends the legality table with the missing
  `HostLost → Idle` edge.
- `1734cb5` — drop the VZ `exec_stream` boot-race retry. Active now
  implies `start_agent` returned; the 10 s exponential backoff has
  nothing left to wait through. `send_prompt`'s harness-attach
  retry intentionally stays — different race (SpawnHarness ack vs
  the in-VM child dial-back), which M2 doesn't address.
- `ee3b5a8` — web app catches up. `SessionState` rename, new
  variants in `Glyph` / `PromptComposer` / `SessionManifest`,
  terminal-banner for `host_lost` and `failed`, "warming up" hint
  for `created` / `guest_ready`. Drops the stale `cold_evicted`
  variant.
- `42649dd` — dev-vm verification surfaced two bugs the unit tests
  didn't catch:
  1. Migration `0020_drop_cold_tier` had installed
     `sessions_status_check` with the pre-M2 status set;
     migration `0031_session_state_m2_variants` rebuilds it in
     place with all nine M2 variants.
  2. `delete_session` raced the reconciler — the heartbeat-driven
     `Active → HostLost → Dead` could land between `host.destroy()`
     and the DELETE's `transition_session(Completed)` call. Fixed
     by reordering: transition to `Completed` *first* (millisecond
     UPDATE; immediately drops the row out of the reconciler's
     `WHERE status='active'` filter), then destroy the sandbox
     best-effort. Conflict on a race-loss is treated as idempotent
     204.

Verification at the M2 boundary:
- `just check` clean (748/748 workspace tests, fmt + clippy).
- `just integration-test` on the dev-vm: bake → enable →
  prefetch-ready → cold-create → DELETE cycle green. The
  `session_events` table shows the new lifecycle:
  ```
  0 | status_changed | pending | created
  1 | status_changed | created | active
  2 | status_changed | active  | completed
  ```
  No spurious HostLost; DELETE wins the race over the reconciler
  because of the transition-first ordering.
- `Active` is now honest. The row reaches `Active` only after
  `start_agent` returns OK, so `/exec` / `/shell` / `/prompt`
  against an `Active` session no longer race agentd readiness.
  Earlier states return typed 409s with state-specific bodies
  (`session is created — agentd is not yet ready`, etc.) instead
  of falling through to a downstream handler that would race.

M5 commit chain (in order):
- `d20e5da` — retire `templates` table + warm pool across coord,
  host-agent, protocol, heartbeat (~4100 LOC net removed). New
  wire shape: `Heartbeat.ready_images: Vec<ManifestDigest>` +
  `HeartbeatAck.enabled_images: Vec<EnabledImageRef>`.
- `18be1ad` — retire the bake-time canonical capture path (the
  `--capture-canonical-memory` CLI family, `CanonicalCaptureConfig`,
  `Builder::capture_canonical_memory`, the OCI snapshot media
  types + `TemplateArtifacts` snapshot fields, the v3 bundle
  schema branch). Migration `0030_drop_templates.sql` lands here.
- `6fb36e2` — host-agent `image_prefetch` supervisor (watch-driven
  reconcile against the heartbeat-ack's enabled set, 16-permit
  shared semaphore, LRU recheck) + coord-side readiness gate
  (`PickError::ImageNotReady`, `SandboxError::ImageNotReady`,
  scheduler filter on `state.ready_images.contains(digest)`,
  HTTP 503 with operator-facing hint distinct from "no capacity").
- `7ab8625` — three field bugs the unit tests didn't catch:
  coord materialized the chunk manifest at a fresh UUID (must use
  bundle.json's ref); supervisor went through BlobStorage instead
  of teeing into the per-host `ChunkCache`; `ensure_image` returned
  stale cached digests on registry re-pushes. Integration test
  script rewired for the new flow.

Verification at the M5 boundary:
- `just check` clean (738/738 workspace tests, fmt + clippy).
- `just integration-test` on the dev-vm: bake → enable →
  prefetch → ready-gate → cold-create cycle green, with the
  templates table physically gone from PG.
- Cold-boot expectations: dev-vm at ~150 s (nested-KVM
  environmental, not regression); prod target ~3-5 s with
  chunks-already-local NVMe reads — same as the plan called for.

M1 commit chain (in order):
- `75ba2df` — collapse `engram-bootstrap` into `engram-agentd`
  (`WireRequest::SpawnHarness`, `HarnessSupervisor` module, atomic
  deletion of the bootstrap crate / constants / wire types / CLI
  flag / init-shim fork)
- `97de278` — `ProcessBackend::start_agent` empty-argv readiness
  short-circuit + drop straggling `bootstrap_binary: None` from test
  fixtures
- `0d444e9` — dev integration scripts stop building and injecting
  bootstrap
- `4b3f890` — replace the boot-race retry helper with a guest-
  initiated readiness dial. New `ENGRAM_AGENTD_READY_PORT = 1027`;
  agentd dials the host on startup; FC backend's `start_agent`
  blocks on a `tokio::sync::watch` filled by the per-sandbox accept
  task; `connect_fc_vsock_with_retry` deleted; `exec_stream` and
  `start_shell` revert to plain one-shot connects (their callers
  only run after Active, which now means agentd is provably bound)

Verification at the M1 boundary:
- `just check` clean (780/780 workspace tests, fmt + clippy).
- `just integration-test` on the dev-vm: full bake → enable →
  cascade → cold-create → cleanup cycle green.
- `just integration-session` (harness=none) + immediate `/exec`:
  60 ms wall, no `early eof`. The race the no-harness path used
  to produce is structurally impossible — every session goes
  through `start_agent`, which only returns after agentd's ready
  dial.

## Context

This ADR is the result of a system-walk-through and design conversation
on 2026-05-22, immediately following a week of cascading prod incidents
(warm-pool CPU vendor mismatch, host MIG rolls evicting active sessions,
cold-Active-before-agentd race, stale templates blob_not_found loops)
and one productive day of building out the dev-vm iteration loop. With
the loop now turnaround-fast — `just integration-up` + `just
integration-session` + the dev-vm port-forward give us full UI + API +
NBD locally in seconds — we have the bandwidth to look at the system
critically and ask which structural choices are causing the recurring
class of bugs we keep patching.

**What's working.** Several abstractions in the current codebase are
genuinely load-bearing and should be preserved as the model for
everything else:

- **`SandboxBackend` trait** — one set of code drives FC, VZ, and
  Process backends with no per-backend logic leaking into the
  coordinator. The reason `mode=all` and `mode=coordinator` share a
  single coord codebase is this trait.
- **`HostClient` trait** — one set of code dispatches sandbox ops in
  both the in-process (mode=all) and gRPC (mode=coordinator) cases.
  Adding a new RPC means adding one method, not threading wire
  formats through five layers.
- **Content-addressed chunked storage** (ADR 0007) — manifests,
  rootfs, memory, harness substrates all refer to identity, not
  location. Dedup is implicit. This is what makes warm-pool refill
  affordable and is the right pattern.
- **The `template_ref` value** (ADR 0014) — warm pool reasons about
  templates without coupling to OCI tags.

**What isn't working.** The codebase has accreted a number of
patterns that were each the right response to an immediate problem
but left underlying complexity in place. They form a class:

1. **Two in-VM processes with two wire protocols.** `engram-bootstrap`
   on vsock 1025 handles `BootstrapLaunch`; `engram-agentd` on 1024
   handles `WireRequest`. Both must be probed for readiness
   independently. The "is the session ready" question requires asking
   the right one, and the no-harness create path that skips bootstrap
   entirely produces the early-eof race we reproduced today.
2. **Fuzzy `SessionStatus::Active`** *(retired by M2).* What does
   Active mean? Sandbox created? Agent reachable? Harness running?
   Depended on call path. /exec, /shell, snapshot.ensure_active all
   interpreted it differently. M2 made Active a single typed state
   — set only after `start_agent` returns — with `Created` /
   `GuestReady` / `HostLost` as explicit intermediate states.
3. **Split-brain registry vs PG** *(retired by M3).* `HostRegistry`
   (in-memory `sandbox_id → host_id` map) and `Postgres.sessions`
   were both sources of truth. They diverged on MIG roll until the
   heartbeat timeout fired. M3 made PG authoritative and the
   registry a strict read-through cache: heartbeat-loss explicitly
   invalidates the cache; stale lookups fall through to a typed
   `SandboxError::HostLost` → HTTP 410 instead of `tcp connect
   error` or 404. A fallback TTL handles the operator-paused case
   the dead-host detector doesn't trip.
4. **Host-pinned sessions.** A session lives on one host for life.
   Host dies, session dies. Today this is policy; with snapshot/
   restore already implemented, it doesn't have to be.
5. **Bake-time canonical snapshot baked into the OCI artifact.** This
   is the design choice that made the AMD-baked → Intel-restored
   disaster possible. It conflates "what image content" with "what
   memory state at boot".
6. **Three wire protocols across the harness boundary** —
   `BootstrapLaunch` (host→bootstrap), `WireRequest::Exec` (host→
   agentd), `HarnessEvent` (harness→agentd→host→coord). They evolved
   independently; the dispatch glue between them is bespoke at each
   crossing.
7. **Path-baked FC `state.bin`.** FC's snapshot embeds host filesystem
   paths verbatim. We work around with a canonical-symlink layer
   (`<work_dir>/rootfs/<sandbox_id>.dev`) that any sibling host can
   re-create. The layer works but produces brittle one-off code (the
   relative-path-leaks-into-state.bin bug we fixed today is one
   manifestation; the symlink-rewrite on warm restore is another).
8. **`enable_image` is a non-atomic cascade.** Materialize → verify →
   cascade. Partial failure can leave a `templates` row pointing at
   blob keys that aren't in storage, and the host's warm-pool refill
   loop will spam `blob_not_found` for that row forever until manual
   intervention.
9. **Split host-coord channel.** HTTP/JSON from host → coord, gRPC
   from coord → host (ADR 0013). The split was made to keep coord
   pods stateless across restarts. The tradeoff: heartbeat is a
   POST every 5 s, not a stream; "host is going away" has no signal;
   lifecycle is fragmented across five HTTP handlers and a gRPC
   service.

**The framing this ADR adopts.** Each of these is an abstraction
opportunity, not a bug list. The right move for each is to identify
the trait or interface that subsumes the existing surfaces, ship the
new abstraction, and *delete* the code that motivated this ADR. Net
LOC should fall. We use `SandboxBackend` and `HostClient` as the bar:
they retire per-backend code, not pile on top of it.

**No backwards compat, no deprecated code.** Every milestone in this
ADR ships as an **atomic cutover**. There is no
"deprecated-but-tolerated" middle state, no `#[allow(deprecated)]`,
no hidden CLI flags retained for legacy CI, no `#[serde(default)]`
patches to keep old wire frames decodable, no stub binaries left for
old images to call. If a struct field, wire variant, RPC method, or
crate becomes unnecessary as a side effect of a milestone landing,
it is deleted in the same PR. All in-flight artifacts (bakes,
images, snapshots, FC `state.bin`s) that the cutover would break are
either re-generated in the same change set or accepted as dropped
state. The reason we accumulated the scab-fix codebase the v2
direction is trying to fix is that we kept landing changes "with
back-compat" — every deprecation slot turned into a load-bearing
permanent feature. Stop doing that. Every milestone implementor:
when in doubt, delete more, not less.

This ADR captures all nine moves so they exist as a coherent v2
direction. Only M1 ships in this PR; M2–M9 will spawn individual
implementation ADRs as we get to them. Some (notably M9) may end up
Rejected after closer examination — the goal is to make every one of
them a deliberate decision rather than a pile of latent work.

## Decision

Nine sections. Each is its own abstraction-introducing move. M1 is
Accepted and being implemented now; M2–M9 are Proposed.

| # | Abstraction | Status |
|---|---|---|
| M1 | `GuestService` — one in-VM RPC surface, one readiness signal | **Accepted + shipped** (commits `75ba2df` / `97de278` / `0d444e9` / `4b3f890`) |
| M2 | `SessionState` — explicit, gated state machine | **Accepted + shipped** (commits `b0c8eca` / `0d928e4` / `aa215f8` / `d69d140` / `1734cb5` / `ee3b5a8` / `42649dd`) |
| M3 | `HostRegistry` as a TTL'd cache of PG truth | **Accepted + shipped** (commits `6e74e71` / `7cdc3d9` / `7f1290f` / `ed9f889`) |
| M4 | `Sandbox` as a content-addressed migratable value | Accepted — ADR 0018 |
| M5 | Host-image readiness as the warm/cold contract — retire the `templates` table | **Accepted + shipped** (commits `d20e5da` / `18be1ad` / `6fb36e2` / `7ab8625`) |
| M6 | Harness events as one typed stream on `GuestService` | Proposed |
| M7 | FC drive references as content hashes via a `DriveResolver` trait | Proposed |
| M8 | Unified gRPC transport in both directions (retire HTTP/JSON host→coord) | Proposed |

**Note on table evolution.** The original ADR had nine sections, with
separate slots for "decouple ImageBundle from WarmTemplate" (M5) and
"enable_image as a saga + GC" (M8). The 2026-05-22 deep-dive on
`templates`'s dangling rows surfaced that both were band-aids on the
same underlying choice: that we have a `templates` table at all.
M5 and M8 are now collapsed into a single M5 milestone that retires
the table outright; the old M9 (coord-host channel) becomes M8. See
M5's body for the unified design and the [Insight: "delete the thing"](#insight-delete-the-thing-keeps-being-the-right-answer)
note at the end of the Consequences section.

---

### M1 — `GuestService`: one in-VM service surface

**Problem.** Two processes (`engram-bootstrap` on vsock 1025;
`engram-agentd` on 1024), two wire protocols (`BootstrapLaunch`;
`WireRequest::*`), two readiness signals. The host has to know which
process to probe for what (start_agent → bootstrap; exec/shell →
agentd) and which protocol to speak. Sessions with `harness: none`
skip the bootstrap dance entirely and produce the "Active before
agentd is reachable" race we reproduced on the dev-vm today. The
split exists because bootstrap predates agentd's exec handler — it's
not a design, it's a history.

**Abstraction.** One in-VM service exposing one `GuestService` trait,
mirror of `HostService` on the coord ↔ host side. The agentd crate
becomes its canonical implementation. The host calls into it via a
single `GuestClient` (existing `WireRequest` enum's variants become
the trait's method dispatch). Empty-argv `SpawnHarness` is a readiness
probe — any successful RPC proves the daemon is up.

**What it deletes.**

- The entire `engram-bootstrap` crate (~400 LOC) — crate dir, workspace
  member, all references in CI / justfile / scripts.
- `BOOTSTRAP_VSOCK_PORT`, `BOOTSTRAP_READY_BYTE`, `BootstrapLaunch`
  from `engram-harness-proto`. The `serde_json` dev-dep added for
  back-compat tests goes with them.
- The `&` fork in `DEFAULT_INIT_SHIM` in `engram-image-builder`,
  plus all stale comments referencing the two-process boot dance.
- The `bootstrap_binary` field on `AgentInjection` and the bake-
  injection branch that consumed it.
- The `--inject-bootstrap` CLI flag on `engram-cli image build`,
  along with its plumbing through `image_build_local`.
- The entire boot-race retry-loop pattern. Pre-M1 we had three
  copies of CONNECT-then-retry-on-early-eof (start_agent against
  bootstrap-1025, exec_stream against agentd-1024, start_shell
  against agentd-1024). After the readiness-dial design lands
  (commit `4b3f890`), `start_agent` blocks on a `tokio::sync::watch`
  filled when agentd dials the host on `ENGRAM_AGENTD_READY_PORT`;
  exec_stream and start_shell go back to plain one-shot connects
  because they only run after Active, and Active now means agentd
  is provably bound. `connect_fc_vsock_with_retry` deleted; no
  per-call deadline knobs anywhere.
- The VZ backend's BootstrapLaunch path —
  `engram-sandbox-vz::start_agent` switched to SpawnHarness on
  agentd-1024 like FC, and the `engram-harness-proto` dependency on
  the VZ crate is dropped.
- The `(None, _)` branch fork in `coord/api/sessions.rs` — both
  harness and no-harness sessions go through the same start_agent
  call; the early-eof race becomes structurally impossible.

**What gets added.**

- `WireRequest::SpawnHarness { argv, env, harness_dev, harness_mount }`
  + `WireResponse::HarnessSpawned { pid }` in
  `crates/engram-agentd/src/proto.rs`.
- `crates/engram-agentd/src/harness_supervisor.rs` — owns the harness
  child process handle; kill+respawn on subsequent `SpawnHarness`
  (needed for resume, identical semantics to bootstrap's existing
  loop).
- One new branch in `crates/engram-agentd/src/handler.rs::dispatch`.
- Harness-drive mount logic lifted verbatim from
  `engram-bootstrap/src/main.rs` into the new supervisor module.

**Migration.** Atomic cutover. The `engram-bootstrap` crate, the
`BOOTSTRAP_*` constants, `BootstrapLaunch`, the `--inject-bootstrap`
CLI flag, the `bootstrap_binary` field on `AgentInjection`, and the
init-shim fork are all deleted in this PR. Existing in-flight bakes
become unbootable; the new bake replaces them. Per the no-back-compat
principle above, we do not leave the bootstrap crate as a stub.

**Open questions.**

- Should we rename `agentd` → `agent` since it now does more than
  daemonize? No — it's still a long-lived daemon at PID 1. Naming
  unchanged.
- Should the SpawnHarness response carry the child pid? Yes — useful
  for guest-side debugging; trivial to populate.

**Expected net diff.** ~−500 LOC.

---

### M2 — `SessionState`: explicit, gated state machine

**Problem.** `SessionStatus` was `{Pending, Active, Idle, Completed,
Failed, Dead}`. Active fired whenever the host returned from create;
that didn't mean usable. Each call site (`/exec`, `/shell`,
`snapshot.ensure_active`) had its own interpretation and its own
retry/wait logic. The harness=none race was one symptom; the
"why does /shell sometimes fail right after create" complaint was
another. Worse, the create handler inserted the row directly at
`Active` *before* calling `start_agent`, so the column's truthfulness
was structurally limited.

**Abstraction (shipped).** A typed state machine where every
transition has a published meaning and goes through a single
validated entry point. `SessionState::Active` means: sandbox
exists, agentd is reachable, harness (if any) is running. `/exec` /
`/shell` / `/prompt` against a non-Active session return 409 with
the actual state, not a vsock race. States:

- `Pending` — request accepted, scheduler not yet returned (in-memory
  only; never persisted, never appears in a `WHERE status=...` query)
- `Created` — sandbox bound to a host; nothing else proven
- `GuestReady` — `start_agent` ready-dial fired (code-level only in
  the normal path; collapsed into the Active write because
  `start_agent` does both halves of the handshake in one RPC)
- `Active` — agentd reachable AND harness running (or harness=none
  and agentd ready). The only state in which `/exec` / `/shell` /
  `/prompt` proceed.
- `Idle` — snapshotted; `/resume` rehydrates
- `HostLost` — heartbeat-loss against the bound host. Non-terminal:
  M3 will wire the heartbeat-loss cache invalidation into this
  transition; M4 will add `HostLost → Created` on a peer host.
  Until then, the reconciler resolves `HostLost → Idle` (if a
  recoverable snapshot exists) or `HostLost → Dead` (otherwise).
- `Completed` — terminal (user-deleted)
- `Failed` — terminal (create failed mid-flight)
- `Dead` — terminal (chunked manifests gone or never were)

`SessionState::try_transition_to(target)` runs the legality table at
every persistence-layer write. The table lives in
`crates/engram-core/src/types/session.rs` and is the single source
of truth — no call site encodes its own preconditions. Illegal
transitions surface as `MetaError::Conflict` carrying the rendered
`IllegalTransition` (both `from` and `to`), so logs and HTTP 409
bodies show the actual collision instead of a generic "couldn't
update."

**Persistence + transition helper.** `MetadataStore::transition_session(id,
target)` replaces every direct `UPDATE sessions SET status = ...`
call site. The Postgres impl does `SELECT ... FOR UPDATE` →
`try_transition_to` → `UPDATE` in a single transaction; the
row-level lock makes "two concurrent callers both validate against
the same pre-state" structurally impossible. The trait method
returns the previous state so callers can emit honest
`StatusChanged { from: prev, to: target }` events without
reconstructing context.

**Create-path shape.** Per the latency-first ordering:

- No PG write before the scheduler returns (no orphan-row class,
  no reaper).
- First persisted state is `Created` (`create_session_created`
  replaces the prior `create_session_active`; insert with
  `host_id` + `sandbox_id` populated, `status='created'`).
- After `start_agent` succeeds, a second UPDATE transitions to
  `Active`. One extra UPDATE per create vs. before M2, in exchange
  for an `Active` that doesn't lie.
- `Pending` lives only as the `from` of the first `StatusChanged`
  event on the create path (the API caller's pre-insert view).

**Host-loss shape.** Both the dead-host detector and the
strike-based reconciler drive a two-stage transition per session:
`Active → HostLost` first, then `HostLost → Idle` (if a recoverable
snapshot exists) or `HostLost → Dead` (otherwise). Each stage emits
its own `StatusChanged` event. The bulk trait method
(`mark_host_dead_and_orphan_sessions`) returns `Vec<(SessionId,
SessionState)>` so the caller emits honest `from` values; the
Postgres impl reads the previous state inside the same UPDATE
via a `WITH ... FOR UPDATE` CTE.

**What it deleted.**

- `MetadataStore::set_session_status` (replaced by
  `transition_session`).
- VZ's `exec_stream` boot-race retry loop. `Active` now implies
  `start_agent` returned, which means agentd is bound on vsock 1024
  — the 10 s exponential backoff has nothing left to wait for.
- The "best-effort warn-then-mark-Active" behavior on the resume
  path's `start_agent` failure. The session now stays at `Created`
  if `start_agent` fails on resume, and the next `/exec` / `/shell`
  / `/prompt` returns a state-specific 409 instead of a downstream
  vsock error.

**What it added.**

- `SessionState` enum + `IllegalTransition` + `can_transition_to` /
  `try_transition_to` in `engram-core/src/types/session.rs`.
- `MetadataStore::transition_session` (trait + Postgres impl + five
  test mocks).
- Migration `0031_session_state_m2_variants.sql` — rebuilds
  `sessions_status_check` to accept all nine variants. (Dev-vm
  verification surfaced this; migration 0020's CHECK predated the
  M2 additions.)
- Two `StatusChanged` events per host-loss (the first lifecycle
  moment is "host went away" — a distinct signal from "session is
  unrecoverable"), giving M4 the seam it needs.
- DELETE handler reordering: transition to `Completed` *before*
  destroying the sandbox so the reconciler's
  `WHERE status='active'` filter immediately skips the row. Without
  this, `host.destroy()` could be slow enough that the reconciler
  flipped the row to HostLost mid-DELETE and the final transition
  failed with Conflict. Caught on the dev-vm; covered in the
  verification at the top of this ADR.

**Decision on typestate.** No. Sessions are DB-backed values
loaded by ID across requests, so typestate's compile-time
guarantees don't compose — every call site does a runtime
match-and-dispatch into a typed wrapper anyway. The runtime-checked
enum with `try_transition_to` gets the correctness payoff (illegal
transitions surface immediately in tests and logs) without the
generic-over-state-type viral propagation typestate forces on
function signatures.

---

### M3 — `HostRegistry` as a TTL'd cache over PG

**Problem.** `HostRegistry` kept `sandbox_id → host_id` in memory;
`Postgres.sessions` kept the same mapping on disk. They diverged on
MIG roll — the host vanished, the DB row was nulled by the M2
`HostLost` transition path, and the registry kept its stale entries
until the dead-host detector got around to dropping the whole host
(~30 s threshold). The divergent failure messages — `tcp connect
error` from gRPC, generic `sandbox not found` from the registry —
were two faces of this split brain.

**Abstraction (shipped).** PG is authoritative.
`HostRegistry.sandbox_owner` is a strict read-through cache that
holds an `Arc<dyn MetadataStore>` and calls
`MetadataStore::host_for_sandbox(sb)` on every miss. The lookup
helper became `async resolve_owner(sb) -> Result<(HostId, Arc<dyn
HostClient>), SandboxError>`:

- **Fast path** — cache hit + host currently registered + host's
  `last_observed_heartbeat` is within the TTL → return the
  backend, no DB round-trip. Steady-state shape for every
  well-behaved RPC.
- **Slow path** — PG returns the current owner + session status.
  HostLost-class statuses (`HostLost` / `Dead` / `Completed` /
  `Failed`) surface as `SandboxError::HostLost` → HTTP 410
  (`host_lost` slug). PG-known host that isn't in the registry
  also surfaces `HostLost` — we know who *should* serve and
  can't reach them. PG returns nothing → genuine `NotFound`
  → 404.

**Invalidation paths.** Three sites, all flowing through the same
M2 `HostLost` transition seam the previous milestone built:

- `dead_host::evict_host_with_lock` — the refactored
  `HostRegistry::unregister(host)` sweeps every `sandbox_owner`
  row pointing at the dying host. The same path covers the
  cross-replica fan-out: `pg_listener.rs` reacts to the
  `host_dead` NOTIFY by calling `unregister` on each replica.
- `reconcile::flip_missing` — invalidates the per-sandbox cache
  row *before* clearing `sessions.sandbox_id`. Order matters: a
  racing reader either gets the fresh PG state (HostLost → 410)
  or a stale-but-about-to-fail cache hit, never the worst case
  where the cache fast path routes a post-flip exec to a sandbox
  PG already knows is HostLost. The reconcile pass piggybacks on
  the `list_active_sandbox_assignments_on_host` query it already
  issues, so no extra round-trip.
- `preemption_drain::drain_session` — symmetric invalidation
  between `SandboxRegistry::unbind` and the best-effort
  `destroy`. Same reasoning: the VM is about to be reclaimed by
  the cloud; a request arriving mid-shutdown must not get
  routed to a destroying host.

**Resolution of the open question.** The original M3 sketch asked
"TTL or strictly invalidate-on-heartbeat-loss?" The answer
landed both: explicit invalidation is the primary mechanism (all
three sites above), and a lazy per-host TTL is the fallback for
hosts that have stopped heartbeating but haven't yet been
declared dead — operator pause, in-flight network partition, or
a paused dead-host detector. The TTL check lives inside
`resolve_owner` (no background sweeper); a host whose
`last_observed_heartbeat` is older than the configurable horizon
(`ENGRAM_HOST_REGISTRY_TTL_SECS`, default 60s) is treated as
unreachable on the next read. Default is intentionally over 2×
the 5s heartbeat cadence and under the 30s dead-host threshold,
so the TTL only fires when detection itself is paused.

**Decision on the registry shape.** `Arc<dyn MetadataStore>` lives
on the struct itself, not threaded through every call site. Every
`HostClient` impl method on `HostRegistry` already takes `&self`;
making `lookup` async + holding the store is a strictly local
change. The alternative — a separate `RoutingStore` wrapper that
the API layer holds alongside the registry — would have splintered
the routing surface across two types for no win.

**What it deleted.**

- The "leave the stale `sandbox_owner` row, it's cheap" comment
  on `unregister`. It wasn't cheap once `resolve_owner` started
  returning 410 on the basis of "cache says known-owner, owner
  is gone".
- The implicit assumption that `SandboxError::NotFound` covered
  every "couldn't reach the sandbox" case. `HostLost` is now its
  own typed branch with its own HTTP status and slug.

**What it added.**

- `SandboxError::HostLost` + `ApiError::HostLost(String)` with the
  `host_lost` slug (distinct from the existing
  `snapshot_invalidated` slug for snapshot-Dead cases).
- `MetadataStore::host_for_sandbox(sb) -> Option<(HostId,
  SessionState)>` — trait + Postgres impl + default for mocks.
- Migration `0032_sessions_sandbox_id_index.sql` — partial index
  `(sandbox_id) WHERE sandbox_id IS NOT NULL` for the per-miss
  lookup.
- Per-host `last_observed_heartbeat: AtomicI64` on `HostEntry`,
  bumped from `register` and `update_state` (the heartbeat
  handler's existing entry point).
- `HostRegistry::invalidate_sandbox(sb) -> Option<HostId>` — the
  per-session primitive the three transition sites call. Returns
  the prior owner for logging; idempotent on a missing row.
- `HostRegistry::new_with_ttl(meta, ttl)` (`#[cfg(test)]`) — lets
  the TTL fallback test exercise a sub-millisecond horizon
  without sleeping in CI.

---

### M4 — `Sandbox` as a migratable value

> **2026-05-25**: M4 is its own ADR. See
> [ADR 0018: Session evacuation](./0018-session-evacuation.md) for
> the as-shipped design + commit chain. The summary below stays as
> the original framing; ADR 0018 documents divergences (e.g. the
> trait method shipped as a documented seam rather than the
> orchestrator dispatch, the NBD-probe runtime wiring deferred to
> a follow-up, auto-triggers env-gated default-off pending the
> /resume-from-Created follow-up).

**Problem.** Today a session is pinned to one host for life. Host
dies → session dies. The snapshot/restore primitives are already
built (M1 of ADR 0014); they're used for warm pool but not for
session evacuation. With M3 in place (PG truth, cache invalidation),
the missing piece is policy: "when a host disappears, snapshot any
evacuable sessions to BlobStorage, restore them on a healthy peer".

**Abstraction.** `Sandbox` becomes a value whose identity (the
`sandbox_id` token in the registry / DB) can be substituted: snapshot
→ restore-on-peer → new sandbox_id, but same logical Sandbox from the
session's perspective. `HostClient::evacuate(sandbox_id, target_host)`
becomes a first-class op. Operator drains use the same primitive as
panic-evacuation.

**What it deletes.** The "your session vanished after a MIG roll"
UX class. Per-host operator drains as a separate concept.

**Open questions.** Eligibility: which sessions can be migrated?
Ones in `Active` state with a recent snapshot, probably. Idle
sessions are easier (snapshot already in BlobStorage from ADR 0014).
Cold sessions mid-boot might not be migratable; fail-loud is fine.

---

### M5 — Host-image readiness as the warm/cold contract (retires `templates`)

**Problem.** The `templates` table has been the source of two
recurring bug classes:

1. **Cross-CPU-vendor snapshot disasters.** The OCI artifact ships
   a bake-time canonical memory snapshot, and `templates.snapshot_id`
   points at it. When the bake host's CPU vendor differs from the
   restore host's (Blacksmith AMD bake → prod Intel restore, or the
   reverse), the snapshot's CPUID is incompatible and the warm-
   restored guest tripfaults on instructions the prod CPU doesn't
   support. Forced us to disable warm pool entirely in prod
   (`ENGRAM_WARM_POOL_DISABLED=1`, ADR 0014 follow-up).
2. **Dangling rows.** `templates` lives in Postgres; the blobs it
   references live in BlobStorage. Nothing reconciles them. Volume
   nuked, GC sweep, accidental key deletion — the row survives,
   the blobs don't, and host-agent's warm-pool refill loop spams
   `blob_not_found` forever. We dodged this in dev with `just
   integration-reset` (drops the postgres + fake-gcs volumes
   together); prod has no such option.

Reading the row column by column, almost nothing is load-bearing:
`template_ref` is derivable from the image's content hash;
`image_repo`/`image_tag` duplicate `enabled_images`;
`harness_pack_uri` is vestigial after option-D; `snapshot_id` is the
bake-time artifact we want to retire; `vcpus`/`memory_mib` belong on
the image manifest; `active` is bookkeeping for the rebake-replaces-
old pattern that wouldn't exist if warm-template state were host-
local.

**Abstraction.** Drop the table. Replace "is there a warm template
for image X" with a per-host readiness signal:

- **`enabled_images` is the only policy table.** It says "this image
  is allowed for sessions" and nothing else. No cascade, no derived
  rows.
- **Hosts converge against `enabled_images` via heartbeat sync.** On
  each heartbeat, the host diffs the coord's enabled set against
  its local NVMe chunk cache. For any image whose chunks aren't all
  present, the host kicks off a background prefetch that pulls the
  missing chunks from BlobStorage into NVMe. The prefetch reuses
  the existing `TieredChunkResolver` machinery — same code path
  the lazy-fault path uses today, just driven by an active loop.
- **Per-host readiness is a heartbeat-reported set.** Each
  heartbeat carries `ready_images: [content_hash, ...]` — the set
  of images for which every chunk is present locally. Coord
  aggregates: "image X has N ready hosts."
- **Scheduler routes only to ready hosts.** Session create resolves
  `image_uri → content_hash`, then picks any host that reports
  ready for that hash. If no host is ready, coord returns 503
  with "still warming up" (same shape as no-capacity).

The bake artifact is purely content: chunked rootfs + manifest. No
`state.bin`, no `memory.bin`, no `sidecar.json`. Push time drops
from ~30 s to ~3 s. CPU vendor stops mattering because there's no
captured memory state to be CPU-specific.

**Three-tier storage, all content-addressed:**

| Tier | What | Role |
|---|---|---|
| BlobStorage (GCS) | Canonical sha256-keyed chunks. Cross-fleet dedup. | Source of truth. Holds *only* image content; no snapshots. |
| Per-host NVMe chunk cache | Mirror of the chunks this host serves. Cross-image dedup within a host (every debian:bookworm-slim child shares base-layer chunks). | Fast local tier AND the inventory backing `ready_images`. |
| Kernel page cache | Hot pages resident across sessions. | Free; the kernel handles it. |

**What it deletes.**

- The `templates` table entirely — schema, migration, `template_ref`
  type, `upsert_template` / `list_active_templates` /
  `resolve_template` in `engram-postgres`.
- The bake's `--capture-canonical-memory` flag, the
  `CanonicalCaptureConfig` struct, the bake-time FC boot + snapshot
  capture pipeline, the embed-snapshot-in-OCI artifact path.
- `enable_image`'s `materialize_and_cascade` — no cascade to write;
  enable just inserts into `enabled_images`.
- The `snapshots` table's role as bake-artifact holder (it may
  still exist as a session-evacuation primitive once M4 lands;
  TBD).
- `WarmPool::observe_templates` and the active-template list in the
  heartbeat-ack — replaced by the host-side prefetch loop driven
  by `enabled_images`.
- The entire saga-rollback work the old M8 was scoped to. No table
  to keep consistent.
- The "no warm slot" failure mode papered with `ENGRAM_WARM_POOL_
  DISABLED=1` in prod today; warm pool simply doesn't exist as a
  concept after M5. It returns as a *separate, optional* future
  milestone (see "After M5" below).

**What it adds.**

- A `prefetch_loop` on the host-agent: reads enabled_images from
  coord on heartbeat, walks each enabled image's manifest, pulls
  missing chunks via the existing `TieredChunkResolver` into the
  local cache, updates the ready set.
- A `ready_images: Vec<ContentHash>` field on the heartbeat
  request.
- A coord-side aggregate (`HostRegistry::ready_hosts_for(hash)`) so
  the scheduler can pick a target host.
- A new error path on session create: "no host is ready for this
  image" (HTTP 503). Different semantics from the current
  no-capacity 503 — operators see "image is still being prefetched"
  not "fleet is full."

**Cold-boot expectations after M5 (no warm pool).**

| Phase | Today (prod, cold) | After M5 |
|---|---|---|
| FC create + InstanceStart | ~180 ms | ~180 ms |
| Kernel boot | ~2.5–3 s | ~2.5–3 s (slim-vmlinux future work targets <500 ms) |
| ext4 mount (rootfs page-in) | ~10–15 s (GCS round-trips) | ~100 ms (chunks local) |
| engram-init shim | ~150 ms | ~150 ms (future cleanup targets ~50 ms) |
| agentd bind + ready dial | ~120 ms | ~120 ms (future cleanup targets ~50 ms) |
| **Total** | **~17 s** | **~3.5 s** |

That's a 5× improvement on cold-boot from the chunk-prefetch alone,
which lands us in a position where the warm-pool re-introduction is
optimization layered on a healthy floor rather than load-bearing.

**After M5: returning the warm pool.**

Sub-1 s TTFM is still the goal. With M5 in place, that becomes:

1. **Slim vmlinux** (separate milestone, packer-image change) —
   strip unused subsystems, target Kata-style <500 ms boot.
2. **engram-init slim-down + agentd cold-start tuning** — replace
   the shell shim with a direct-syscall binary; tokio
   `current_thread`; bake the egress CA into a tiny ramdisk so the
   `/dev/vdb` tmp-mount disappears. Targets ~700–800 ms total
   cold-boot.
3. **Warm pool, take 2** — generate the memory snapshot *at runtime
   on each host* from the first cold-create per (host, image), then
   lease subsequent sessions from that local snapshot. CPU-vendor
   issue dissolves because each host snapshots on its own CPU.
   `templates` doesn't return; the local snapshot is host-private
   state in the host-agent process, surfaced through the same
   `ready_images` mechanism with a "with_warm_slot" flag.

Each of those is its own ADR; they don't need to land together.
M5 is the cleanest single step that unblocks all of them.

**Open questions.**

- Eviction policy when NVMe fills. LRU on chunks is what we have
  today; we'd extend the readiness signal to flip to false for
  any image whose chunks got evicted out from under it. Operator
  capacity-planning surface area, but cleaner than today's "warm
  slot count" model.
- Prefetch bandwidth. Hosts pulling chunks in parallel could
  saturate fleet egress from BlobStorage. Bound the per-host
  prefetch concurrency.
- What about post-M5 image *deletes* from `enabled_images`? Hosts
  see the image leave the set, then either: (a) immediately evict
  its chunks (frees NVMe, regret-on-re-enable), or (b) keep chunks
  cached and rely on LRU. (b) is what today's chunk cache does;
  keep it.
- Migration of in-flight prod images. The current `enabled_images`
  rows that have bake-time snapshots associated need a one-time
  re-enable to drop the snapshot references. Plan: M5's
  implementation includes a migration that strips `snapshot_id`
  from existing rows; old artifacts in BlobStorage age out via
  normal LRU.

**As-built notes (after the four-commit M5 chain landed).**

- **Prefetch concurrency: 16, not 4.** Wall-clock profiling on
  modern hosts showed 4 chunks-in-flight underutilizes the 10 Gbps
  NIC by ~70 %. The shipped supervisor uses a single
  `tokio::sync::Semaphore::new(16)` shared across all in-flight
  images, tunable via `ENGRAM_PREFETCH_CONCURRENCY`.
- **The `disk_manifest` ref is content-from-bake, not coord-
  minted.** First-pass implementation had coord generate a fresh
  `ManifestRef::new()` when materializing chunks into BlobStorage.
  The host's prefetch then read `bundle.json::disk_manifest`
  (the bake's UUID) and faulted with `blob not found`. Fix: coord
  parses bundle.json and writes the manifest at the bake's ref.
  The bake's UUID is therefore the canonical identifier; coord
  is a pass-through.
- **Prefetch must tee into the per-host `ChunkCache`.**
  `chunk_store.get_chunk(h)` returns bytes through the tiered
  resolver but doesn't populate tier 1. The supervisor wraps each
  get in `chunk_cache.prefetch(h, || chunk_store.get_chunk(h))`
  so the chunks land on local NVMe. Without this, session-create's
  `materialize_to_file_cached` re-paid a per-chunk BlobStorage
  round-trip even though the chunk was "ready."
- **Cache invalidation on registry re-push.** The host's
  `ImageCache::ensure_image(uri)` cached by URI string. When a
  `:warm-<sha>` tag was re-pushed (a different bake under the
  same tag), the host returned the prior digest's data and looked
  for stale chunk blobs. Fix: `ImageCache::invalidate_uri(uri)`
  drops the URI→digest map entry; the supervisor calls it when
  `cached.digest != EnabledImageRef.manifest_digest`. On-disk
  artifacts ride LRU rather than eager delete.
- **Distinct 503 reasons.** Session create now returns one of two
  503s: `SandboxError::ImageNotReady(digest)` for "no host has
  prefetched this image" (operator runbook: wait, check
  `/api/hosts`, check BlobStorage egress) and the existing
  capacity-fit 503 (operator runbook: wait for sessions to drain
  or for the MIG to scale up). `PickError` in `host_registry.rs`
  is the typed boundary; the API layer formats each with a
  different body.
- **`/api/hosts` exposes `ready_image_digests: Vec<String>`** so
  operators (and the integration test) can poll for a *specific*
  digest's readiness rather than just the count.

#### Known regression — chunk-store GC deleted (2026-05-23) — RESOLVED

Resolved by [ADR 0016 Phase C, shipped 2026-05-25](0016-cow-observability-and-continuous-sync.md#phase-c-as-built-notes-2026-05-25).
The redesigned chunk-GC unions four pin-set sources
(`enabled_images.disk_manifest_*`, `sessions.live_disk_manifest_*`,
recoverable `snapshots.{disk,memory}_manifest_*`), uses a
`chunk_generation` PG barrier to catch mid-sweep flush races, and
gates promote-pass deletes behind a 24h candidate-table grace
window — discharging all four design constraints originally
enumerated here. See ADR 0016 §"Phase C as-built notes" for the
commit chain and constraint-discharge mapping. The 2026-05-23
incident post-mortem (symptom, root cause, two compounding bugs in
the prior GC) is preserved in this section's git history.

---

### M6 — Harness events as one typed stream on `GuestService`

**Problem.** Three protocols across the harness boundary:
`BootstrapLaunch` (host→bootstrap; gone after M1),
`WireRequest::Exec` (host→agentd, replies with exec event stream),
and `HarnessEvent` (harness→agentd→host→coord via a separate
JSON-shaped stream on a different vsock port). Two of them survive
M1, with bespoke dispatch glue between them.

**Abstraction.** All harness↔coord communication flows over the same
`GuestService` stream introduced in M1. `HarnessEvent` becomes a
typed message on the same trait, alongside `Exec` events and shell
bytes. Single multiplexed channel; coord's subscription path
collapses.

**What it deletes.** The separate harness-event vsock port + its
proxy; the JSON-on-vsock framing for harness events; ~one bespoke
streaming protocol.

**Open questions.** Backpressure semantics — the harness can produce
events faster than coord can drain them. Pick a bounded channel size
and document the spill policy.

---

### M7 — FC drive refs as content hashes, via `DriveResolver`

**Problem.** FC's `state.bin` embeds host filesystem paths verbatim
(rootfs path, harness path). We work around with the canonical-
symlink layer at `<work_dir>/rootfs/<sandbox_id>.dev`, which any
sibling host can rebuild on restore. The layer works but is brittle:
the relative-path-leaks-into-state.bin bug we fixed today (commit
95ad20a), the symlink-rewrite-on-warm-restore dance in
`swap_harness_drive`, and the entire "destroy must not touch the
canonical entries" contract are all evidence of the impedance
mismatch.

**Abstraction.** Wrap FC behind a small `DriveResolver` shim. State.
bin embeds a content-addressed `DriveRef` (the manifest hash). On
restore, the host's `DriveResolver` translates the hash to whatever
local path serves that content (NBD device, materialized file). The
canonical-symlink layer goes away; warm-restore stops needing a
symlink rewrite step; the relative-path bug class is structurally
impossible.

**What it deletes.** `paths::install_symlink` and its callers;
`canonical_parent_dirs`, `canonical_entries_for`, `assert_rootfs_canonical`;
the symlink rewrite in `swap_harness_drive`; the entire concept of
a "canonical path outside the jail".

**Open questions.** Does FC need to change to support this, or can
we wrap it? Most likely a wrapper around FC's APIs that lies about
paths (always tells FC to use a stable per-sandbox local path,
controls what's there via `DriveResolver`). Doesn't require upstream
FC changes.

---

### M8 — Unified gRPC transport in both directions

**Problem.** Per ADR 0013, host → coord is HTTP/JSON (heartbeat,
harness events, idle-eviction-candidates, live-manifest publish,
auth resolution) and coord → host is gRPC. The split was a
historical accident, not a design: HTTP came first because it was
the simplest thing that worked over the k8s LB; gRPC arrived later
for the reverse direction (ADR 0011 / 0013). The split kept the
coord pods stateless, which is load-bearing, but the transport
asymmetry itself isn't. Cost today:

- **Bespoke dispatch glue at every crossing.** Five HTTP handlers
  on the coord side, each with its own request/response struct,
  bearer-auth middleware, and JSON serde — sitting alongside the
  gRPC service that handles the reverse direction. Adding a new
  host→coord RPC means writing a route, a handler, a request type,
  a response type, and a host-side `reqwest` helper. Adding a
  coord→host RPC means one proto definition + two trait impls.
- **Two error-handling regimes.** HTTP errors are status code +
  body string; gRPC errors are typed status. Translating between
  them at the boundary is bespoke per handler.
- **Two wire-shape conventions.** Snake-case JSON on one side,
  protobuf on the other. Hand-maintained TS types in the web app
  have to track both.

**Abstraction.** Move host→coord onto **unary gRPC** alongside the
existing coord→host service. Both lanes use the same transport,
the same auth (mTLS or bearer-in-metadata), the same typed errors,
the same proto-generated client/server. Coord pods stay stateless:
unary gRPC load-balances over the GCP internal LB the same way
HTTP does (HTTP/2 frames, no client affinity). No long-lived
streams, no pod pinning — the structural property ADR 0013 was
built on stays intact.

**What it deletes.**

- `crates/engram-host-agent/src/coord_client.rs::CoordClient`'s
  `reqwest::Client` + bearer-header builders + `trim_ws_suffix` +
  the per-endpoint JSON serializers. Replaced by a generated gRPC
  client.
- The host→coord HTTP handlers in
  `crates/engram-coordinator/src/api/host_http.rs` (heartbeat,
  harness_event, idle-eviction-candidates, live-manifest, auth
  resolution). Replaced by methods on the existing host-facing
  gRPC service.
- The two-regime error translation at the host↔coord boundary.
  One `tonic::Status` taxonomy across both lanes.
- The hand-maintained JSON wire-shape types that exist solely for
  this channel.

**What it adds.**

- New `HostToCoordService` proto, with one RPC per existing
  host→coord HTTP endpoint. Same payload shapes (protobuf
  equivalent of today's JSON).
- One new gRPC server lane on coord, sharing the existing tonic
  router. Same `require_bearer` equivalent via a `tonic`
  interceptor.
- A host-side `CoordClient` that wraps the generated gRPC client
  with the per-RPC timeouts today's `reqwest::RequestBuilder`
  applies (heartbeat 30s; live-manifest 120s; idle-eviction-
  candidates 120s; per A.1.5a in ADR 0016).

**Explicit non-goals.**

- **No bidirectional streaming.** The "host opens a long-lived
  stream, coord serves reverse RPCs on it" shape is rejected:
  it would pin each host to one coord pod, break helm-deploy
  drains, and move half-open detection into our code. Unary gRPC
  both ways gets the transport-unification payoff without the
  stateful-pod cost. (See "Why not bidirectional streaming"
  below.)
- **No heartbeat shape change.** Heartbeat stays a 5s unary call
  per host, same payload. The fleet-scale overhead today is
  trivial (single-digit req/s); optimizing it isn't M8's job.
- **No web-app wire change.** The web app talks to coord's
  *public* HTTP API, not the host↔coord channel. Untouched.

**Why not bidirectional streaming.** A single bidi stream per host
gets the lifecycle unification ("one channel, one signal for host-
going-away") but:

1. Each host becomes pinned to a specific coord pod for the life
   of its stream. ADR 0013's stateless-pod property goes away.
2. Pod rolls on helm-deploy require an explicit drain handshake;
   today the LB handles it transparently.
3. Half-open detection moves into application code (gRPC
   keepalive + app-level health checks) — a new failure class.

The unification payoff doesn't justify those costs. If a future
operation genuinely needs push semantics (e.g. "drain
immediately"), add one streaming RPC for that operation, not a
stream for everything.

**Migration shape.** Atomic cutover per the no-back-compat
principle: drop the HTTP handlers + the `reqwest::Client` host
client in the same PR that ships the gRPC equivalents. In-flight
hosts and coord pods roll together via the existing deploy
pipeline; no "speak both lanes during transition" code.

**Open questions.**

- Should host→coord and coord→host share one tonic server on the
  host side (a single combined service definition) or stay as two
  separate services on shared infrastructure? Lean toward separate
  services with one shared interceptor stack — keeps the auth
  asymmetry honest (host→coord uses bearer; coord→host today is
  unauthenticated within the VPC).
- Auth: stay with bearer-in-metadata, or move to mTLS? Bearer
  matches today's setup and is enough; mTLS is a separate
  hardening pass.
- Sequencing relative to M4 (session evacuation). M4 doesn't
  block on M8 and vice versa; pick by team bandwidth, not
  dependency.

**Expected net diff.** Modest LOC reduction (a few hundred lines
of bespoke HTTP plumbing retired, replaced by generated gRPC stubs
and slimmer handlers). The win is structural — one transport
taxonomy across both lanes — not LOC.

---

## Consequences

**Net code.** We expect the v2 direction overall to remove on the
order of 2-3 kLOC of bespoke scab-fix code (canonical-symlink layer,
bootstrap process + protocol, per-call-site retry loops, the
`templates` table + cascade + GC). New abstractions add code (the
`GuestService` trait, the state machine module, the `DriveResolver`
shim, the host prefetch loop) but each is small. The shift is from
breadth-of-bespoke to depth-of-trait.

**Tests.** Several test suites collapse (bootstrap's tests → agentd's;
the per-call-site retry tests → state machine transition tests; the
canonical-symlink tests → DriveResolver tests; the
`materialize_and_cascade` + templates upsert + warm-pool refill
tests → host prefetch tests). New tests for the trait boundaries.

**What gets harder.** Cross-version compatibility — once M1 lands,
mixed old/new host-agents and bakes have a brief window of
incompatibility. The atomic-cutover migration plan in each section
covers this. M5 retires the bake-time snapshot artifact — there's a
one-time migration to strip `snapshot_id` from existing
`enabled_images` rows and drop the `templates` + `snapshots`-as-
bake-artifact tables. M7 (drive refs) requires careful state.bin
handling for in-flight session snapshots — likely a separate
migration script.

**What stays unchanged.** All the load-bearing existing abstractions:
`SandboxBackend`, `HostClient`, chunked storage. The v2 direction is
additive to those, not replacing them.

### Insight: "delete the thing" keeps being the right answer

Three structural improvements identified during the M1
implementation conversations followed the same pattern:

1. **bootstrap + agentd collapse.** Not "fix the bootstrap-ready
   race" → "delete bootstrap, fold its job into agentd."
2. **Boot-race retry helper.** Not "tune the retry deadline" →
   "delete the retry, have agentd dial the host on startup."
3. **`templates` table.** Not "harden enable_image with a saga +
   periodic GC" → "delete the table, hosts converge against
   `enabled_images` via heartbeat sync."

Each time the impulse was to patch around the symptom, and each
time the cleaner move was to identify the underlying abstraction
that shouldn't exist and remove it. The reason the codebase
accumulated scab-fix surface area is that we kept choosing
"harden" over "delete." Implementers of M2–M8 should hold this
prior: when the next layer of complexity feels like the only way
forward, look for the abstraction one layer down that could simply
not exist.

## Phased rollout

- **Phase 1 (shipped):** M1 — in-VM service unification. Commits
  `664cf61` (ADR) and `75ba2df` / `97de278` / `0d444e9` / `4b3f890`
  (implementation + readiness-dial follow-on).
- **Phase 2 (shipped 2026-05-22):** **M5 — Host-image readiness +
  retire `templates`.** Largest single simplification by code
  retired and biggest cold-boot win (~17 s → ~3.5 s prod target).
  Four-commit chain `d20e5da` / `18be1ad` / `6fb36e2` / `7ab8625`.
  Unblocks all subsequent warm-pool work.
- **Phase 3 (shipped 2026-05-22):** **M2 — `SessionState`
  machine.** `Active` becomes honest (only set after `start_agent`
  returns); host-loss routes through `HostLost`; every transition
  goes through one validated entry point. Seven-commit chain
  `b0c8eca` / `0d928e4` / `aa215f8` / `d69d140` / `1734cb5` /
  `ee3b5a8` / `42649dd`. Unblocks M3 (cache invalidation hooks
  into the existing `HostLost` transition) and M4 (session
  migration becomes `HostLost → Created` on a peer).
- **Phase 4 (shipped 2026-05-23):** **M3 — `HostRegistry` as a
  TTL'd read-through cache over PG.** PG becomes authoritative
  for sandbox-owner routing; the in-memory cache invalidates on
  the M2 `HostLost` transition; stale lookups surface
  `SandboxError::HostLost` → HTTP 410 instead of `tcp connect
  error` or 404. Four-commit chain `6e74e71` / `7cdc3d9` /
  `7f1290f` / `ed9f889`. Unblocks M4 (session migration is now a
  `HostLost → Created` transition on a peer host — the seam M2
  introduced is now invalidation-clean on both sides).
- **Phase 5 (cold-boot floor):** vmlinux slim-down + engram-init/
  agentd cold-start tuning. Not in this ADR — separate milestones
  layered on M5. Targets ~700–800 ms cold-boot.
- **Phase 6 (warm pool, take 2):** Host-local runtime snapshot
  generation, layered on the post-M5 storage model. Sub-1 s TTFM
  via warm lease; cold-create falls back to the Phase 5 floor on
  miss. CPU-vendor issue dissolved by construction.
- **Phase 7 (large refactors):** M4 (session migration), M6
  (unified harness stream), M7 (DriveResolver). Each is its own
  ADR + multi-PR effort. Sequencing depends on which bugs surface
  first.
- **Phase 8 (transport hygiene):** M8 — unary gRPC in both
  directions, retire HTTP/JSON host→coord. Pure transport
  cleanup; no behavioural change. Sequencing independent of M4.

Each phase ships its own implementation ADR. This document is the
v2 direction summary, not the implementation plan for any single
move beyond M1, M2, M3, and M5.
