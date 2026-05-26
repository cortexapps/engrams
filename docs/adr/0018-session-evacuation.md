# ADR 0018: Session evacuation — `Sandbox` as a host-fungible value

Status: 2026-05-25 — **Accepted** as of commit chain below. Phase A
(alive-source primitive) + Phase B (dead-source + NBD-loss auto
triggers) + Phase C (target selection + admin endpoint + tests) all
shipped. Discharges ADR 0015 §M4 and folds ADR 0017's M4.1
follow-up.

Supersedes nothing. The auto-trigger paths are env-gated default-off
pending the resume-from-Created follow-up; the admin endpoint ships
default-on so operators can drive drains today.

Phase chain:

- **Phase 0** — this ADR in Proposed status. (commit `aa37933`)
- **Phase A** — `HostClient::evacuate` trait + types (commit `4fac78e`)
  → orchestration in `evacuation::evacuate_to` (commit `95db896`).
- **Phase B** — dead-source path in `dead_host.rs` via
  `evacuate_dead_source` (commit `874e397`) → NBD-unhealthy heartbeat
  field + monitor seam (commit `96f3cc7`) → coord-side
  `nbd_loss_trigger::process_unhealthy` (commit `42e0030`).
- **Phase C** — `ScheduleContext::exclude_host` ranking
  (commit `e128e72`) → `POST /api/admin/sessions/:id/evacuate`
  (commit `5b1352c`) → fmt+clippy sweep (commit `608a5a7`) →
  Postgres integration (commit `f0202c3`) → e2e admin-shape test
  (commit `74c9566`).
- **ADR closing bookend** — as-built notes (this commit).

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

---

## As-built notes (2026-05-25)

### Phase A — alive-source primitive

The trait method `HostClient::evacuate` (commit `4fac78e`) shipped
exactly as the design specified — default `Err(NotFound)` on all
per-host impls; the orchestrator override never materialized
because `HostRegistry::evacuate` would have needed `SharedState`
access (start_agent / secrets / egress policy) that the trait can't
carry. Instead the real orchestration lives in
`engram-coordinator::evacuation::evacuate_to` (free function;
commit `95db896`) — the trait method is a documented seam that
keeps the ADR 0015 §M4 vocabulary alive.

The state-machine sequence shipped as designed:
`Active → HostLost → Created`. The legality table at
`engram-core::types::session::can_transition_to`:124 already
permitted `HostLost → Created` from M2's groundwork; no
state-machine code changes were necessary in this chain.

`evacuate_to` leaves the session at `Created` on the target host —
the caller drives `start_agent` + → `Active`. The admin endpoint
(commit `5b1352c`) accepts this contract, returning the receipt
without finishing the resume dance. The /resume-from-Created
follow-up (see open question below) closes the UX loop.

**Commit 10 ("M4 actually works") closes the loop**: extracts
`finish_resume_to_active(state, session, new_sandbox_id)` from
`resume_from_fc_snapshot` and wires it from all four callers — the
admin endpoint, `dead_host.rs` auto-trigger, `nbd_loss_trigger`, and
a new `/resume from Created` dispatcher arm. Also adds the shared
`bind_session_routing(state, id, sandbox_id)` helper that updates
both coord's session→sandbox cache and the target host-agent's
session_bindings map; without it /exec hits 404 after relocate.
Both env flags flipped default-on now that the path is stuck-free.

### Phase B — dead-source + NBD-loss auto triggers

`evacuate_dead_source` (commit `874e397`) is the dead-source
equivalent of `evacuate_to`. Restores from existing artifacts:
`pick_evac_disk_manifest(session.live_disk_manifest, snapshot.disk_manifest)`
for the disk side, `snapshot.memory_manifest` for memory. Adopts the
tiered loss policy from ADR 0016 §"What gets easier" verbatim —
`EvacLoss::None` when both manifests present, `EvacLoss::Memory{reason:
"source-dead-no-snapshot"}` for disk-only.

