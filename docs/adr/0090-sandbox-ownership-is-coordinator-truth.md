# ADR 0090: Sandbox ownership is coordinator truth — rolls and recoveries must not kill owned VMs

- Status: Accepted (2026-07-13)
- Date: 2026-07-13
- Commit chain: PR #653 (`4ee65faa`) — the whole decision landed in one
  change: the reconciler's coordinator-ask (+ `session_owning_sandbox` /
  `/hosts/:id/sandboxes/:sid/owner`), quarantined-survivor heartbeat
  advert driving `evict_local`, the `start_agent` retry budget →
  terminal `Failed` + `harness_start_failed`, bundle-coverage placement
  preference, and the loud heartbeat-failure mode.
- Issues: 2026-07-11 reliability campaign (`scratch/devbrain-campaign-2026-07-11/REPORT.md` §2-4); memory note `teardown-reconcile-kills-reattached-vms` (2026-07-10, live-repro'd 2026-07-12 on session dd8d8f1f)

## Context

Two campaign incidents share one root cause: **host-agent-local state standing in
for ownership truth that only the coordinator has.**

1. **The teardown reconciler kills roll-surviving VMs.** ADR 0050 E's reconciler
   destroys any sandbox whose local `session_bindings` entry is missing
   (`session=None` arm) after `ORPHAN_STRIKES=2` (~30-60s), **without ever asking
   the coordinator**. `session_bindings` is an in-memory `DashMap` that a fresh
   host-agent generation only repopulates at the tail of a *successful* NBD
   rehydrate (`pooled_backend.rs` rehydrate pass). On 2026-07-12 a fleet roll
   pidfd-reattached C1's VM successfully, the NBD `RECONFIGURE` failed (EINVAL),
   the rehydrate bailed *before* the binding insert — and 34s later the
   reconciler SIGKILLed a healthy VM running a 13-minute build. The quarantine
   WARN even names the correct remediation ("recover via evict_local → resume");
   nothing takes it.
2. **Post-resume `start_agent` failure loops forever at `Created`.**
   `finish_resume_to_active` makes one `start_agent` attempt and maps failure to
   `Ok(CreatedHarnessFailed)`; the op executor treats any `Ok` as
   `OpOutcome::Done`, which wakes the sibling Deliver op, which re-enqueues a
   *fresh* Resume op — an unbounded retry loop with zero backoff (5/sec measured,
   923 host-side spawn attempts) that never surfaces to the user. The underlying
   spawn failure was itself an ownership-adjacent gap: the session relocated onto
   a 25-min-old autoscaled node whose bundle staging didn't yet cover the
   session's pinned harness (`/opt/engram/dyn/0/harness: NotFound`), because
   resume placement doesn't consult `hosts.current_bundles`.

ADR 0088 established the principle for enable work: *a roll must not destroy
in-flight work the coordinator still owns*. This ADR extends it to sessions.

## Decision

1. **The `None` arm asks the coordinator.** `sandbox_ownership` gains a
   by-sandbox form: the host asks "does any session own sandbox S on host H?"
   and the coordinator answers from `sessions.sandbox_id = $1 AND host_id = $2`.
   The reconciler's `None` arm calls it before counting an orphan strike;
   coordinator-unreachable ⇒ assume owned (the `Some` arm's existing posture).
   A local binding miss is no longer, by itself, grounds for destruction.
2. **The ownership answer repairs the local table.** An owned answer from (1)
   repopulates `session_bindings` (`record_session_binding`), restoring the
   fast path for the publisher and later reconciler ticks. (An earlier draft
   added `session_id` to `SandboxManifest` as a reattach-time fast path; the
   RPC answer makes that schema change redundant — the unknown-binding arm is
   rare, post-roll-with-failed-rehydrate only.)
3. **Quarantine auto-drives its own remediation.** When the rehydrate pass
   quarantines a survivor's NBD slot, the host-agent reports the sandbox in a
   new `quarantined_sandboxes` heartbeat field. The coordinator reacts by
   enqueueing the already-documented `evict_local → resume` (capture what the
   VM has, release the slot, relocate) instead of leaving the VM to the orphan
   path.
4. **`start_agent` failures are retryable-then-terminal.**
   `CreatedHarnessFailed` propagates as `OpOutcome::Retry`, engaging the
   existing `RESUME_MAX_ATTEMPTS` backoff machinery; budget exhaustion
   transitions the session to `Failed` with a user-visible
   `harness_start_failed` event. No more silent forever-`Created`.
5. **Resume placement gates on bundle coverage.** `CapabilityRequirements`
   gains the session's pinned aux-bundle refs; `host_passes_filters` rejects a
   host whose heartbeat-reported `current_bundles` stamp doesn't cover them
   (same posture as the base-shm/digest readiness gates). Exclusion surfaces
   through the ADR 0068 vocabulary as `cap:bundle_coverage`.
6. **Heartbeat failure is loud.** After ~30s of consecutive heartbeat delivery
   failures the host-agent logs ERROR per tick and bumps
   `engram_host_heartbeat_delivery_failures_total` (the fleet alert rule's
   input) — the 2026-07-12 outbound-network wedge cost a healthy host 1.5h of
   capacity with only DEBUG-level traces. No host-side self-heal: the wedge
   class is node-network-down, where a re-register fails identically and a
   returning network heals through the normal heartbeat path anyway.

## Consequences

- A fleet roll can no longer destroy a VM the coordinator still considers
  owned, even when NBD rehydrate fails — the failure mode becomes a
  coordinator-driven evict/relocate with durable state.
- One more RPC on the reconciler's orphan-suspect path (rare; strikes already
  debounce it). The manifest fast path keeps rolls RPC-free in the common case.
- The heartbeat grows two fields (`quarantined_sandboxes`, plus the existing
  wire-version bump discipline applies).
- `parse_session_state` and friends are untouched — no new session states here
  (the `Unreachable` state is ADR 0091's concern, WS3).

## Outcome (2026-07-13)

Deployed. The prod fleet rolled onto it without incident. Divergences
from the Decision above, both recorded during implementation:

1. **`SandboxManifest.session_id` was NOT added** (Decision §2 proposed it
   as a reattach-time fast path). The coordinator's ownership answer
   repairs the local binding table directly (`record_session_binding`),
   which makes the schema change redundant — the unknown-binding arm is
   rare (post-roll, failed-rehydrate only), so an RPC there is cheap.

2. The `start_agent` failure was not "no backoff, then give up" as the
   campaign reported: `CreatedHarnessFailed` mapped to `OpOutcome::Done`,
   so the Deliver verb enqueued a *fresh* Resume op each round — an
   unbounded, backoff-free loop where the existing `RESUME_MAX_ATTEMPTS`
   machinery was simply unreachable. Propagating it as `Retry` re-engaged
   the budget that was already there.

The teardown-reconcile kill (the memory note this ADR closes) has not
recurred; the next fleet roll over an active session is its acceptance
test.

## Addendum (2026-07-20): the budget-exhausted destroy is a durability rollback — make it loud

The Decision's quarantined-survivor recovery has a terminal arm: when the
`evict_local` (capture + relocate) retry budget exhausts, the coordinator
DESTROYS the crippled VM so the dead-host straggler sweep can drive
`HostLost → Idle` and the user's next resume recovers. That resume rewinds to
the **last published disk manifest** (`sessions.live_disk_manifest`, or the
snapshot's disk lineage) — silently discarding any guest writes the host
acked but never uploaded past that manifest version.

On the evening of 2026-07-20 this fired for real on three sessions after an
NBD rehydrate failure: `evict_local` kept refusing (un-pause onto a dead data
plane), the budget exhausted, and the coordinator destroyed the VMs — dropping
134 MiB, 100 MiB, and 50 MiB tails. The loss was **only** inferable by
hand-diffing host-agent spool logs against manifest versions: nothing durable
or user-visible recorded it, and no metric counted it.

Decision: the exhaustion arm now emits a durable, coordinator-authoritative
`SessionEvent::DurabilityRollback` (`sandbox_id`, the `ManifestRef` the resume
rewinds to, and a human-readable `reason`) and increments the
`engram_durability_rollback_total` counter. The event is excluded from
`rewind_session_to_cursor`'s tombstone UPDATE — like the other control-plane
facts — so it outlives the very rewind it warns about; the web timeline renders
it as a destructive warning card and `engrams session log` shows it inline.
Alerting keys on the counter (it must be ~0 — every increment is real,
user-visible data loss). The logic lives in a `reap_quarantined_survivor`
helper so the emit + payload are unit-testable against the in-proc backend, and
the exclusion-set SQL change carries the ADR 0098 D4 conformance obligation
(extended `rewind_excludes_coordinator_facts`).

**The rollback is decided at exhaustion, not at destroy time** (PR #829 review
finding). Once the exhaustion branch is taken the session settles `HostLost`
(`exhaustion_settles_host_lost`), and the next resume rewinds to the last
published manifest *unconditionally* — whether *this* attempt's `destroy` lands
or the dead-host straggler sweep re-destroys the sandbox and drives
`HostLost → Idle` later. The sweep only ever emits `status_changed` and never
re-invokes the reap, so an earlier draft that emitted only inside the
successful-`destroy` arm left a hole: a transient reap-destroy failure that the
sweep later completed performed the exact rollback *silently*. The reap
therefore emits the event + counter FIRST, at exhaustion, independent of the
destroy outcome; the `reason` string is outcome-neutral ("VM destroy
initiated") because either the reap or the sweep finishes the mechanics. A
destroy failure here is not a reprieve — the loss is already fact — so it is
logged and left to the sweep.

This does not change the recovery *mechanism* — the write loss was always
possible on this path; the addendum only makes it observable instead of silent.
The deeper fix (bounding the acked-but-unuploaded window itself) remains the NBD
acked-write-loss work, out of scope here.