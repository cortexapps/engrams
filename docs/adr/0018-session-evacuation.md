# ADR 0018: Session evacuation — `Sandbox` as a host-fungible value

Status: 2026-05-25 — **Proposed** (Phase 0). Multi-phase ADR; opens with
the finalized design and a per-phase commit chain. Status flips to
Accepted at the closing bookend once the chain has landed.

Supersedes nothing. Discharges ADR 0015 §M4 (session evacuation) and
folds the M4.1 follow-up filed in ADR 0017 §"Out of scope" (FC NBD-loss
recovery).

Phase chain (commit hashes filled in at the closing bookend):

- **Phase 0** — this ADR in Proposed status. _(this commit)_
- **Phase A** — evacuation primitive: `HostClient::evacuate` RPC, alive-source
  graceful drain, target-side restore, atomic PG rebind.
- **Phase B** — auto-triggers: dead-source path in `dead_host.rs` + FC
  NBD-loss path in host-agent heartbeat.
- **Phase C** — target-host selection policy, admin endpoint, Postgres
  integration + e2e tests.
- **ADR closing bookend** — as-built notes, divergences, flip to Accepted.

---

## Context

ADR 0016 Phase C closed on 2026-05-23 with three primitives that, taken
together, were M4's only remaining precondition:

1. **Continuous disk sync** (ADR 0016 Phase B). Every active session
   has `sessions.live_disk_manifest_*` populated in Postgres after
   each flush. The disk side of session state has a continuously-fresh
   pointer in PG that survives host crash.
2. **Pinned chunk lifetime** (ADR 0016 Phase C). The chunk-GC pin-set
   unions `sessions.live_disk_manifest_*` with `snapshots` and
   `enabled_images`; the 24h grace period absorbs sweep / publish
   races. Chunks referenced by a live disk manifest stay in
   BlobStorage long enough to be restored on a peer host.
3. **Clean host-agent restart** (ADR 0017). NBD device pool no longer
   leaks slots on crash / OOM / panic. Restart is routine, so drain
   is now about session relocation — not also about recovering from
   leaked device state.

ADR 0015 §M4 placed session evacuation in "Phase 7 — large refactors,
each its own ADR + multi-PR effort." This is that ADR.

The user-visible failure mode this discharges: today, when a host
disappears (MIG roll, OOM, network partition that breaks the heartbeat
threshold, kernel NBD wedge), every session bound to it gets driven
through `HostLost → Idle` (best case, snapshot exists) or
`HostLost → Dead` (no snapshot). Either way the session that was
running stops running until the user explicitly /resumes. With M4, the
session relocates automatically and the user sees a session that
keeps running on a different host — no UI flicker, no manual /resume.

