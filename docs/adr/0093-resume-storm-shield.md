# ADR 0093: Resume storm-shield — cold harness spawns get a quiet guest

- Status: **Proposed** — mechanism designed from the ADR 0092 WS0
  measurement campaign; moves to Accepted after the dev-VM spike
  (before/after TTFM on the prod dev-brain artifacts) and a prod
  canary.
- Date: 2026-07-14
- Related: ADR 0092 (the measurement campaign + Goal-2 lever list),
  ADR 0037 (persistent harness — the eventual endgame this bridges to),
  ADR 0080 (agentd re-exec — why the resume marker must be a file),
  engrams#669 (nice-boost, measured insufficient).

## Context: the 40 s is contention, and only contention

Dev-brain time-to-first-message is ~45 s; ~40 s of it is the harness
(claude) cold start racing the resumed guest's wake-up stampede — the
JVM fleet's GC catch-up, health-check retries and timer floods
demand-fault ~5.9 GiB in the first 5 s and saturate all vCPUs and the
disk path. The same cold start on a **quiet** guest is **2.7 s**
(measured, twice: strace'd direct spawn, and a delayed first prompt
through the full harness path).

Every in-place mitigation has been measured or eliminated:

- **nice −10** (engrams#669): verified applied mid-storm; TTFM
  unchanged. The starvation is not CPU-scheduler arbitration.
- **Memory substrate**: File-mode fresh creates (kernel demand paging,
  no userspace fault loop) measure the same ~45 s as UFFD/tmpfs. Not
  the fault path.
- **What remains**: the guest's *disk* path (claude's cold reads
  head-of-line blocked behind the JVM read storm at the virtio/NBD
  queue) plus raw vCPU saturation by kernel/IO time nice cannot
  reprioritize.
- **Page-warming the harness at capture is impossible by
  construction**: base captures carry the ADR 0055 *sentinel* at
  `dyn_0`; the real harness squashfs is hot-swapped at session create.
  Its blocks aren't in the captured image to warm.
- **In-guest cgroup io.weight/cpu.max**: the guest mounts no cgroup2
  hierarchy (the init shim is agentd), and guest-side priorities can't
  reorder host-side queues anyway.

The one intervention that provably works is *sequencing*: give the
harness the quiet guest first, let the stampede run after.

## Decision

agentd (guest PID 1) freezes the resumed workload for the few seconds
the harness needs to cold-start, then thaws it:

1. **Resume detection = the clock step, made durable.** The only guest
   signal that a restore happened is the CLOCK_REALTIME step agentd
   already performs against the KVM PTP device. A step larger than a
   resume-scale threshold (60 s) writes a marker file
   (`/run/engram/resume-step`). A file, not process state, because
   fresh creates re-exec agentd (ADR 0080 RefreshAgent) and either
   generation — pre- or post-re-exec — may be the one that steps
   (the periodic tick is 10 s).
2. **Shield window on the cold-spawn path only.** In the harness
   supervisor's spawn arm (the reattach arm returns earlier — a warm
   harness has no cold start to protect), if the marker is fresh
   (< 120 s): SIGSTOP every userspace process except PID 1 and kernel
   threads, spawn the harness, and SIGCONT everything after a fixed
   grace (default 8 s — cold start is 2.7 s quiet; the margin covers
   spawn + init + the first API send). The stampede still happens, but
   overlapped with model latency instead of ahead of the first token.
   A Track-A live `start_agent` re-issue, VZ/dev backends (no PTP →
   no marker), and plain harness restarts see no marker and never
   shield.
3. **Fail-open everywhere.** Freeze/thaw are best-effort per-pid
   (EPERM/ESRCH skipped); the thaw runs from a spawned timer AND from
   guard drop (spawn failure), is idempotent, and only CONTs the pids
   it stopped. Two backstops heal the pathological captures:
   - on agentd startup, and
   - whenever a resume-scale clock step occurs with no shield active,
   agentd sweeps `/proc` for state-`T` processes and SIGCONTs them
   (logged loudly). This covers "captured mid-shield" (a snapshot taken
   inside the window would otherwise resume with the workload frozen
   forever) and "re-exec'd away the thaw timer".
4. **No knob, no proto change.** The shield keys purely on local
   evidence (cold spawn + fresh resume marker), which is exactly the
   storm case and nothing else. Rollback = roll back the agentd bundle
   (ADR 0080 makes that a re-point).

## What could go wrong (and why it's bounded)

- **First prompt drives a frozen service** (e.g. "run the tests" hits
  a stopped gradle daemon): stalls at most the grace window; the model
  round-trip usually absorbs it.
- **Frozen processes' TCP peers time out**: the window is seconds;
  in-guest peers are also frozen, external peers retry.
- **Eviction snapshots the frozen set**: the startup + clock-step
  sweeps CONT stragglers on the other side; and no evictor acts within
  seconds of a create in practice.
- **Someone's deliberate SIGSTOP gets CONT'd** by the backstop sweep:
  accepted in a single-workload guest; the sweep logs each pid.

## Gates

- Dev-VM spike (prod dev-brain artifacts, the ADR 0092 rig):
  create→first-reply with shield vs without, N≥2 each. Success =
  harness segment drops from ~40 s to single digits with no
  post-thaw instability (services healthy, second turn normal).
- Prod canary: dev-brain TTFM p50 well under the 36.9 s pre-flip
  baseline; no session failures attributable to the freeze window.

## Consequences

- TTFM for heavyweight images stops being hostage to their wake-up
  behavior; the remaining budget is infra (~5–10 s, separately
  attackable) + model TTFT.
- The freeze window is a new invariant others must respect: anything
  added to the guest that must stay responsive during the first
  seconds after resume needs an exemption (today: nothing qualifies).
- ADR 0037 (persistent warm harness) remains the endgame — it deletes
  the cold start instead of sheltering it. The shield is the bridge,
  and stays useful after: a reattached-warm harness's *first turn*
  competes with the same stampede on every resume.
