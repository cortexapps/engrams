# ADR 0094: First-prompt delivery latency — wake the deliver op when the boot completes

- Status: **Proposed** — root-caused + fixed 2026-07-15; dev-VM validated.
- Date: 2026-07-15
- Supersedes the abandoned "resume storm-shield" draft that first held
  this number (PR #675, closed unmerged — see Rejected alternatives).
- Related: ADR 0079 (the session-op executor + the *resume* sibling-
  deliver wake this extends), ADR 0092 (whose "40 s = guest stampede"
  TTFM reading this corrects; its density work is unaffected),
  engrams#669 (nice-boost, reverted — same misdiagnosis).

## Symptom

Time-to-first-message (session create → first assistant token) on the
dev-brain image was ~40 s. ADR 0092's WS0 campaign attributed it to the
resumed guest's JVM wake-up stampede starving the freshly-spawned
harness.

## Root cause (measured on the dev VM, real prod dev-brain base)

The ~40 s is **the coordinator deferring the initial prompt's `Deliver`
op through the session's boot, then waiting out its retry backoff** —
not the guest, not claude, not our harness adapter, not egress:

- `claude --version`: **225 ms** (binary + node runtime load is cheap).
- bare `claude --print "pong"` (real API): **3.0 s**.
- claude with the *exact* harness flags, prompt fed immediately: **2.3 s**.
- Full harness path: **40 s** — the adapter spawns claude, then dead
  silence until the turn completes; claude answers ~2 s after it finally
  receives the prompt on stdin.

The coordinator enqueues a `Deliver` op for the create prompt while the
session is still `pending` (booting). Deliver can't run yet → it defers
("session is pending — not deliverable") and is requeued on a **linear
backoff** (`(attempts+1)×2 s`, capped 60 — `session_ops.rs`). Nothing
makes it *ready* again when `CreateBoot` flips the row to `Active`, so
the accumulated backoff (4+6+8+10…s) plus the 5 s fallback poll elapses
before the prompt is forwarded — ~36 s after the session is actually
serviceable. The `/proc` trace of the waiting claude confirms it: idle
in `epoll_pwait`, no CPU, no reads — waiting for stdin that hasn't
arrived.

This is the **same class ADR 0079 already fixed for resumes**: a
`Resume{flavor=for_delivery}` (prompt-after-idle) left its sibling
Deliver backed off, and ADR 0079 wakes it on the resume's success. The
*fresh-create* boot (`CreateBoot`) was never given the same wake.

## Decision

Extend the ADR 0079 sibling-deliver wake to `CreateBoot`: when the boot
op finishes (Done, or the session goes terminal), call
`op_wake_queued_kind(session_id, Deliver)` so the executor's next claim
forwards the prompt in <100 ms instead of waiting out the backoff. Same
Done-or-session-terminal gate as the resume case — a boot that merely
retries must not wake the deliver (that would reset its backoff into an
unpaced failure loop); a boot that fails *terminally* wakes it so the
`gone`/failed path stays fast (the woken deliver drops its rows and
completes).

One `match` on `op.kind` now covers both boot ops (`CreateBoot` always;
`Resume` only for `flavor=for_delivery`). No new op, no wire change, no
knob. Delivery durability is unchanged — the Deliver op is still the
durable owner; this only makes it *ready sooner*. If the wake NOTIFY is
missed (crash), the 5 s fallback poll still backstops as before.

## Validation

Dev VM, prod dev-brain base restore, egress allow-listed so claude
completes a real turn: create → first assistant token drops from ~40 s
to ≈ boot (~5 s) + claude cold-start (~2–3 s), with the prompt forwarded
within ~100 ms of `Active` instead of ~36 s later.

## Rejected alternatives (how we found it)

- **Resume storm-shield** (this ADR's original draft; PR #675): agentd
  SIGSTOPs the resumed workload so the harness cold-starts on a quiet
  guest. Built and measured — under a **60 s full-workload freeze the
  TTFM was still ~36 s**, which is what disproved the stampede theory and
  sent the investigation to the prompt-delivery path. Abandoned.
- **Harness nice-boost** (engrams#669, reverted): −10 niced the harness
  to out-run the stampede. Also aimed at the misdiagnosed cause; measured
  no effect. Reverted for one-path clarity.
- **A shorter Deliver backoff**: would shave a few seconds but still
  leaves the prompt waiting on a poll tick after boot; the event-driven
  wake is the correct fix (and is what ADR 0079 already established).

## Consequences

- The dominant TTFM cost on fresh creates is removed; TTFM is now bounded
  by real boot + claude cold-start, not a scheduler backoff.
- The harness adapter and the guest stampede are exonerated — no shield,
  no nice-boost, no adapter surgery needed for first-prompt latency.
- ADR 0092's density work (File-unpinned base, lazy base-shm, orphan
  reap) is untouched and remains correct; only its TTFM attribution is
  superseded here.
