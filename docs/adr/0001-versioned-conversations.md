# ADR 0001: Sessions are versioned conversations, not migratable VMs

Status: **superseded by [ADR 0005](./0005-disk-pressure-blob-tier.md), 2026-05-09**

Original status: accepted, 2026-04-28
Phase: 4

> **Superseded.** ADR 0005 reverses the two load-bearing decisions
> here:
>
> - **"Blob storage is removed entirely"** is gone. The cold-tier
>   `BlobStorage` subsystem is back, this time as the disk-pressure
>   flush target rather than a 30-second-preemption-window
>   replication path.
> - **"Engram becomes a system for versioned agent conversations"**
>   with git as the workspace durability primitive is replaced by
>   "sessions are bake-image sandboxes with hot+cold snapshot
>   tiers." The conversation log in `session_events` stays, but git
>   is no longer the platform's workspace persistence — agents push
>   inside the sandbox using mounted credentials when they want to.
>
> What this ADR got right and ADR 0005 keeps:
>
> - Live VM migration to blob in a 30-second window is infeasible
>   (the math holds; ADR 0005 just stopped optimizing for that
>   premise).
> - The conversation log lives in Postgres `session_events` — that
>   stays the source of truth for the agent's play-by-play.
> - The split between "what the platform persists" vs. "what the
>   agent does inside the box."
>
> Body kept verbatim below for historical context.

---

## Context

Phase 4's original framing in DESIGN.md was "cloud abstraction & spot
tolerance," with the deliverable "kill a spot host, sessions
seamlessly resume on remaining hosts; no client-visible downtime." The
design called for live VM migration via memory-snapshot replication to
blob storage (GCS / S3) every few seconds, so a preempted host could
hand off in-flight sandboxes within GCP's 30-second preemption window.

Two findings reshaped the deliverable:

**1. Live VM migration to blob storage in a 30-second window is
mathematically infeasible.** A realistic engram host packs 5+
sandboxes at 8 GiB each = 40 GiB+ of memory. At GCS' ~250 MB/s steady
upload, that's ~3 minutes — not 30 seconds. Continuous pre-copy
(Drafter / Loophole's Firecracker fork) closes the gap, but it's a
year of work. Comparable systems sidestep the problem entirely:
Modal forces non-preemptible at 3× premium, Fly tells users to
replicate at the app layer, E2B has no spot story, Lambda is stateless.

**2. Agents are not arbitrary VM workloads.** Their load-bearing
state is externalized: workspace files, the conversation transcript,
and idempotent re-runnable tool calls. Almost nothing critical lives
in process RAM. The right durability primitive for agents is the
conversation log + workspace, not VM memory.

## Decision

**Engram becomes a system for versioned agent conversations.**

- A session is a versioned conversation: a sequence of orchestration
  events recorded in Postgres (`session_events`) plus workspace files
  at checkpoint boundaries committed to git
  (`engram/sessions/<session_id>` on the writable repo's remote).
- **Storage split.** Postgres = conversation source of truth; git =
  workspace source of truth. They don't duplicate: transcripts are
  never in git, workspace files are never in Postgres.
- **Preemption is tolerable.** When a host dies, sessions land in
  `PendingReassign` with intact conversation history. The orchestrator
  does *not* race to keep them running. The calling system (or a
  human operator) deliberately resumes when ready.
- **Hot snapshots stay valuable** for the *local* problem of packing
  hosts: idle sessions auto-suspend to local NVMe and auto-resume on
  the next request. They are *not* the cross-host durability
  mechanism.
- **"Going back in time" is first-class.** `engram session fork
  <id> --at <event_idx>` branches the conversation+workspace at any
  past checkpoint. Operators iterate by forking from where the agent
  went off the rails; autonomous outer-loop systems use it to retry
  strategies.
- **Vendor-agnostic** via a uniform harness protocol (Track A). Each
  agent (Claude Code, Codex, Aider, in-house) needs a small adapter
  implementing the harness wire protocol; the protocol is the same.
  Phase 4 ships Claude Code as the reference adapter (Phase 5+).
- **Blob storage is removed entirely.** No snapshot replication, no
  cold-tier fetch, no `BlobStorage` trait, no `engram-storage-{local,
  gcs,s3}` crates. Image distribution (Phase 5+) will use a Docker
  registry — more standard, plays with existing tooling.

## What Engram does *not* solve

This is the explicit contract for callers. Engineering it any other
way is out of scope.

- **Idempotency for non-idempotent tool calls.** A session that calls
  `slack.post()` and gets preempted may, on resume, redo the post —
  depends on whether the harness flushed before the VM died. Tool
  wrappers must use idempotency keys derived from `(session_id,
  tool_call_id)`. Same model as Modal, AWS Step Functions.
- **Fork determinism for sessions with side effects.** Forking from a
  checkpoint that pre-dates a `db.delete_user(123)` call replays a
  conversation into a world where 123 is *already* deleted. Fork is
  best-suited to code-edit tasks; for operational tasks with side
  effects, "you're replaying a history into a world that's moved on."
- **Mid-tool-call work preservation.** A 5-minute `pytest` running
  when preemption hits has no checkpoint since the last completed
  run. On resume, the agent reruns `pytest` — another 5 minutes.
  Acceptable: checkpoints land at run boundaries, not arbitrary
  millisecond points.
- **Interactive-session UX during preemption.** The contract is
  "preempted sessions land in `PendingReassign`; resume is a
  deliberate action." For interactive flows (Slack pair-programming),
  this means the user sees "session paused, click resume." For
  autonomous outer loops, the calling system handles resume — the
  dominant target.
- **Cross-vendor agent compatibility out of the box.** Each agent
  needs a harness adapter. Phase 4 ships Claude Code; others land as
  adopters need them.

## Consequences

**Simpler.** Phase 4 retired ~1500 LOC of replication subsystem
(snapshot replicator, cold-tier fetch, `BlobStorage` trait + 3
backend crates, env-var permutations). Net code change is ~+5k LOC
(harness protocol + checkpoint primitive + git workdir + sessions
inspect + CLI verbs); this would have been ~+15k LOC for the live-
migration design.

**Slower preemption recovery.** A preempted session takes seconds-to-
minutes to come back (depending on caller-side logic), not
milliseconds. For autonomous workflows this is fine — the work
queue moves on and the resumed session catches up. For interactive
sessions it's a UX wart that has to be papered over by the calling
client.

**Dense checkpoint cadence.** One commit per completed agent run.
This drowns the checkpoint branch on chatty agents (multi-hour runs
with hundreds of tool calls become a few dozen commits, not hundreds).
Reads naturally as a session log; PRs land cleanly. Per-tool-call
commits would have been noise.

**Forks come cheap.** Because checkpoint branches are inert artifacts
on the writable repo, `engram session fork --at N` is just `git push
<sha>:engram/sessions/<new_id>` plus a session row copy. No special
infrastructure.

**Vendor lock-in is at the harness adapter layer, not Engram.**
Switching from Claude Code to Codex is a new ~500 LOC adapter, not
a re-architecture. The wire protocol (`HarnessEvent` /
`HarnessCommand` in `engram-harness-proto`) is the contract.

## Alternatives considered

- **Continuous pre-copy via Drafter.** Solves cross-host VM migration
  in the 30s window. Estimated cost: a year of FC patching + peer
  replication work. Out of scope; revisit if a customer explicitly
  needs sub-30s preemption recovery.
- **3× premium non-preemptible only (Modal's path).** Punts the
  problem to billing. Considered; rejected because cheap spot
  capacity is a real engram differentiator vs. Modal/E2B and we'd
  rather give callers the pause-and-resume contract than force them
  off spot.
- **App-level replication (Fly's path).** Tells the agent / calling
  system to manage durability. Considered; rejected because every
  caller would re-implement workspace+conversation persistence
  badly. Engram already has the Postgres + git infra to centralize
  it once.
- **Per-tool-call git commits.** Simpler mental model but produced
  300-commit branches on long runs. Rejected for cadence reasons —
  one commit per completed run reads as a session log.

## Implementation tracks

Track 0: Blob storage removal.
Track A: Harness protocol foundation (`engram-harness-proto`,
`engram-harness-noop`, host-agent harness hub).
Track B: Hot-suspend pack hosts (idle evictor + auto-resume).
Track C: Git workspace + checkpoint primitive (smart-bootstrap,
auto-checkpoint on Idle/RunCompleted, manual checkpoint endpoint).
Track D: Preemption best-effort flush.
Track E: Cleanup + dev ergonomics + docs.
Track F: Conversation + git-native verbs (log/diff/fork/resume,
CLI surface).

Track F.6 (`POST /sessions/:id/pr` — GitHub-only) is deferred
until the prod auth path (GitHub App + Workload Identity) lands;
the runtime API for PR creation is identical whether it goes
through a per-session token (Phase 4 dev) or App installation
token (Phase 6+).
