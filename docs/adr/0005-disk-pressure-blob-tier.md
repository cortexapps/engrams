# ADR 0005: Disk-pressure blob tier; git removed from the platform

Status: superseded by [ADR 0007](./0007-chunked-immutable-storage.md), 2026-05-11
Originally accepted 2026-05-09
Phase: 6 (re-shape)

Supersedes the "blob storage retired" + "git as the workspace
durability primitive" claims of [ADR 0001](./0001-versioned-conversations.md);
amends the "session lifetime is host-bounded" contract of
[ADR 0002](./0002-one-shot-task-runner.md) — durability is now
*snapshot-tier-bounded* (hot or cold), not host-bounded.

> **Superseded note (2026-05-11):** ADR 0007's chunked-immutable
> storage rolled out through Phases 1-7. The "hot tier
> (`local_path`) + cold tier (tar.zst sealed blob)" durability
> scheme this ADR introduced is retired. Snapshots now reference
> content-addressed chunks in `BlobStorage` directly via
> `disk_manifest_id` + `memory_manifest_id`. The
> `engram-host-agent::flush` / `disk_pressure` modules + the
> coord-side seal/unseal helpers + the cold-tier columns were
> deleted in Phase 7 (migration `0020_drop_cold_tier.sql`). Read
> this ADR for historical context; ADR 0007 documents the system
> as it stands today.

## Context

Two independent observations forced a re-evaluation of the durability
model that ADR 0001 + 0002 settled on.

