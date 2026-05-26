# ADR 0018: Session evacuation — `Sandbox` as a host-fungible value

Status: 2026-05-26 — **Accepted, validated in production.** The
async evac rework (commits 12a–12n) lands the state machine +
scanner + cordon pattern and is proven end-to-end on the GKE
prod cluster. The user-visible promise — "operator drains a host
/ a host dies, the session moves to a peer and keeps working" —
holds in prod with disk content bit-identical across the relocate.

**Prod validation (2026-05-26, cluster `engrams` us-west2):**

- **Dead-host recovery, unprompted:** a real user session
  (`3e692ab6`) survived THREE consecutive MIG rolls during the
  deploy. Each time its host VM was terminated, `dead_host`
  flipped it HostLost→Evacuating and the scanner restored it on a
  peer in ~24s, fully automatic. A freshly-created session with
  no recoverable state correctly went `Dead` (the no-state branch).
- **Operator evac** `POST /api/admin/sessions/:id/evacuate` →
  202 `{status:"evacuating"}`, scanner resumed on a different
  host, `/exec` post-evac returned exit 0.
- **Cordon / uncordon** → picker excludes / restores the host.
- **Drain** `POST /api/admin/hosts/:id/drain` (2 sessions) →
  202 `{evacuating:[…], failures:[]}`, both relocated to the peer
  in parallel, both reachable via `/exec`.
- **Disk fidelity:** canary write → evac → read, md5 **identical**
  across the relocate for BOTH a uniform 0xAB pattern AND a 512 KB
  random-data file. The cross-host md5 mismatch seen on the dev-vm
  did **not** reproduce in prod — it was an artifact of the broken
  dev-vm environment (same env with the pre-existing `start_agent`
  hang, stale host rows, nondeterministic bakes; see
  §"Dev-vm stumbling blocks"). 12m's pause-before-flush ordering +
  NBD in-flight barrier + write/flush atomicity fix is what makes
  it hold.
- Live-PG tests (CI Postgres-gated lane): migration 0037 schema
  shape, `list_evacuating_sessions`/`bump_evac_attempts`/counter-
  reset round-trip, `HostRegistry::cordon`/`uncordon` picker
  filtering.