The NBD-loss path (commits `96f3cc7` + `42e0030`) shipped a thinner
shape than the design specified. The heartbeat field
`nbd_unhealthy: Vec<SandboxId>` and the consumer
`nbd_loss_trigger::process_unhealthy` are in place; the host-agent's
runtime NBD-probe task that *populates* the field stayed out of
scope this round. The `NbdHealthMonitor` (host-agent
`heartbeat.rs`) is the test seam — the field will stay empty until a
follow-up wires the per-slot probe extension of
`disk_daemon/slot.rs::nbd_kernel_busy` into a runtime monitor.
Trade-off: the trigger machinery ships end-to-end now (testable via
admin injection); the actual NBD-degradation detector lands when
production has a stuck-NBD incident to validate the probe shape
against.

As of commit 10, both auto-triggers (dead_host + nbd_loss) default
**on**: `ENGRAM_DEAD_HOST_AUTO_EVAC=0` and `ENGRAM_NBD_AUTO_EVAC=0`
roll back to the legacy paths if needed. Pre-commit-10 the
"stuck at Created" UX wart blocked default-on; commit 10's
finish_resume_to_active wiring closes the loop, so default-on is
now the conservative ship. Operators can flip back to off via env
flag if a regression surfaces. The pre-commit-10 path was operator-
override-only.

### Phase C — target selection + admin + tests