**Observation 1 — git as the platform's workspace durability primitive
is an industry outlier.** A survey of how comparable AI software-factory
platforms persist agent state showed convergent use of *hot + cold
snapshot tiers* — full machine state captured to local NVMe, optionally
flushed to blob, restored on possibly-different host. Modal's filesystem
+ memory snapshots, E2B's pause/resume, Devin's custom blockdiff
hypervisor, Cursor's disk-snapshot + `persistedDirectories`, Ramp Inspect
(layered on Modal's snapshots) — all use this shape. Git appears
universally as (a) seed for the workspace and (b) egress channel that
the agent uses for handoff (PR creation), but **never as the platform's
durability guarantee**. Stripe Minions is the lone counter-example —
explicit one-shot, no durability primitive, accepted state loss on
sandbox death.

**Observation 2 — the premise that motivated git-as-durability has
changed.** ADR 0001 retired the `BlobStorage` subsystem with the math
"5 sandboxes × 8 GiB = 40 GiB, infeasible to upload in GCP's 30-second
preemption window." Correct for that premise. The premise no longer
applies: production tolerance for cross-host failover within a 30-second
window is no longer a goal. The remaining motivation for blob-tier
durability is *disk pressure* — when a host's NVMe fills, evict idle
sessions to free space. That's a minutes-not-seconds budget. A 1.5 GB
zstd-compressed memory snapshot uploads in < 60s on commodity cloud
egress. The math that made blob storage infeasible doesn't apply to the
workload that actually drives the requirement.

A third observation reinforces both: workspace files (the agent's
edits) are vastly smaller than memory state (the FC `memory.bin`).
A typical edit session diverges the rootfs by tens to hundreds of MB,
not GB. Even at the most conservative bandwidth, flushing a session's
divergence-from-bake to blob is bounded by the size of the agent's
work, not the size of the VM. Git was solving the wrong-cost end of
this problem.

## Decision

**Remove git from the platform layer. Reintroduce a cold-tier blob
durability layer driven by disk pressure.** Concretely:

1. **Workspace contents come from the bake image.** Every session's
   `/workspace` is whatever the OCI bake populated. The platform does
   no `git clone`, no `git fetch`, no `git push`. Agents that want to
   land code in a remote do it themselves inside the sandbox using
   `GITHUB_TOKEN` (or an SSH key) mounted via the existing
   `[secrets.X]` machinery — same pipeline that already carries
   `CLAUDE_CODE_OAUTH_TOKEN`, harness creds, etc.

2. **Two snapshot tiers, one primitive.** Hot snapshots (FC + UFFD on
   Linux, APFS clone on VZ) live on local NVMe, give sub-second resume
   on the same host. Cold snapshots are tar+zstd of the FC snapshot dir
   + per-sandbox rootfs.ext4, uploaded to a `BlobStorage` backend
   (S3/GCS/local). Cold resume on any host with the matching OCI
   manifest digest cached.

3. **Flush is disk-pressure-driven, not preemption-driven.** A host-
   agent background loop (`disk_pressure::run`) polls `statvfs`; below
   a configurable threshold, picks the LRU idle session, takes a hot
   snapshot if needed, ships it cold, drops the local copy. Preemption
   stays the contract from ADR 0002 — session goes Dead, no flush
   attempt. Two different problems, two different mechanisms; conflating
   them was ADR 0001's confusion.

4. **Admin endpoints expose the same primitive explicitly.** `POST
   /api/admin/sessions/:id/flush` and `POST /api/admin/flush-idle` fire
   the underlying `flush_session` for testability, drain-before-redeploy
   ops, and integration tests that don't want to synthesize disk
   pressure. Implicit (detector) and explicit (admin) triggers share
   one code path.

5. **Cross-host cold resume.** `POST /sessions/:id/resume` dispatches
   on residency: `Idle` + local snapshot intact → hot resume on the
   same host (sub-second); `ColdEvicted` → scheduler picks a host with
   the matching `manifest_digest` cached, coord drives a download RPC,
   the new host untars + restores; `Dead` or no snapshot → 410 Gone.
   `session_id` is preserved across cold resume.

6. **No git-shaped APIs, types, or columns survive.** `WorkspaceSpec`,
   `SessionKind`, `Session.checkpoint_branch`, `agent_commits`,
   `engram-coordinator::git_workdir`, `engram-host-agent::checkpoint`,
   `auto_checkpoint`, `POST /sessions/:id/{checkpoint,fork,diff}`,
   `engram session {log,diff,fork,checkpoint}` — all retired in this
   pivot. Schema migrations drop the columns directly; no shims.

## What this preserves from ADR 0001

- The conversation-log durability story. `session_events` in Postgres
  remains the conversation source of truth. SSE replay via
  `Last-Event-ID` keeps working.
- The split between "what the platform persists" (now: snapshots +
  conversation log) and "what the agent does inside the box" (any git
  push, any HTTP call, any tool use). Agents still have full freedom
  inside the sandbox.

## What this preserves from ADR 0002

- One-shot task semantics. Sessions are bounded — no infinite
  cross-host migration, no eternal lifetime.
- Hot suspend + auto-resume on the same host for idle eviction. Same
  sub-second UFFD path.
- `engram session fork` *as a workspace primitive* goes away (no git
  branch to fork from), but a cold snapshot can serve the same role —
  fork-from-cold-snapshot-into-fresh-session lands as a follow-up if
  the use case proves out.

## What this changes from ADR 0002

- `Session lives ↔ FC snapshot exists on its origin host` becomes
  `Session lives ↔ snapshot exists somewhere (hot tier on any host
  whose NVMe still has it, or cold tier in blob)`. `Dead` is now
  reserved for "the cold tier is also gone" (deleted blob, KEK loss,
  intentional GC).

## Implementation tracks

Bundled into a single 8-stage plan:

- **Stage 0** — Foundations: ADR, `BlobStorage` trait + `engram-core`
  surface, three new storage crates (S3/GCS stubs + real local), schema
  migration `0016` adding cold-tier columns, `SessionStatus::ColdEvicted`.
- **Stages 1–3** — Git surface removal: coordinator API, host-agent +
  CLI + web, type + schema cleanup. ~−2700 LOC across the three.
- **Stage 4** — Real S3/GCS impls; `MetadataStore` cold-tier methods;
  KEK-sealed blob URLs reusing `engram-crypto` from Phase 5b;
  `fake-gcs-server` + `minio` wired into `Tiltfile` so the blob path
  runs end-to-end in `just dev` without cloud credentials.
- **Stage 5** — `flush_session` primitive on host-agent + admin
  endpoints firing it explicitly + matching CLI subcommands.
- **Stage 6** — Three-branch resume dispatcher; cross-host cold resume;
  scheduler `pick_for_cold_resume` aware of OCI manifest digest.
- **Stage 7** — Disk-pressure detector calling the same `flush_session`
  primitive Stage 5 ships.
- **Stage 8** — Documentation rewrite. README, DESIGN.md, ADR 0001/0002
  supersession headers.

## What this does *not* solve

- **Differential / delta snapshots.** Stage 5 ships full-snapshot
  tarballs. A second-pass diff format (Devin-style blockdiff) is a
  follow-up if the size/bandwidth math demands it.
- **Cross-kernel-version cold resume.** Hosts in a single deployment are
  assumed to run the same kernel; FC's UFFD-backed restore is sensitive
  to kernel ABI. Mitigation: the bake injects the kernel hash into the
  image; `pick_for_cold_resume` matches on manifest digest, which
  pins kernel transitively. Explicit kernel-version pinning is a
  follow-up.
- **KEK loss recovery.** Same risk profile as Phase 5b registry creds
  and session secrets — losing the KEK means losing readability of
  every cold snapshot. Backup the KEK in a KMS with its own durability
  story; document the procedure.
- **Cold-tier GC.** Snapshots accumulate in blob over time. A separate
  lifecycle policy (TTL on `last_accessed_at`, blob-side bucket
  lifecycle rules) lands as a follow-up.

## Consequences

**Smaller surface.** ~5000 LOC of churn ends up at +1700 net (deletions
roughly cancel additions). One coherent durability story instead of two
overlapping ones (git + hot snapshots).

**Adopters without git workflows can use Engram.** Today every session
needed a writable repo + branch + GITHUB_TOKEN before it could even
start; non-code-edit agents (ops bots, data analysis, Slack-driven Q&A)
paid for git they didn't use. After this pivot, the platform doesn't
care whether the agent does anything git-shaped.

**Disk pressure becomes survivable.** Today, hosts that fill up have
no recourse other than killing sessions outright. After this pivot,
they ship the LRU idle sessions cold and recover the disk. Sessions
come back on the next access from any host with capacity.

**Cross-host cold resume reintroduces a complexity dimension** (ADR
0002 deliberately removed it). The mitigations: (a) OCI manifest
digest match on the destination host, so VM state isn't restored
against a kernel ABI that diverges; (b) the same `flush_session`
primitive backing both implicit (detector) and explicit (admin)
triggers, so the code path that production exercises is the code path
tests exercise; (c) `Dead` is now genuinely terminal — once both tiers
are gone, there's no continuation.

## Alternatives considered

- **Stripe Minions shape — no durability primitive at all.** Considered;
  rejected. Loses pack-host-under-disk-pressure: the host has to kill
  sessions instead of pausing them. The user explicitly wants the pause
  semantics for disk pressure. Reasonable for a one-shot-only product
  surface; doesn't fit the long-running interactive sessions Engram
  also serves.

- **Daytona/Coder shape — persistent volumes mounted into sandboxes.**
  Considered; rejected for v1. Adds a stateful filesystem subsystem
  (consistency model, mount semantics, evict-while-mounted handling)
  that's its own product. Reconsider if Engram pivots toward dev-
  environment-as-a-service.

- **Keep git as a `WorkspaceCheckpoint` trait, add cold tier alongside.**
  Considered; rejected as carrying both for no clear win. The
  durability-primitive role is occupied by the snapshot tier; git as
  an *agent-level* tool already has its place inside the sandbox via
  the `[secrets.X]` pipeline. Keeping the platform-side git path for
  optional use just means two things to maintain.
