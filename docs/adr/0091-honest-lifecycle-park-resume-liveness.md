# ADR 0091: Honest lifecycle — parking that parks, resumes that aren't recoveries, and a state for dead guests

- Status: Accepted (2026-07-13)
- Date: 2026-07-13
- Commit chain: PR #655 (`5eb7af64`) — the rung-2 capture-lock guard +
  `rung2_park_failed_total`, the `harness_idle` rewind exclusion and the
  derived `CheckpointLag` cause, and `SessionState::Unreachable`
  end-to-end (migration 0104, `SandboxProbe::control_alive`, the
  checkpoint-driver producer, the heartbeat advert, the coordinator flip,
  and the resume verb's recovery arm).
- Issues: 2026-07-11 reliability campaign (`scratch/devbrain-campaign-2026-07-11/REPORT.md` §3, §6, §7); ADR 0074 (parking ladder); ADR 0028 (A.log rewind); ADR 0034 (state-machine discipline)

## Context

Three lifecycle dishonesties, each measured in production:

1. **Rung-2 park never parks.** Every observed idle eviction (N=4) logged
   `rung-2 park: pause failed; falling through to full eviction`, so every
   5-minute idle gap paid a full 15-19 min ~25.8 GB checkpoint. Root cause:
   `PooledBackend::pause()` is a bare passthrough to FC's `PATCH /vm` — it
   bypasses the `capture_in_flight` guard every other capture path respects,
   so the park pause races the periodic checkpoint's own pause/flush/resume
   sequence (600s cadence — any session older than 10 minutes overlaps) on
   FC's single-threaded API socket. The failure was also unclassified: one
   WARN with `error = %e`, no variant breakdown, no metric.
2. **Every clean idle resume claims to be a disaster.** `resume_from_fc_snapshot`
   passes the literal `RecoveryCause::HostFailureRecovery` to
   `apply_rung1_rewind` on every manual/auto resume from `Idle` — a routine
   evict/resume cycle emits `recovered_from_checkpoint {cause:
   host_failure_recovery, rolled_back: 1}` (measured on every campaign resume).
   The `rolled_back: 1` is its own bug: the tombstoned event is the session's
   own `harness_idle` (deliberately NOT in the issue-#529 rewind exclusion
   list) — coordinator-observable bookkeeping, not "guest memory of an action,"
   landing after the checkpoint cursor on every clean cycle.
3. **A dead guest reads `active` forever.** The periodic checkpoint tick
   observes a dead FC (`pre-flush pause: … firecracker.sock: Connection
   refused`) and discards it as a WARN. Campaign C1's replacement VM died
   silently minutes after `run_started`; `session get` said `active` for 16+
   minutes while every exec bounced. `Active` is asserted once at
   `start_agent` success and never re-verified.

## Decision

1. **Park under the capture guard.** The rung-2 pause acquires the same
   per-sandbox capture lock the periodic checkpoint holds (skip-and-retry
   next nomination when contended, mirroring the checkpoint driver's own
   skip posture) so the two can never interleave on FC's API socket. Pause
   failures get a bounded-reason metric
   (`engram_rung2_park_failed_total{reason}`) and log the error variant.
2. **Clean resumes emit nothing; residual lag gets an honest name.**
   `harness_idle` joins the issue-#529 rewind exclusion list (it is the idle
   detector's own nomination input — a fact that stays true across a clean
   cycle and lands after the checkpoint cursor by construction). With it
   excluded, a clean idle resume tombstones zero rows and
   `apply_rung1_rewind`'s early-return emits no recovery event at all. The
   fc-snapshot resume path's hardcoded `HostFailureRecovery` becomes a new
   `CheckpointLag` cause ("resumed from an earlier checkpoint" in the web
   copy) for the rare genuinely-lagging case; `HostFailureRecovery` remains
   on the dead-host/evac paths that actually mean it. (Investigation note:
   the campaign's "fresh ~25.8 GB snapshot before active" was a misread —
   that `snapshot_taken` is the eviction's own host-owned finalize landing
   asynchronously via heartbeat reconcile, not a resume-time capture. No
   snapshot is taken on the resume path.)
3. **`SessionState::Unreachable`.** New non-terminal state: "the coordinator
   believes a sandbox exists, but its guest is not responding on the control
   plane." Producers: the periodic-checkpoint failure arm after N=3
   consecutive socket-level failures (connection refused/reset — not
   timeouts, which a busy guest can cause); `probe_sandbox` extended to probe
   the FC API socket, not just process identity. Consumers: `ensure_active`
   treats `Unreachable` like a recoverable fault — a prompt/exec triggers the
   existing checkpoint-recovery resume (which relocates if needed) instead of
   forwarding into a dead VM; the web/CLI render it honestly. Transitions:
   `Active → Unreachable` (producer), `Unreachable → Active` (probe healed /
   recovery succeeded), `Unreachable → Idle/Evicting/HostLost/Failed` (the
   states Active can already reach — recovery machinery is unchanged).
   Reserves host memory (the sandbox may still be occupying it).

## Consequences

- Idle gaps become cheap once rung-2 actually parks (pause-in-place,
  millisecond ascent) — the 15-19 min eviction checkpoint becomes the
  exception (genuine capture points), not the idle-path tax.
- Clean resumes stop rolling back transcript rows and stop paying a
  ~25.8 GB re-snapshot before going active.
- Every `parse_session_state` / status-match site gains a variant — the
  ADR 0034 every-variant rule applies; the migration adds the CHECK-constraint
  value (high-water: this ADR ships migration 0104).
- A busy-but-alive guest is protected from misclassification by requiring
  socket-level connection errors (not timeouts) and N consecutive ticks.

## Outcome (2026-07-13)

Deployed and verified against prod the same day.

**Decision 1 (park) — confirmed.** The capture-lock race is dead: coord
logs show `rung-2 park: VM paused in place` on every idle nomination
(pre-fix: 4/4 fell through to a full eviction). But verification also
found the park being *undone* 15 minutes later by ADR 0074's dwell
reaper, which descends a parked VM on a clock regardless of memory
pressure — so the latency win did not reach users. That is an ADR 0074
policy bug, not a defect here; see its 2026-07-13 addendum (PR #659),
which retires the dwell clock and makes descent pressure-driven, as ADR
0074's own Decision always specified.

**Decision 2 (honest resumes) — confirmed, with one correction.** The
campaign's claim that resume "takes a fresh ~25.8 GB snapshot before
going active" was a misread: that `snapshot_taken` is the *eviction's*
own host-owned finalize landing asynchronously via heartbeat reconcile,
not a resume-time capture. No snapshot is taken on the resume path. The
mislabeled `host_failure_recovery` cause and the `rolled_back: 1` on
clean cycles were real and are fixed.

**Decision 3 (`Unreachable`) — shipped, not yet exercised.** No zombie
guest has occurred since deploy. The state machine, migration, probe, and
recovery arm are in place; its acceptance test is the next dead FC
process.

**Follow-up found during verification:** on a genuine checkpoint-recovery
resume, the rewind tombstones the *resume prompt's own user-echo* (the
prompt is appended before `apply_rung1_rewind` runs). Cosmetic, predates
this ADR; filed rather than fixed here.