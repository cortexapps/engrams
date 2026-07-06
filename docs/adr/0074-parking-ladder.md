# 0074 — The parking ladder: cancellable, pressure-driven eviction

Status: Proposed (2026-07-06)

Issue: #545 (2026-07 core-ops overhaul, Tier 2). Depends on: #540 (RAM
ledger — hard for rungs 2–4), #543 (op log — soft; cancel becomes an
op-cancel once it lands). Builds on ADR 0067 (the outbox delivery
driver is where a returning prompt meets the ladder) and ADR 0028
(diff-first checkpoints are the cheap-pause primitive).

## Problem

Idle is a cliff, not a ladder. From the instant of nomination the
session is on a one-way conveyor — nominate → snapshot → destroy →
Idle → full rebuild on return — with no `Evicting → Active` edge in
the FSM, so the single most common "the user is here" signal (a
follow-up prompt during eviction) waits for a healthy VM to be
destroyed and then pays the full resume (prod p50 12.2s / p95 89.1s;
prompt→run_started p50 ≈24.5s; 45/45 resumes rewound transcripts) plus
a 3–21s lease collision with the eviction's own finalize. Descent
fires on a 300s clock regardless of memory pressure; the
pressure-aware fix shipped default-off because parked residency is
unaccounted (the ADR 0046 honesty problem → #540).

## Decision

Idle becomes descending rungs driven only by memory pressure, each
with a cheap ascent. **Invariant: eviction is reversible up to the
moment its resources are actually reclaimed, and it descends only as
far as pressure requires.**

1. **Nominated** (`Evicting`, `park_rung=1`, VM untouched): ascent is
   one lease-guarded PG CAS over the new `Evicting → Active` edge —
   the only new FSM edge in the overhaul (no new states; op rows are
   the in-flight visibility per the resolved overhaul decision).
2. **Parked-paused** (`park_rung=2`; FC paused after a 38–53ms diff
   whose upload runs in the background): ascent = un-pause + the same
   edge. Frees no RAM — it buys interactive-return latency, dwell
   capped (default 900s; stale guest TCP), skipped under acute
   pressure.
3. **Parked-local** (`Idle`, `park_rung=3`; VM destroyed, snapshot
   staging + NBD backing retained on NVMe): ascent = resume pinned to
   the parking host from local artifacts (~0.5–2s). NEW machinery —
   explicitly not `live_attach` (which reattaches live processes).
4. **Evicted-remote** (`park_rung=0`): today's full GCS restore, now
   the rare case.

Pressure-driven descent becomes the ONLY mode; the clock TTL survives
as candidacy, never as a reclaim trigger; hard TTL remains the
absolute ceiling. Rung-2/3 residency charges `allocatable_mib` (#540).
The speculative `ensure_rung` typing hint is dropped from v1.

## This PR: rung 1 only

Ships standalone (the issue's own staging): the `Evicting → Active`
legality edge, `park_rung`/`parked_at` schema, nomination stamping,
and a lease-guarded cancel folded into `ensure_active` — so ADR 0067's
outbox delivery driver cancels the nomination inline and the returning
user's prompt proceeds against the untouched VM. The 8s Evicting hold
loop survives only as the capture-already-started fallback. Rungs 2–4
land as follow-ups on this ADR.

## Divergence log

- **No host `cancel_evict` RPC.** The issue specified one ("clear the
  eviction-inflight marker, reset idle clocks") — but ADR 0067 phase 3
  deleted the host-side eviction markers and idle clocks entirely; the
  host holds NO nomination state. Cancellation is purely
  coordinator-side: `try_acquire` the session lease (no retry — a held
  lease means the capture pipeline owns the session; fall back to the
  hold loop), CAS `Evicting → Active`, clear `park_rung`, release. The
  lease guard is load-bearing: the eviction scanner treats `Active` as
  a legal post-lease input (the drain path), so an unguarded CAS could
  cancel "under" a pipeline that then proceeds to evict an Active
  session.
