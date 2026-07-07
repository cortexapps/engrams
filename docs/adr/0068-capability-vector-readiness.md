# ADR 0068: typed capability-vector host readiness + probe-before-host_lost

**Status:** Accepted (2026-07-02) — implemented in one PR (issue #531) against `main` @
`42ed9bb2`. Part of the 2026-07 core-ops overhaul; must land *after* #526
(telemetry-restoration) at merge time for heartbeat/counter-shape compatibility, though it
was implemented independently.

**Related:** #526 telemetry-restoration (counters must survive rolls) · #542
epic-binding-epoch-delivery (fixes the desync that *produces* live-VMs-missing-from-list;
this ADR is the belt that stops the coordinator killing sessions over it) · #532
fc-load-path-rekey (its fork patch rides the `fc_snapshot_version` cutover this ADR makes
visible) · #546 epic-capture-jobs (decision 9 there: the cold-base content key folds in
`fc_snapshot_version`, the probed value this ADR provides) · #538 enable-fleet-prewarm
(graded readiness is the recorded end state; this ADR's vector is the substrate) · #530
generation-purge (the dead `required_image_digest` filter this issue flags stays dead until
that lands).

## Problem

"Host ready" was a boolean that lied. A host became schedulable the moment it registered
(`host_http.rs`'s register handler hardcoded `status: HostStatus::Ready`), and every real
precondition for serving a session had been discovered as a separate prod incident: gRPC
listener not yet bound, base-shm tmpfs not yet mounted by node-prep (every host-agent roll
dropped fresh-host UFFD-restore capacity for a window), bundles not staged, the NBD module
absent silently degrading to materialize-to-file, wire-version skew making hosts silently
*invisible* to the scheduler ("no capacity" with free hosts). Each got a point fix at a
different layer; none was a property the scheduler could see.

The same optimism ran the other direction: the coordinator declared sessions `host_lost`
from indirect signals (a stale `last_heartbeat_at` row, or a sandbox missing from the
host's self-reported `running_sandboxes`) without ever probing the thing it was about to
kill. Prod incident fbd3794c (2026-06-28): a provably-alive VM was declared `host_lost` and
"resumed" 8 times in 12 minutes — each cycle rewinding the transcript — while the host was
reachable the entire time.

## Decision

**(a) Capability vector.** `engram_core::types::host::HostCapabilities` — a typed,
self-verified vector (`schema`, `backend`, `grpc_self_connect`, `base_shm_tmpfs`,
`uffd_minor_shmem`, `nbd`, `bundle_stamp`, `fc_snapshot_version`, `wire_version`), each
capability a `CapStatus` (`Unknown | Ok(detail) | Failed(msg) | NotApplicable`). Probed by
the host-agent (`engram-host-agent::capabilities`) once at startup (before the first
register) and re-probed every heartbeat tick; persisted on `hosts.capabilities` (migration
0080, JSONB, default `'{}'::jsonb` ⇒ `schema: 0`). `schema == 0` is the soft-pass posture a
pre-0068 row or a mid-roll host gets — same shape `wire_version == 0` already had.

The placement gate (`engram-coordinator::placement::host_meets_capabilities`) requires,
once `schema >= 1`: `grpc_self_connect` + `bundle_stamp` both `Ok` for ANY FC placement;
`base_shm_tmpfs` + `uffd_minor_shmem` + `nbd` each `Ok` **or** `NotApplicable` when the
placement's `CapabilityRequirements::needs_uffd_substrate` is set (derived per call site —
the enabled image's `base_snapshot_memory_manifest` on create, the snapshot row's
`memory_manifest` on resume/evac); and an exact `fc_snapshot_version` match when both the
snapshot row and the candidate host report one. `Failed` and `Unknown` fail a *required*
capability; `NotApplicable` passes it — it's the honest report of an ADR 0022 File-backend
host (the substrate was never configured, so there's nothing to probe), which can still
legitimately serve a memory-manifest placement via the File-backend path (see "Post-review
fixes" below — this was a bug in the original implementation, fixed post-review).

`snapshots.fc_snapshot_version` (migration 0080) records the capturing host's `firecracker
--snapshot-version` at eviction-snapshot and checkpoint-advert-reconcile time — this is the
capture-time pairing key `epic-capture-jobs` decision 9 folds into its cold-base content
key.

The old 30s blocking "gRPC readiness gate" in `host-agent`'s startup path is **deleted, not
hardened** — the heartbeat's per-tick `grpc_self_connect` re-probe IS the retry now, and the
coordinator's capability gate is what actually withholds placement.

NoCapacity visibility: `placement::exclusion_summary` names the first failing reason per
host (`excluded | not_ready | cordoned | wire_skew | stale | cap:<name> | digest_not_ready |
no_fit`), logged + counted (`engram_placement_excluded_total{origin,reason}`) via the shared
`placement::log_empty_candidates` helper — kills the "no capacity with free hosts" mystery
mode. A core-ops-batch correction pass (see the deferred-items note below) extended this from
just the `pick_for_session` `NoCapacity` path to all four empty-candidate call sites (`create`
/ `queue_create` / `queue_resume_precheck` / `resume`), each distinguished by the `origin`
label. The fleet view (`HostView.failing_capabilities` / `fc_snapshot_version` /
`capabilities_schema`, proto fields 24–26) surfaces the same thing to operators.

**(b) Probe-before-host_lost.** New `HostService.ProbeSandbox` RPC
(`ProbeSandboxResponse { known_to_backend, process_alive }`) and a matching
`HostClient::probe_sandbox` trait method with **no default `Ok`-shaped impl** — a defaulted
`Ok` would make "can't probe" indistinguishable from "alive." `SandboxBackend` DOES get a
default (`known_to_backend` from `list()`, mirrored into `process_alive` — correct for
VZ/Process, which have no orphan-VM mode); `FirecrackerBackend` overrides it with an
INDEPENDENT ground-truth check: read the persisted per-sandbox manifest
(`sandbox_manifest::manifest_path`) and verify the same three-axis pid identity (pid +
start-time-jiffies + comm) the survivor-reattach pass already trusts — not the in-memory
map, since the map (or its heartbeat mirror `running_sandboxes`) being wrong is exactly the
desync this probe exists to catch.

`reconcile::flip_missing` probes before flipping: on `process_alive == true`, skip the flip,
reset the strike counter (`apply_missing_sandbox_strikes(&[session_id], &[], _)`), and bump
`engram_reconcile_probe_rescues_total`. A probe error (unreachable, or `Unsupported` — an
old host-agent mid-roll answering `Unimplemented`, mapped to a dedicated `SandboxError`
variant distinct from the `snapshot_begin`/`snapshot_wait` fallback's `InvalidSpec` mapping)
falls through to the flip unchanged. `reconcile_with_deps`'s return value was tightened to
reflect ACTUALLY-flipped sessions (not just strike-threshold-crossed ones), so a rescue
doesn't get misreported as a flip by callers.

Plus one ordering fix: the heartbeat handler now persists (`touch_host_heartbeat`) BEFORE
running reconcile — a heartbeat the coordinator 5xx's (persist failure, issue #231) can no
longer drive session flips off an un-persisted heartbeat.

The dead-host detector's own host-level `Ping` probe (added in `7fcc4c3c`) is untouched
beyond a paired `engram_dead_host_probe_rescues_total` counter so both rescue paths are
graphable together.

## What was deferred / deviations from the issue's literal plan

- The issue sketched three PRs (host probes / coordinator gate+surface / probe-before-
  host_lost); shipped as one PR here since one agent owns the whole worktree.
- `PR 2 step 12`'s NoCapacity exclusion summary originally landed at `pick_for_session`
  (resume/evac) only — NOT the path the wire-skew incident evidence actually named (that
  incident's "queued-stuck" symptom traced to the queue-scanner's create-origin break in
  `place_create`, which had no exclusion visibility at all). A core-ops-batch correction pass
  fixed the gap: `placement::log_empty_candidates` is now the one shared helper every
  empty-candidate call site invokes, so coverage spans all four paths — `create`
  (`api/sessions.rs`), `queue_create` / `queue_resume_precheck` (`queue_scanner.rs`), and
  `resume` (`pick_for_session`) — each distinguished by the counter's `origin` label.
- A dedicated unit test for the heartbeat-handler persist-before-reconcile ordering
  (`Testing & CI`'s explicit ask) was not added — it would require extending the widely
  shared `state::tests::MiniMeta` fixture with fault-injection + call-tracking, and the
  reorder itself is a straightforward, inline-documented code motion verified by the full
  existing reconcile + heartbeat test suite staying green.
- `probe_uffd_minor_shmem`'s exact kernel feature-negotiation shape (whether
  `UFFD_FEATURE_MINOR_SHMEM` needs explicit request via `UffdBuilder::require_features`
  before `register_with_mode(MINOR)` succeeds) could not be verified against the vendored
  Firecracker fork or a real Linux kernel from this macOS development environment ADR
  authorship happened in. The implementation matches the `userfaultfd` crate's public API
  and the kernel UAPI documentation as researched; needs first-real-fleet-heartbeat
  confirmation that `uffd_minor_shmem` actually reports `Ok` on a healthy FC host (not just
  "doesn't panic," which the CI-safe unit test covers).

## Acceptance criteria — status

- [x] A freshly rolled K8s host whose base-shm tmpfs isn't mounted receives zero UFFD-
      substrate placements until `base_shm_tmpfs = Ok` — enforced by
      `host_meets_capabilities`; unit-tested in `placement.rs`.
- [x] A wire-skewed host is visible in `engram_placement_excluded_total{reason="wire_skew"}`
      and the NoCapacity log — unit-tested (`exclusion_summary_names_the_first_matching_reason`).
- [x] A registered-but-not-heartbeated new-agent host isn't placeable until its first
      passing heartbeat; a `schema == 0` old-agent host keeps today's behavior.
- [x] The fbd3794c shape is unrepresentable — `probe_rescues_a_session_whose_process_is_alive…`
      integration test proves the rescue + eventual honest flip.
- [ ] Needs prod verification: a genuinely unreachable host still flips within
      `poll_interval + stale_threshold` (the probe must not delay real detection) — covered by
      unit test locally; prod timing needs a real rollout to confirm.
- [x] Restore placement never pairs a snapshot row's `fc_snapshot_version` with a
      mismatched host — unit-tested.
- [x] `snapshots.fc_snapshot_version` populated on new eviction snapshots + checkpoint-
      advert rows on FC hosts (wired in `idle_evictor.rs` + `host_http.rs`'s checkpoint
      reconcile).

## Post-review fixes (PR #564)

A deep review pass (`gh api repos/cortexapps/engrams/pulls/564/reviews`) found six
CONFIRMED issues, all fixed on the same branch before merge:

1. **`host_meets_capabilities` failed `NotApplicable` on the substrate caps** — an ADR 0022
   File-backend host honestly reports `NotApplicable` for `base_shm_tmpfs`/
   `uffd_minor_shmem`/`nbd` (nothing to probe, substrate never configured), but the gate
   treated that the same as `Failed`/`Unknown`, so a File-mode fleet would be 100%
   `NoCapacity` for every memory-manifest placement — foreclosing the ADR 0022 canary/flip.
   Fixed: `NotApplicable` now passes a required substrate capability alongside `Ok`; only
   `Failed`/`Unknown` withhold it. (Checked first whether PR #560's dead-code purge deletes
   `RestoreMode::File` and makes this moot — as of the fix, #560 had explicitly NOT landed
   that deletion, so the finding was live and needed a real code fix, not just a
   landing-order note.)
2. **Two `record_snapshot` call sites never stamped `fc_snapshot_version`** despite having
   the host in hand: `api/snapshot.rs::snapshot_core` (API-initiated session snapshots) and
   `api/enabled_images.rs::capture_and_record_base_snapshot` (base-template capture). Both
   now do the same best-effort `fc_snapshot_version_for_host` lookup the idle-evictor
   pipeline already used.
3. **Misleading comment on `api/sessions.rs`'s create-path gate** — it claimed
   `fc_snapshot_version` pairing "only matters for RESTORING a previously-captured snapshot
   ... not for booting from the base template," but a create IS an FC restore of the base
   snapshot (no warm pool). Rewritten to state the real reason creates stay unconstrained:
   `PreparedBoot` assembly doesn't thread the base row's recorded version through yet.
4. **`FC_SNAPSHOT_VERSION`'s `OnceLock` permanently cached a transient probe failure** — a
   one-off `firecracker --snapshot-version` spawn failure (fork EAGAIN, binary momentarily
   missing mid-bake) locked in `None` until process restart, silently version-unconstraining
   every snapshot/restore on that host thereafter. Fixed: only the success path is cached; a
   failed probe re-spawns (cheap) on the next heartbeat tick and self-heals.
5. **`exclusion_summary`'s doc comment didn't match its code** — omitted `not_ready` and
   stated the wrong order. Fixed to match the actual first-match order (and its own unit
   test).
6. **Web fleet view dropped `fc_snapshot_version`/`capabilities_schema`** — regenerated into
   the proto bindings (fields 25/26) but never threaded through `protoHostToLegacy`,
   `types.ts`, or `Fleet.tsx`, so the operator-facing "one-glance skew display" step 13 asked
   for was incomplete. Wired through; `Fleet.tsx` now shows a host's reported snapshot
   version inline and a "caps unreported" badge for `schema == 0` hosts.

Also fixed a CI-only bug (not a review-doc finding, but blocking merge): the
`cross-compile linux-musl artifacts` job built `engram-host-agent` without the
`CFLAGS_x86_64_unknown_linux_musl`/`BINDGEN_EXTRA_CLANG_ARGS` env the job already set for
`engram-uffd-handler`'s step — the new `userfaultfd` dependency (this ADR's UFFD MINOR probe)
needed the same kernel-UAPI-header wiring the first time it hit the host-agent musl build.
Hoisted both vars to the job level.

Migration renumbered `0077` → `0080` as part of a six-PR batch-wide collision resolution
(six sibling PRs in the same 2026-07 core-ops overhaul batch all claimed `0077` from the same
`main` base); this PR's assigned slot in the land-queue is `0080`.
