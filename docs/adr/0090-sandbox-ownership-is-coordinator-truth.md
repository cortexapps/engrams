# ADR 0090: Sandbox ownership is coordinator truth — rolls and recoveries must not kill owned VMs

- Status: Proposed
- Date: 2026-07-13
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