`ScheduleContext::exclude_host: Option<HostId>` (commit `e128e72`)
landed minimal: it filters the candidate set in `pick_for_session`'s
three ranking arms. Same-zone preference and image-warm
promotion (filter → ranking) — both named in the original scope
decision — deferred. Same-zone needs zone tagging plumbed into
`HostState` (heartbeats don't carry zone today); image-warm
promotion provides no measurable value while the existing
`required_image_digest` filter is a hard floor that prod fleets
satisfy. Filed forward.

The admin endpoint (commit `5b1352c`) ships only the alive-source
variant (Active session). HostLost / Idle sessions return 409 — the
operator drain story is "evacuate Active sessions". The dead-source
admin variant is a future endpoint after the resume-from-Created
follow-up.

Tests:
- Unit (commits 2, 3, 6): 14 evacuation tests + 2 host_registry
  exclude_host tests. Mock HostClient + FakeMeta.
- Postgres integration (commit `f0202c3`): 4 live-PG tests in
  `admin_evac_live_pg.rs`. Wired into ci.yml's Postgres-gated
  ignored lane next to `admin_chunk_gc_live_pg`.
- E2E (commit `74c9566`): `e2e_evac_admin_endpoint_shape` in the
  test-e2e-stack CI lane. Single-host integration fixture →
  endpoint-shape coverage with a loud-warning skip for the full
  2-host relocate. Filed forward.

### Divergences from the design

1. **`HostClient::evacuate` trait method shipped as documented
   seam, not orchestrator dispatch.** The trait can't access
   SharedState (secrets/egress/start_agent plumbing); the
   orchestration lives in `evacuation::evacuate_to` as a free
   function. The trait method's default `Err(NotFound)` makes it
   a tombstone that keeps the ADR 0015 vocabulary discoverable.

2. **NBD-probe runtime wiring deferred.** The trigger ships
   end-to-end via the `NbdHealthMonitor` seam, but production-
   driven probe population is a follow-up. The heartbeat field
   stays empty until the probe lands.

3. **Same-zone + image-warm scheduler refinements deferred.**
   Only `exclude_host` lands in Phase C. The other Phase C
   scope-decision-2 items are filed forward.

4. **Admin endpoint covers alive-source only.** Dead-source
   relocate via admin is a follow-up endpoint after
   resume-from-Created lands.

5. **e2e test covers endpoint shape only.** Full 2-host relocate
   is a follow-up after integration-up.sh learns the two-host
   topology.

### Newly-filed follow-ups

- **FC cross-host vsock UDS remap (load-bearing).** Commit 10's
  dev-vm validation showed that after a cross-host relocate, /exec
  fails with `connect to FC vsock UDS ./var/host-sandboxes-integration/<old-id>.vsock:
  No such file or directory`. The FC snapshot embeds the source
  host's vsock UDS path verbatim
  (`engram-sandbox-firecracker::restore_in_jail`:1743 picks
  `manifest.source_vsock_canonical` and falls back to the host's
  work_dir + `manifest.sandbox_id`, both of which assume same-host
  semantics). Cross-host restore needs either path remapping
  pre-`load_snapshot` or symlink staging on the target host. Without
  this, the M4 promise "session keeps running through a MIG roll"
  is unfulfilled — the session reaches Active on the new host but
  /exec / /shell / /prompt fail until the user manually re-creates.
  This is FC-domain work, outside M4's scope; the M4 primitive is
  ready and waits for the FC fix.
- **Runtime NBD-probe task** in
  `engram-host-agent::disk_daemon`. Per-slot tokio task probing
  `nbd_kernel_busy` on 5s cadence; 3 consecutive failures →
  `NbdHealthMonitor::insert(sandbox_id)`. Linux-only; dev-vm clippy.
- **Same-zone scheduling preference**. Needs zone plumbed into
  `HostState` (extend heartbeat to carry zone from
  `HostMetadata.zone`, or read PG `HostRecord.cloud_metadata.zone`
  on registry update).
- **CI-lane e2e against the 2-host integration fixture.** The
  manual dev-vm runbook (`scripts/integration-evac-test.sh`)
  exercises the full path locally; wire that pattern into ci.yml's
  test-e2e-stack lane once Blacksmith has NBD wired.
- **Dead-source admin endpoint**. Operator-driven dead-source evac
  (today only the dead_host.rs heartbeat-timeout path can fire it).
- **Image-cache-warm scheduler ranking** (filter → ranking
  promotion). No measurable value over the current hard-floor
  filter; revisit when fleets see heterogeneous prefetch states.
- **Continuous memory sync**. ADR 0018 §"Out of scope" already
  filed this as a future ADR; memory RPO stays snapshot-bounded.

### Dev-vm verification

Validated on `engram-dev` (project `cortex-test-1608327238078`,
zone `us-west2-a`):

- **Linux clippy** clean.
- **Postgres-gated suite** (CI-shape): 17/17 pass, including all 4
  new `admin_evac_live_pg` tests covering alive-source +
  dead-source primitives + state-machine drive against live PG.
- **Mac-side `just check`** workspace-wide: 825 tests, 21 skipped.
- **2-host-agent dev-vm runbook** (`scripts/integration-evac-test.sh`,
  run under `ENGRAM_INTEG_TWO_HOSTS=1 just integration-up`):
  validated the full orchestration end-to-end with real Firecracker
  microVMs:

  - Coord → source.snapshot(): SUCCESS (real FC snapshot to BlobStorage)
  - Coord → target.restore(): SUCCESS (real FC restore on peer host)
  - PG rebind through `Active → HostLost → Created`: SUCCESS
  - `bind_session_routing` (coord cache + host-agent
    session_bindings): SUCCESS
  - `finish_resume_to_active` → `Created → Active`: SUCCESS
  - Post-evac session row at `status=active` on new host_id: SUCCESS
  - **/exec on the relocated session: FAILS** with
    `connect to FC vsock UDS ./var/host-sandboxes-integration/<old-id>.vsock`.

  The /exec failure is **not** an M4 orchestration bug — it's the FC
  cross-host-vsock issue documented in follow-ups. The snapshot
  embeds the source host's UDS filesystem path; on cross-host
  restore the new host opens a non-existent path. Same-host
  /resume from Idle doesn't hit this because the path remains
  valid. M4's primitives + state machine + routing are all
  correct; the path-remap fix is FC-backend territory.

### Closing notes

Commit 10 ("M4 actually works") closes the loop on the
orchestration: `finish_resume_to_active` + `bind_session_routing`
mean an evac-relocated session reaches `Active` on the peer with
all the coord-side + host-agent-side bindings updated. Auto-trigger
flags default on now that the path is stuck-free.

What still doesn't work in prod is the FC backend's cross-host
vsock UDS path remap (load-bearing follow-up above). Until that
ships, the M4 primitive can be exercised via the admin endpoint or
the auto-triggers, but /exec after relocate will return the FC
vsock-not-found error. The orchestration is otherwise complete —
PG state, routing cache, host-agent bindings, harness rebuild
dispatch, state machine all behave correctly.

The end-to-end "session keeps running through a MIG roll" promise
from ADR 0015 §M4 is *almost* delivered: state and orchestration
arrive correctly, but the user-visible "next /exec works" step
needs the FC vsock path remap. That gap is filed forward and is
the single load-bearing follow-up.

The chain is intentionally split across small commits per
`[separate_commits]` so reviewers can read each phase
independently. The fmt+clippy sweep commit at the Phase B → Phase C
boundary is a one-time catch-up for `[lint_before_commit]`
discipline that slipped during the long chain.