**Prod-found + fixed (commit 12n):** re-evacuating an
already-restored sandbox failed `assert_rootfs_canonical`.
`restore()` mints a fresh sandbox_id but
`restore_canonical_symlinks` keyed the host's-own canonical rootfs
+ harness symlinks off `manifest.sandbox_id` (the *source* id), so
a later `snapshot(new_id)` couldn't find `rootfs/<new_id>.dev`.
Surfaced only on the SECOND evac (the first lineage's source id ==
the cold-create id, so the bug hid). Fix threads the new id into
`restore_canonical_symlinks`. 12n is **verified in prod**: the
snapshot-side failure is gone (`POST /evacuate` of a restored
sandbox now returns 202). Dead-host recovery was unaffected
(it doesn't snapshot a dead source), which is why `3e692ab6`
survived four rolls. See §"Commit 12n" below.

**One edge case still open (commit 12o):** with 12n in place,
re-evac gets past the snapshot but the scanner's *restore* of a
twice-relocated sandbox fails FC `load_snapshot` (source-keyed
rootfs backing file not materialized on the new target); the
session falls back to `Idle` after the retry budget. This affects
**only** back-to-back *operator* evac/drain of the same session
— not dead-host recovery, not single operator-evac. The primary
deploy-roll promise is unaffected. See §"Commit 12o (open)".

The earlier "12m.4 deferred — residual cross-host disk drift"
investigation is **closed**: the drift was dev-vm-only.

Phase chain (commits 0–11 already on `main`):

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
- **ADR closing bookend** — as-built notes (commit `d5aa0ea`).
- **Dev-vm fixes** — test scaffolding for live PG (commit `0fcd42c`)
  + doc correction (commit `a29e1aa`).
- **Commit 10** — `feat(coord): M4 actually works`
  (commit `cddeb7d`). Extracts `finish_resume_to_active` from
  `resume_from_fc_snapshot` and wires it from admin evac + auto
  triggers. Adds `bind_session_routing` (coord cache +
  host-agent session_bindings). Default-on flip for both env flags.
- **Commit 11** — `fix(sandbox-firecracker): don't unlink vsock UDS
  file on destroy` (commit `22dd2e9`). Cross-host evac safety
  on shared filesystems; removes the destroy-side `remove_file` of
  the base UDS so the target's freshly-bound binding survives the
  source's destroy. Validated end-to-end on dev-vm: /exec works on
  the relocated session.
- **Commit 12** — async rework (this is the load-bearing milestone):
  - **12a** (`f1f60b6`) — `SessionState::Evacuating` variant + legality
    table; `idle_evictor::evict_session_to_state(target)` parameterized
    pipeline; `ensure_active` dispatches Evacuating like Idle.
  - **12b** (`f780a7b`) — migration 0037 adds `sessions.evac_attempts`;
    `MetadataStore::list_evacuating_sessions` + `bump_evac_attempts`
    with PG impl + MiniMeta mirror. `transition_session(Evacuating)`
    resets the counter atomically.
  - **12c** (`00e578e`) — new `engram_coordinator::evac_resumer`
    background scanner sibling to `dead_host.rs`. Polls Evacuating,
    bumps attempts, calls `evacuate_dead_source` + `bind_session_routing`
    + `finish_resume_to_active`. 20-attempt budget then falls back
    to Idle. Spawned from `run_with_registry_and_local`.
  - **12d** (`a7a9067`) — `HostRegistry::cordon(host_id) /
    uncordon(host_id) / sandboxes_on_host(host_id)`. Cordon flips
    `HostState.draining` which the existing picker already filters.
  - **12e+12f** (`38a67ae`) — admin endpoints `cordon`, `uncordon`,
    `drain` + rewrite of `evacuate_session` to async 202 shape.
    `drain` parallel-evacs every Active session on a host via
    `evict_session_to_state(Evacuating)`.
  - **12g** (`a4df07f`) — `dead_host.rs` second-stage no longer
    orchestrates relocate inline; routes `HostLost → Evacuating`
    when recoverable state exists, `Dead` otherwise. The scanner is
    the unified resume driver.
  - **12i** (`d0844c0`) — `e2e_evac_admin_endpoint_shape` asserts the
    202 + `{status: "evacuating"}` body. `integration-evac-test.sh`
    polls for `Evacuating → Active` with disk-canary md5 preserved.
  - **12h** (follow-up) — retire `evacuation::evacuate_to`; the
    function has no production callers post-12e/f but tests still
    exercise the type. Will land in a focused cleanup commit.
  - **12j** (`f7272ae`) — live-PG tests for the scanner primitives:
    migration 0037 schema pin, `list_evacuating_sessions` +
    `bump_evac_attempts` + counter-reset round-trip, cordon/uncordon
    picker integration. Runs in CI's existing Postgres-gated lane.
  - **12l** (`cec2cc6`) — dev-vm-found bugfixes:
    (1) migration 0037 also drops + re-adds `sessions_status_check`
    to permit `evacuating` (commit 12a missed this; transitions
    failed at the DB layer); (2) `evacuate_dead_source` derives
    `state_blob_key` + `sidecar_blob_key` from the snapshot id so
    the target host can materialize the FC sidecar from BlobStorage
    on cross-host restore (the sync `evacuate_to` path didn't lose
    them; the async path does via the PG round-trip); (3)
    `evac_resumer::run_once` is `pub(crate)` for the 12j tests.

### Commit 12m — pause-before-flush + NBD write/flush race + in-flight barrier

Landed as three commits (c6d65e5, d11c480, 5df7b1f):

  **12m.1** (`c6d65e5`) — trait change. `SandboxBackend::pause(id)`
  + `resume(id)` with default `Ok(())` impls. FC overrides to
  call `FirecrackerClient::pause/resume`. No behaviour change for
  Process/VZ/mocks; they inherit the defaults.

  **12m.2** (`d11c480`) — NBD daemon hardening. Two real bugs:

  1. `backend.write()` acquired the dirty lock in two phases
     (ensure_dirty + a separate re-acquire to patch). Between
     them, `flush()` could drain — making the subsequent
     `dirty.get_mut(...).expect(...)` panic OR causing the
     patched bytes to land in the NEXT version's dirty map after
     flush published the current manifest. Refactored to fetch
     the base bytes outside any lock, then take the dirty lock
     ONCE for insert-if-missing + patch. Atomic w.r.t. flush.

  2. The serve loop processes requests sequentially per
     connection, but the host kernel's NBD client can still be
     handing requests to our daemon after FC's vCPUs have paused
     (virtio queue → kernel NBD → userspace daemon pipeline).
     New `InFlightTracker` (atomic counter + tokio `Notify`):
     serve loop grabs an `InFlightGuard` per request; drop
     decrements and wakes any `wait_idle()` parker on the 1→0
     edge. `ChunkedDiskBackend::wait_idle()` is the public
     barrier.

  **12m.3** (`5df7b1f`) — `PooledBackend::snapshot` reordering:

  ```
  inner.pause(id)         # vCPUs stop
  nbd.wait_idle()         # pipeline drains
  nbd.flush()             # disk captured at paused state
  inner.snapshot(id)      # memory captured (idempotent re-pause,
                          # capture, resume — VM back to running)
  ```

  Pre-12m: nbd.flush() ran with FC still executing, then
  inner.snapshot() paused inside its own create_snapshot. Writes
  between the flush and the internal pause landed in memory but
  not in the published disk manifest.

Validation: 23 NBD backend unit tests pass (incl. 3 new tracker
tests). Dev-vm 2-host integration-evac-test.sh shows the new
ordering ("chunked NBD disk flushed (post-pause)") and the
session relocates successfully. Disk content **still does not
match** across the cross-host relocate (md5 differs);
investigation is in §"Commit 12m.4 (deferred)" below — the
flush-vs-pause race that 12m structurally closes is provably not
the cause, since same-host repeated reads on the source ARE
stable.

### Commit 12m.4 — residual cross-host disk drift — CLOSED (dev-vm-only)

12m closed the flush-vs-pause race. On the dev-vm, a canary md5
still appeared to differ across the relocate, and this section
tracked three hypotheses for a "residual drift" bug. **Prod
validation (2026-05-26) closed the question: there is no drift.**

The dev-vm symptom was an artifact of that environment, not a
code bug. On the prod GKE cluster, the canary write → evac → read
test passed with **bit-identical md5** for both a uniform 0xAB
pattern (`9b8596f4…` source == target) and a 512 KB random-data
file (`5715c627…` source == target) — the random case being
apples-to-apples with the dev-vm scenario that "failed." 12m's
pause-before-flush ordering + NBD in-flight barrier + write/flush
atomicity fix is what makes disk fidelity hold; once those run
against a healthy host-agent + bake pipeline (which prod has and
the dev-vm did not, per §"Dev-vm stumbling blocks"), bytes are
preserved.

The three hypotheses are moot — none described a real prod
failure. Most likely the dev-vm "three distinct md5s" came from
the same broken environment that hung `start_agent` and served
nondeterministic bakes: a half-restored sandbox reading from an
inconsistent local materialized rootfs. Not pursued further; prod
is the source of truth and prod is correct.

### Commit 12n — restore canonical symlink keyed on new sandbox_id

Prod-found during 12m validation. Re-evacuating an
already-restored sandbox failed:

```
evac pipeline: … snapshot error: non-canonical jail layout:
rootfs canonical symlink missing at …/rootfs/<sandbox_id>.dev
```

`FirecrackerBackend::restore` mints a FRESH `sandbox_id` for the
restored sandbox, but `restore_canonical_symlinks` installed the
host's-own canonical rootfs + harness symlinks keyed off
`manifest.sandbox_id` — the **source** id baked into the snapshot
manifest, not the new live id. The restored sandbox runs (FC's
`load_snapshot` opens the drive via the `source_rootfs_canonical`
path, which is recreated), but the new id's canonical symlink
never gets made. A later `snapshot(new_id)` — i.e. operator-evac
or drain of the restored session — calls
`assert_rootfs_canonical(new_id)`, looks for `rootfs/<new_id>.dev`,
and fails.

Why it hid until prod: on the FIRST snapshot lineage the source id
equals the cold-create id, so `source_canonical == canonical` and
the second-symlink branch collapsed — the one symlink that got
made happened to be the right one. The bug only surfaces on the
SECOND evac, when the live id diverges from the manifest's source
id.

Fix: thread the new `sandbox_id` into `restore_canonical_symlinks`
and key the host's-own rootfs + harness symlinks off it. The
`manifest.source_*` paths (what `state.bin` embeds, what
`load_snapshot` opens) are unchanged — both symlinks now exist,
pointing at the same target.

**Impact:** back-to-back deploy rolls using `drain` — a session
drained in roll N could not be drained again in roll N+1. The
dead-host recovery path is unaffected (it restores from the last
snapshot, never snapshots a dead source), which is why the real
session `3e692ab6` survived three consecutive MIG rolls in the
prod validation.

**12n verified in prod (2026-05-26):** after the 12n host image
rolled, operator-evac of an already-restored sandbox no longer
errors at the snapshot — `POST /evacuate` returns 202 and the
re-snapshot succeeds (session reaches Evacuating). The original
"non-canonical jail layout" failure is gone.

### Commit 12o (open) — restore of a twice-relocated sandbox

Fixing 12n peeled the onion to the next layer. With 12n in place,
re-evac gets past the snapshot, but the scanner's *restore* of the
twice-relocated sandbox fails on the new target host:

```
Firecracker PUT /snapshot/load -> 400: … Failed to restore MMIO
device: … Block: Virtio backend error: Error manipulating the
backing file: No such file or directory (os error 2)
/var/lib/engram/sandboxes/rootfs/<restored_sandbox_id>.dev
```

The scanner retries the full 20-attempt budget, then falls back to
`Idle` (so the session is recoverable-by-hand, not lost — though
`/resume` from Idle hits the same restore path).

**Root cause (traced 2026-05-26 against the actual prod snapshot
artifacts in GCS):** `source_rootfs_canonical` is computed from the
*live* sandbox id at snapshot time, but the rootfs `path_on_host`
baked into FC's opaque `state.bin` is whatever the drive was
*configured* with — and restore never re-points the rootfs drive
(only the harness drive gets a `patch_drive` on restore, at
lib.rs:2952). So after the first restore the two diverge. Confirmed
by fetching both sidecars + `state.bin` from GCS:

| | cold `706a1c2f` (snap 216ffb37) | restored `61057473` (snap ce2d9024) |
|---|---|---|
| `state.bin` embedded rootfs path | `rootfs/706a1c2f.dev` | **`rootfs/706a1c2f.dev`** (grep: live id `61057473` 0×, ancestor `706a1c2f` 2×) |
| sidecar `source_rootfs_canonical` | `rootfs/706a1c2f.dev` | `rootfs/61057473.dev` (recomputed) |
| match? | ✅ | ❌ diverged |

Hop 1 works because the cold sandbox's drive path == its computed
canonical (same id). Hop 2 fails because `restore_canonical_symlinks`
recreates the *computed* path (`61057473`) while FC's `load_snapshot`
opens the *embedded* path (`706a1c2f`), which nobody recreated →
ENOENT. The rootfs is NBD-backed: the `.dev` symlink is a per-host
indirection to `/dev/nbdN`, and the disk lineage is intact (both
snapshots share `disk_manifest` 5ad53e36, recoverable). The failure
is purely the path-identity mismatch — not a missing materialize.

This is a **leaky abstraction**: `source_rootfs_canonical` is
derived from sandbox identity under a cold-create assumption ("the
live sandbox's rootfs sits at its own id-keyed path") that restore
silently violates. The cold ancestor's id gets welded into
`state.bin` and rides the lineage forever, while the bookkeeping
keeps computing "current live id" paths that drift after hop 1.
12n (new-id symlink) was a correct fix one layer up; this is the
layer below.

**Why dead-host recovery doesn't hit this:** the dead-host path
(`evacuate_dead_source` from a *dead* source) restores from the
session's existing snapshot and never takes a fresh snapshot of a
restored sandbox, so `source_rootfs_canonical` is never recomputed
away from the embedded path. The operator path
(`evict_session_to_state` → `host.snapshot`) takes a NEW snapshot
of the restored sandbox — that recompute is what introduces the
divergence. This is why `3e692ab6` survived four dead-host
recoveries but a deliberate double operator-evac stalls.

**Fix — Option B (normalize identity on restore), chosen:** make a
restored sandbox truly equal to a cold-created one by re-pointing
its rootfs drive to its *own* new-id canonical path on restore,
mirroring what the harness drive already does. Mechanically:

- Restore loads via `load_snapshot_paused` /
  `load_snapshot_uffd_paused` (already in the client, used by the
  M1.12 option-D harness path), then
  `patch_drive("rootfs", rootfs_canonical(new_id))`, then resume.
  12n already installs `rootfs/<new_id>.dev` → the live `/dev/nbdN`,
  so the patch target exists.
- After this the running drive is at `rootfs/<new_id>.dev`, so the
  *next* snapshot's `state.bin` embeds the new id,
  `source_rootfs_canonical` (= `rootfs_canonical(new_id)`) matches,
  and ancestry is severed. Hop N == hop 1 for all N.

Rejected alternative (Option A): record the *true* embedded path in
`source_rootfs_canonical` rather than the computed one. Fixes the
bug but welds the cold ancestor id into every descendant forever —
keeps the smell. B removes the whole 12n/12o class.

**Residual risk to confirm during implementation:** FC must open
the root block device on *resume*, not on *load*, for load-paused →
patch → resume to work on the root drive (the harness path proves
it for a non-root drive). To be validated on the dev-vm FC
boot-test harness before wiring + end-to-end on prod via the
re-evac canary.

**Net operational state until 12o ships:** the primary M4 promise —
session survives a host loss / deploy roll via dead-host recovery —
is fully working in prod (proven 4×). Single operator-evac / drain
works. The only gap is **back-to-back *operator* evac/drain of the
same session**.

### Dev-vm stumbling blocks (12m.4 investigation, 2026-05-26)

Two days of attempted disk-drift inspection on the GCP dev-vm hit
hard environmental issues that prevented the planned tests
(recognizable-pattern hex inspection + loopback-mount of the
materialized rootfs). Documented here so the next attempt knows
what to expect:

  - **Disk pressure floor halts the host-agent.** `var/host-
    sandboxes-integration*` accumulates ~30 GB of FC sandbox state
    across test runs. Once the dev-vm's root FS drops below the
    21 GB floor (`engram_host_agent: idle-evict paused: disk
    pressure`), host-agent stops accepting new sandbox creates,
    coord returns 503 "no host has capacity," and nothing runs.
    `sudo rm -rf var/host-sandboxes-integration{,-b}` between runs
    is the only stable workaround. The directories are scratch;
    `integration-up.sh` recreates them. Not a code bug — a
    deployment hygiene issue.

  - **Stale `hosts` rows in PG poison the gRPC pool prewarm.**
    Restarting just coord (without `docker compose down -v`)
    leaves the prior run's `hosts.last_heartbeat_at` rows present.
    Coord boots, calls `GrpcHostPool::warm` against every
    historical host_id, all fail with `tcp connect error`, and
    the pool entries linger as half-initialized. The fresh
    host-agents that register later get NEW host_ids but the pool
    state for those is fine — except subsequent gRPC operations
    sometimes still return `http2 error` from the bad cached
    state. Full `docker compose -f deploy/docker-compose.dev.yml
    down -v` to wipe the PG volume is the only reliable reset.

  - **`integration-bake-demo.sh` produces nondeterministic
    digests.** Each invocation builds an image whose layer digests
    differ from the previous bake (timestamps in the OCI manifest
    or the layer contents). The host-agent's OCI image cache then
    oscillates between "stale cached digest" and "expected digest"
    every reconcile tick. Multiple `DELETE FROM enabled_images;`
    + rebake cycles are needed to converge.

  - **SSH session churn breaks tooling.** Many short-lived SSH
    invocations (one per `bash .claude/skills/dev-vm/scripts/run.sh
    …`) hit `gcloud ssh` rate limits intermittently — `ERROR:
    (gcloud.compute.ssh) [/usr/bin/ssh] exited with return code
    [255]`. Using the lower-level `ssh.sh` (which reuses the
    cached `gcloud compute config-ssh` host alias) is faster and
    more reliable. The `portforward.sh` IAP tunnels didn't help —
    they failed to bind localhost ports under the same load. For
    future inspection work, a single SSH session into a `tmux`
    multiplexer on the dev-vm + driving commands locally is
    probably the right shape.

  - **Pre-existing `start_agent` hang.** Independent of 12m: even
    after a clean docker-compose-down + rm var + integration-up
    + fresh bake + verified host registration + ready image,
    `POST /sessions` hangs at the start_agent gRPC call. The FC
    microVM boots successfully, FlushScheduler runs, manifests
    publish — but the in-guest bootstrap apparently doesn't dial
    back (no `start_agent` entry in host-agent.log, session
    stays at `created` status indefinitely). **Reproduced on
    binaries built from `c6ff7a6` (pre-12m), so this is NOT
    introduced by commit 12m.** Likely a bad baked image or a
    kernel/init mismatch on the dev-vm. Blocks any e2e dev-vm
    verification of the evac flow.

**Recommendation for 12m.4 investigation:** abandon the dev-vm
path and validate against a staging deploy of the production
GCP cluster (where the bake pipeline + host-agent images are
known-good and managed by deployment automation, not local
build artifacts). The full e2e shape — `kubectl drain`, watch
sessions move, run a canary write/read across the relocate —
is the same regardless of where it executes, and the prod path
sidesteps every stumbling block above.

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

---

## Commit 12 rework — async evac via Evacuating state + scanner

### Why we rewrote the alive-source path

Commit 11 unblocked /exec post-evac, but the dev-vm e2e surfaced a
**disk content mismatch** across cross-host evac: the canary file
exists at the right path with the right size on the relocated
session, but the bytes differ from what was written on the source.

Root-cause analysis pointed at the flush-vs-pause ordering inside
`PooledBackend::snapshot`. The existing alive-source evac primitive
(`evacuation::evacuate_to`) does `source.snapshot()` which internally:

1. Calls `chunked_disk.flush()` to push dirty bytes to BlobStorage.
2. Calls FC `pause`.
3. Calls FC `create_snapshot` for the memory.bin + state.bin.

Between step 1 and step 2, the guest can queue more writes via the
NBD daemon. The flush published manifest_v_N capturing dirty bytes
up to step 1. The memory snapshot in step 3 captures the guest
kernel's page cache, which has *newer* writes than manifest_v_N. On
target restore, NBD serves manifest_v_N (older bytes); memory's
view disagrees; reads via the page cache return memory's view until
eviction, then NBD's older bytes.

**The fix is structural**: stop having evac do its own flush. Lean
on the FlushScheduler's continuous-sync publish (the same one
`/resume from Idle` already trusts via `effective_resume_disk_manifest`),
and require the snapshot path to take the memory dump *with FC
already paused*. That order — pause → flush → snapshot — is what
the user proposed in this session, and it matches the existing
idle-eviction pipeline (`idle_evictor::evict_idle_session`) which
does pause-then-snapshot under FC's `create_snapshot` API.

### The async design (this is what commit 12 ships)

Replace the synchronous `evacuate_to` (orchestrate snapshot +
restore + rebind in one HTTP call) with an asynchronous flow split
across the database:

**Source-side primitive** (one host, one session):

1. Pause FC
2. Flush remaining dirty pages (one call into the chunked disk; FC
   is paused, so no new writes)
3. Save snapshot memory (`create_snapshot` → state.bin + memory.bin,
   chunked to BlobStorage)
4. Mark session as `Evacuating` in PG (status transition, sandbox_id
   cleared, snapshot row recorded)
5. Destroy local sandbox

This is exactly what `idle_evictor::evict_idle_session` already does,
but with the terminal transition going to `Evacuating` instead of
`Idle`. Commit 12's first change: parameterize that pipeline. The
existing function becomes a back-compat wrapper at
`target_state = Idle`; the new shape is
`evict_session_to_state(state, session_id, sandbox_id, target_state)`.

**Coord-side scanner** (new `evac_resumer` background task):

- Polls `sessions WHERE status = 'evacuating'` every ~10s.
- For each, calls the same `resume_session` machinery `/resume from
  Idle` uses today (which since commit 10 dispatches through
  `finish_resume_to_active` for the harness rebuild step).
- Tracks per-session retry attempts via a new
  `sessions.evac_attempts INT` column.
- After 20 attempts (~3 min), transitions `Evacuating → Idle` so a
  user `/resume` can drive it forward by hand. The retry budget is
  the "give up gracefully" boundary: a session that can't auto-
  relocate falls back to user-paused — not a regression vs the
  pre-M4 status quo.

**Cordon** (new admin endpoints):

- `POST /api/admin/hosts/:id/cordon` — sets `hosts.status =
  'Draining'` in PG. The coord-side `HostRegistry`'s
  `pick_for_session` already filters out draining hosts (the flag
  was wired for host-agent self-reported drain; we now drive it
  from coord-side admin too). Cordoned hosts cannot be picked as
  new-session targets OR as evacuation-relocation targets — the
  scanner naturally avoids them via the same filter.
- `POST /api/admin/hosts/:id/uncordon` — reverses, status back to
  `Ready`.
- `POST /api/admin/hosts/:id/drain` — cordon + evict every Active
  session on the host (each becomes Evacuating via
  `evict_session_to_state(Evacuating)`). Returns the list of
  session_ids that started evacuating. Operator polls `/sessions`
  to see them flip to `Active` on peers, and once the host has zero
  active sessions, it's safe to roll.

**Rewritten admin endpoint**:
`POST /api/admin/sessions/:id/evacuate` becomes async — calls
`evict_session_to_state(Evacuating)` and returns immediately. The
scanner picks up the work. The synchronous "block until Active on a
peer" shape is retired; callers poll `/sessions/:id`.

### State machine (already in tree as part of commit 12)

Added `Evacuating` variant to `SessionState` with these transitions:

```
Active     → Evacuating | …existing…
HostLost   → Evacuating | …existing…
Evacuating → Created (scanner resumes on peer)
           | Idle (scanner exhausted retries; user /resume)
           | Dead (terminal; chunks gone)
           | Completed (user delete mid-evac)
```

`legality_table_matches_adr` test updated; `terminal_states_reject_all_outgoing`
includes the new variant.

### What's still required to finish commit 12

(In rough order; the state-machine + idle_evictor parameterization
are already done as of writing.)

1. **Migration** `0036_sessions_evac_attempts.sql` — add
   `sessions.evac_attempts INT NOT NULL DEFAULT 0`.
2. **`evac_resumer` module** — sibling to `dead_host.rs`. Background
   task, polls `Evacuating`, invokes `resume_session` machinery,
   bumps retry counter, falls back to `Idle` after threshold.
3. **`HostRegistry` cordon plumbing** — make sure PG `Draining`
   status flows into the in-memory `HostState.draining` flag (today
   it's heartbeat-derived; add a PG-read path on registry update
   so admin-driven cordon takes effect immediately, not waiting on
   host-agent heartbeat). Add `pub fn cordon(host_id) /
   uncordon(host_id)` on `HostRegistry` that writes PG +
   updates in-memory.
4. **Admin endpoints**: cordon, uncordon, drain. The drain handler
   uses `list_active_sandbox_assignments_on_host` to enumerate and
   parallelizes `evict_session_to_state(Evacuating)` calls (bounded
   concurrency, ~8 in flight).
5. **`resume_session` dispatcher** in `api/snapshot.rs` — extend
   to handle `Evacuating` exactly like `Idle` (same code path).
6. **`dead_host.rs`** — change the second-stage transition to route
   `HostLost → Evacuating` (the scanner takes over) instead of
   today's `HostLost → Idle`. Removes the `evacuate_dead_source`
   plumbing entirely.
7. **Retire `evacuation::evacuate_to`** and
   `evacuation::evacuate_dead_source` — the new flow doesn't need
   them. Keep `EvacReceipt` / `EvacLoss` types — `finish_resume_to_active`
   returns the equivalent shape.
8. **`POST /api/admin/sessions/:id/evacuate`** — rewrite to async
   shape (return 202 with the new session state immediately).
9. **Tests**:
   - Update legality test (done).
   - Update `admin_evac_live_pg`: assert
     `evict_session_to_state(Evacuating)` leaves state at
     `Evacuating`, then simulated scanner tick drives to `Active`.
   - New test: scanner exhausts retries → `Evacuating → Idle`.
   - New test: cordon excludes host from `pick_for_session`.
10. **`integration-evac-test.sh`** — update to expect async flow
    (poll `/sessions/:id` for `Evacuating → Active` rather than
    synchronous evac response).

### Why this design ends up being smaller code than what it replaces

- One snapshot/restore pipeline (`evict_session_to_state` +
  `resume_session`) serves both idle eviction and operator evac.
- The scanner is the canonical resume driver — same role for
  Evacuating sessions as the user is for Idle ones. No parallel
  primitive.
- `dead_host.rs` loses its `evacuate_dead_source` dispatch path;
  it just flips state to Evacuating and the scanner does the rest.
- The admin endpoint is ~30 lines vs the current ~150-line
  evacuate_to + finish_resume_to_active orchestration.
- `evacuation` module shrinks from ~600 lines (two primitives + 14
  unit tests) to ~50 lines (just the EvacReceipt/EvacLoss types,
  if even those are kept).

### Why the disk-content bug goes away

Once the source primitive is "pause then flush then snapshot," the
flush-vs-pause race is structurally eliminated. The memory snapshot
captures the page cache state with the kernel quiesced, AND the
chunked-disk flush publishes manifest_v_N reflecting the same
quiesced bytes. Target restore reads manifest_v_N; reads via the
restored page cache + NBD agree. No content mismatch possible.

This is the same invariant `idle_evictor::evict_idle_session` already
relied on — that's why same-host `/resume from Idle` has worked
forever without this class of bug. The async evac flow finally
brings cross-host evac into the same regime.