ADR 0017 §"Why not deferred to M4.1" noted that M4 doesn't help with
the *ungraceful* exit class (M4 needs a working drain on the source,
which crashes don't run). That observation is true and complementary —
M4 covers the operator-drain path with full memory preservation, the
dead-source path with disk preservation only, and the FC NBD-loss path
which behaves like a per-sandbox dead source.

### What ADR 0015 already named

Verbatim from §M4 (ADR 0015 lines 686–702):

> `Sandbox` becomes a value whose identity (the `sandbox_id` token in
> the registry / DB) can be substituted: snapshot → restore-on-peer →
> new sandbox_id, but same logical Sandbox from the session's
> perspective.
>
> First-class operation: `HostClient::evacuate(sandbox_id, target_host)`.

ADR 0015 §M2 (lines 477–479) and §M3 (line 1181) already named the
seam: `HostLost → Created` is the transition M4 takes on a peer host,
and M3 left it invalidation-clean on both sides via PG-authoritative
`host_for_sandbox` + `HostRegistry::invalidate_sandbox`. The legality
table in `crates/engram-core/src/types/session.rs:99,124` already
permits `HostLost → Created` — the state-machine seam M2 introduced is
intact and waiting.

### What ADR 0016 already promised

ADR 0016 §"What gets easier" (lines 2039–2043):

> M4.1 (evacuation) can lean on `sessions.live_disk_manifest_*` instead
> of forcing a fresh snapshot on the source host. The host is alive →
> take a fresh memory snapshot only (~1–4 GiB) and use the existing
> live disk manifest. The host is dead → restore disk from the live
> manifest and accept memory loss (or use the most recent snapshot
> row's memory manifest if available).

That's the tiered memory policy this ADR adopts verbatim.

---

## Decision

One ADR, three phases, full scope across the four design forks:

### Scope decision 1 — Both paths + admin endpoint in one ADR

This ADR ships both the graceful-drain primitive (alive source, fresh
memory snapshot + reuse live disk manifest) and the dead-source
recovery path (`HostLost → Created` on peer, driven from
`dead_host.rs`'s second-stage transition). The admin endpoint
`POST /api/admin/sessions/:id/evacuate` is the test seam per
`[explicit_admin_triggers_for_testability]` — implicit triggers (dead-
host detector, FC NBD-loss) and explicit triggers (operator drain)
fire the same primitive.

### Scope decision 2 — Full target-host selection policy

`HostRegistry::pick_for_session` (the richer scheduler at
`crates/engram-coordinator/src/host_registry.rs:330+`, not the trivial
`scheduler.rs:9–19`) gains evacuation awareness:

1. **Exclude source host** via a new `ScheduleContext::exclude_host`
   field. The evacuation primitive sets this; no other caller does.
2. **Prefer image-cache-warm hosts.** Today `pick_for_session` already
   filters by `ready_images.contains(digest)`; evac promotes that from
   filter to ranking — hosts with the image warm rank above those that
   would have to prefetch. (The filter stays as a hard floor so we
   never schedule against a host that can't honour the boot.)
3. **Prefer same zone** when host records carry a zone label. Zone is
   today informational on `HostRecord`; promoting it to a ranking
   input costs nothing and meaningfully reduces evac latency by
   keeping chunk fetches in-zone.
4. **Largest free `memory_mib` among ties.**

Selection is a pure extension of the existing scheduler; no parallel
scheduler is introduced.

### Scope decision 3 — FC NBD-loss trigger in scope

ADR 0017 §"Out of scope" filed FC NBD-loss-triggered eviction for
M4.1. This ADR folds it in. The slot probe today
(`crates/engram-host-agent/src/disk_daemon/slot.rs:35–71` —
`nbd_kernel_busy`) is one-shot at acquire time. Phase B extends it
into a runtime health surface: a background task probes per-attached
NBD device on a periodic cadence (default 5s, aligned with heartbeat),
and any device whose state diverges from "bound + alive" for ≥3
consecutive probes promotes the bound sandbox into a
`heartbeat.nbd_unhealthy` list. Coord interprets membership in that
list as a per-sandbox `HostLost` equivalent and fires the evac
primitive against a peer host.

### Scope decision 4 — Tiered memory policy

Adopted verbatim from ADR 0016 §"What gets easier":

| Source state                            | Memory recovery                                       |
|-----------------------------------------|-------------------------------------------------------|
| Alive (operator drain / NBD-loss)        | Fresh memory snapshot via existing `host.snapshot()` |
| Dead, recent recoverable snapshot       | Restore from most-recent recoverable snapshot row    |
| Dead, no snapshot                       | Restore disk only, `loss=memory` flag, loud warning  |

The disk path is identical in all three cases: read
`sessions.live_disk_manifest_*` (or fall back to the latest
recoverable snapshot's disk manifest) and restore on the target via
the existing `resume_from_fc_snapshot` flow.

### Why a separate ADR

ADR 0015 already pre-budgeted this: M4 was explicitly Phase 7 (each
its own ADR). Folding the work into ADR 0015 itself would bloat the
already-large system-design doc; reopening ADR 0016 would conflate
COW-observability + continuous-sync surface with the higher-level
"`Sandbox` is migratable" abstraction. Cross-references stay tight:
this ADR's §Context links back to 0015 §M4, 0016 §"What gets easier",
and 0017's M4.1 references; the closing bookend updates ADR 0015 §M4
with a forward pointer.

### Out of scope (filed forward)

- **Cold-session evacuation.** Sessions in `Created` / `GuestReady`
  (mid-boot, no snapshot yet) remain non-evacuable. ADR 0015 §M4
  explicitly OK'd fail-loud for cold sessions. Revisit if cold-boot
  windows lengthen materially.
- **Continuous memory sync.** Disk has it (ADR 0016 Phase B); memory
  RPO is still snapshot-bounded. A future ADR could continuous-sync
  memory dirty pages, making alive-source evac essentially zero-loss
  without the on-drain memory snapshot. Today the on-drain snapshot
  is fast enough (~1–4 GiB → ~1–4s on local NVMe → ~5–20s upload to
  BlobStorage) that this isn't on the critical path.
- **Multi-host drain orchestration.** This ADR ships the per-session
  primitive. The "drain this whole host" loop (operator interrupts N
  sessions in parallel against target capacity) is a thin wrapper
  that can land in a follow-up.
- **Drain SIGTERM trap on host-agent.** ADR 0017 §"Explicit non-goals"
  noted graceful host-agent shutdown signaling as separate work. M4
  doesn't change that.

---

## Plan

### Phase A — evacuation primitive (alive source)

`HostClient` trait gains:

```rust
async fn evacuate(
    &self,
    sandbox_id: SandboxId,
    target_host: HostId,
) -> Result<EvacReceipt, SandboxError>;
```

`EvacReceipt { new_host_id: HostId, new_sandbox_id: SandboxId, loss: EvacLoss }`,
where `EvacLoss` is one of `None` | `Memory { reason: &'static str }`.

`HostRegistry`'s impl orchestrates:

1. Look up source via `meta.host_for_sandbox(sandbox_id)` (M3
   primitive; `crates/engram-core/src/traits/metadata.rs:198–209`).
2. Pre-rebind invalidate the routing cache:
   `host_registry.invalidate_sandbox(sandbox_id)` (M3 primitive;
   `host_registry.rs:296`).
3. Source-side: `source_host.snapshot(sandbox_id)`. This produces
   `SnapshotMetadata` carrying disk + memory manifest refs. The disk
   manifest will match (or supersede) the current
   `live_disk_manifest_*`; either way the snapshot path is the same
   as today's evict-to-snapshot. Coord records the snapshot row via
   the existing `record_snapshot` path so the chunk-GC pin-set keeps
   the chunks alive past evac (Phase C's pin-set already unions
   snapshots).
4. Target-side: `target_host.restore(metadata)` — existing primitive
   in the `HostClient` trait. Returns the new `SandboxId`.
5. PG rebind in TX: new `MetadataStore::rebind_session` updates
   `sessions.host_id` + `sessions.sandbox_id`, drives the state
   machine through `HostLost → Created → Active` (the latter two
   transitions are already legal; the rebind TX takes the lock and
   fires both via the existing `transition_session` primitive), and
   emits the paired `StatusChanged` events.
6. Cleanup on the source: best-effort `source_host.destroy(old_sandbox_id)`.
   Failures logged; the sandbox is now orphaned on the source and
   either the idle evictor cleans it up or host-agent restart will.

Failure semantics: the snapshot is the load-bearing checkpoint. If
target restore fails after a successful snapshot, the rebind TX
rolls back and the session is left at `HostLost` with the freshly
recorded snapshot — operator can /resume, or the auto-trigger paths
retry against a different peer.

### Phase B — auto-triggers

**Dead-source path** (`crates/engram-coordinator/src/dead_host.rs`):
extend the second-stage transition (lines 215–251). The current logic
routes `HostLost → {Idle, Dead}` based on snapshot presence. New
logic, in this order:

1. If `live_disk_manifest_*` is fresh (non-null, age ≤ some bound —
   default 5min) → fire the evac primitive against a peer host with
   `EvacLoss::Memory { reason: "source-dead-no-fresh-memory" }`.
2. Else if a recoverable snapshot exists → fire the evac primitive
   against a peer with the snapshot's memory manifest restored
   (`EvacLoss::None` if memory manifest age ≤ session age else
   `EvacLoss::Memory`).
3. Else → existing fall-through: `HostLost → Dead`.

The Idle path goes away — when the host is dead, we always try to
relocate. `HostLost → Idle` was only reasonable when the session
couldn't relocate because there was no mechanism; now there is, so a
session that *could* be migrated should be migrated, not stranded
in Idle waiting for a human /resume.

**FC NBD-loss path** (host-agent → coord heartbeat):

- New runtime probe in `crates/engram-host-agent/src/disk_daemon/`:
  a tokio task per attached NBD slot probes `nbd_kernel_busy` (the
  existing helper) on a 5s cadence. After 3 consecutive failed
  probes (≥15s of degraded NBD), the slot's bound sandbox is added
  to the next heartbeat payload's new `nbd_unhealthy: Vec<SandboxId>`
  field.
- Coord (`crates/engram-coordinator/src/`) consumes the field on each
  heartbeat. For each `SandboxId` in the list, fire the evac
  primitive against a peer host with `EvacLoss::Memory` (the source
  host is still alive but its disk I/O is degraded; a fresh memory
  snapshot via `host.snapshot()` won't work because the snapshot
  itself needs disk I/O, so we treat this case as source-disk-only-
  available).

### Phase C — target-host selection + admin + tests

`ScheduleContext::exclude_host: Option<HostId>` field added.
`pick_for_session` ranking extended per scope decision 2 above.

Admin endpoint `POST /api/admin/sessions/:id/evacuate` per the
canonical bearer-auth pattern at `api/admin.rs:98–150`. Request body:
`{ target_host: Option<HostId> }` (omitted ⇒ scheduler picks).
Response: `EvacReceipt`.

Tests:

- Unit / mock-MetadataStore coverage of the primitive shape.
- Postgres integration test `tests/admin_evac_live_pg.rs`: rebind
  TX, state transition coverage, cache invalidation, target
  selection algorithm against synthetic host fleet. Postgres-gated,
  wired into the existing FC CI lane per
  `[fc_tests_run_in_ci]` and `[new_tests_must_run_in_ci]`.
- E2E `tests/e2e_evac.rs` in the test-e2e-stack CI lane: two host-
  agents, dirty disk, admin-trigger evac, assert peer-Active with
  new `sandbox_id` and preserved disk. Second case kills source
  for auto-evac via `dead_host.rs` path. Modeled on Phase B's
  `e2e_flush_now` and Phase C's `e2e_chunk_gc`.

---

## Commit chain

Per `[adr_bookends_substantive_work]` and `[separate_commits]`:

- **Commit 0** — `docs(adr-0018): open M4 — Proposed, finalized
  design`. _(this commit)_
- **Commit 1** — `feat(core): HostClient::evacuate trait + EvacReceipt
  types`. Trait method + supporting types. Default impl returns
  `SandboxError::NotFound` so non-orchestrator impls compile.
- **Commit 2** — `feat(coord): Phase A evac primitive orchestration`.
  `HostRegistry::evacuate` impl: source snapshot → target restore →
  PG rebind TX → cache invalidation. Mock-driven unit tests.
- **Commit 3** — `feat(coord): Phase B dead-source auto trigger`.
  Extend `dead_host.rs` second-stage to consult manifest freshness
  + fire the primitive. Fall-through to Dead when no recoverable
  state. Mock-driven unit tests cover all four arms.
- **Commit 4** — `feat(host-agent): Phase B NBD-loss health signal`.
  Runtime probe + heartbeat field extension. Linux-gated; dev-vm
  clippy run before commit per
  `[linux_only_clippy_via_dev_vm]`.
- **Commit 5** — `feat(coord): Phase B NBD-loss trigger`. Coord
  consumes the heartbeat field and fires the primitive. Mock-
  heartbeat-producer tests.
- **Commit 6** — `feat(coord): Phase C target-host selection`.
  `ScheduleContext::exclude_host`; ranking with image-warm + zone
  preferences. Unit tests with synthetic host fleet.
- **Commit 7** — `feat(coord): Phase C admin endpoint`.
  `POST /api/admin/sessions/:id/evacuate` per the
  `reap_materialize_dir` pattern.
- **Commit 8** — `test(coord): evac unit + Postgres integration`.
  PG-gated rebind / transition / invalidation / selection tests.
  Wired into ci.yml FC test list per `[new_tests_must_run_in_ci]`.
- **Commit 8a** — `test(coord): e2e_evac in test-e2e-stack lane`.
  Two-host-agent stack; admin-trigger and auto-trigger paths.
- **Commit 9** — `docs(adr-0018+0015): close M4 — Accepted`. Closing
  bookend, divergences, commit hashes, dev-vm verification status,
  follow-ups. ADR 0015 §M4 gets a one-line forward pointer matching
  how ADR 0016 Phase C pointed at ADR 0015 §M5.

---

## Consequences

### What gets easier

- **Host loss stops killing sessions.** MIG rolls, OOM, network
  partitions past the heartbeat threshold — all relocate
  automatically, with memory preserved when a fresh snapshot was
  available, with disk preserved always (modulo cold-session non-
  goal). The "your session vanished" UX class — ADR 0015 §M4 named
  it explicitly — goes away.
- **Operator-initiated drains become first-class.** Deploy roll or
  cordon-and-drain becomes `for sandbox in host.sandboxes: POST
  /api/admin/sessions/:id/evacuate` against a healthy peer. The
  fan-out wrapper is small follow-up work; the primitive is here.
- **FC NBD-loss is no longer an operator escalation.** A wedged
  /dev/nbdN today requires manual host reboot (ADR 0017 §Issue 1).
  Phase B's heartbeat-fed trigger relocates the affected session
  before the host reboot — the wedge is still a host-level concern,
  but it's no longer a session-level concern.
- **Snapshot-as-load-bearing-checkpoint.** Evacuation's failure
  mode is "snapshot fails" (the only call that can fail at the
  point-of-no-return). Snapshot reliability is already the
  load-bearing primitive for idle eviction, so M4 doesn't introduce
  a new failure surface — it inherits the existing one.

### What gets harder

- **More sessions in `HostLost`-mid-relocation transient state.** A
  failed-restore path leaves the session at `HostLost` with a
  freshly recorded snapshot. Operator surface or auto-retry needs
  to handle the case where N consecutive peers fail (poison-target
  scheduling problem). For Phase A we accept manual /resume in the
  rare double-failure case; retry-with-different-peer is a future
  refinement.
- **`sandbox_id` rebinding is observable to clients.** A client
  caching a `sandbox_id` across an evac sees its prior id 404 on
  retry. The `session_id` is stable; clients should key on session.
  The web app already does — but any third-party or test that holds
  a raw sandbox_id will need updating. This is the explicit ADR
  0015 §M4 contract ("the `sandbox_id` token … can be substituted")
  and is the right boundary, but it's worth calling out.
- **Image-warm preference adds a knob.** The scheduler today picks
  the first ready host; with evac, it ranks. A host with a stale
  ready-images set ranks below a fresher peer, which is correct,
  but the ranking depends on heartbeat-reported state freshness.
  If a host's ready_images set drifts (e.g. the prefetch supervisor
  silently fails to update), evac targeting will be skewed. Phase C
  adds a metric `evac_target_image_warm_total / evac_target_total`
  to detect skew operationally.

### Explicit non-goals

- **Replacing the heartbeat-loss detector.** Phase B's auto-trigger
  uses `dead_host.rs`'s existing detection; M4 doesn't tighten the
  30s threshold or change the polling cadence. Faster detection is
  a separate concern.
- **Cross-region evacuation.** Same-zone preference is a ranking
  input; cross-zone is allowed when no same-zone peer can accept.
  Cross-region (multi-cluster) is not in scope — the chunk-store
  topology assumes a single bucket per cluster.

---

## Open questions

- **EvacReceipt sync vs async.** The admin endpoint should block until
  the target sandbox is `Active`. The auto-triggers (`dead_host.rs`,
  NBD-loss) are already background tasks; they can run sync inline.
  Strawman: keep the trait method sync, polling on the target.rs side
  if needed. Revisit if the admin endpoint's blocking time becomes
  operationally annoying (snapshot + restore ≈ 5–20s typical).
- **Heartbeat schema extension.** `nbd_unhealthy: Vec<SandboxId>` as
  a new field on the heartbeat protobuf vs. sidecar message vs.
  reuse an existing per-sandbox health slot. Decided at commit 4
  when the heartbeat code is open.
- **Source-cleanup ordering after target Active.** If the source-side
  destroy fires before the target's restore commits, a target-side
  failure has no rollback. Strawman: destroy after rebind TX commit;
  accept that a source-side stuck destroy leaves an orphan that the
  next host-agent restart cleans up (ADR 0017's surface).
- **Evac-retry-against-different-peer.** Today's plan accepts manual
  /resume when target restore fails. The auto-triggers could retry
  against the next-best-peer in the same tick. Defer to commit 3/5
  when the trigger code is open.

---

_(Closing bookend will append "As-built notes" on the Accepted commit.)_
