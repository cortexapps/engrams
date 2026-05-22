# ADR 0015: System design v2 — abstractions, not scab fixes

Status: Proposed, 2026-05-22. Section **M1 (in-VM service unification)
is Accepted and ships in this PR**; the remaining eight sections stay
Proposed pending their own implementation ADRs.

## Context

This ADR is the result of a system-walk-through and design conversation
on 2026-05-22, immediately following a week of cascading prod incidents
(warm-pool CPU vendor mismatch, host MIG rolls evicting active sessions,
cold-Active-before-agentd race, stale templates blob_not_found loops)
and one productive day of building out the dev-vm iteration loop. With
the loop now turnaround-fast — `just integration-up` + `just
integration-session` + the dev-vm port-forward give us full UI + API +
NBD locally in seconds — we have the bandwidth to look at the system
critically and ask which structural choices are causing the recurring
class of bugs we keep patching.

**What's working.** Several abstractions in the current codebase are
genuinely load-bearing and should be preserved as the model for
everything else:

- **`SandboxBackend` trait** — one set of code drives FC, VZ, and
  Process backends with no per-backend logic leaking into the
  coordinator. The reason `mode=all` and `mode=coordinator` share a
  single coord codebase is this trait.
- **`HostClient` trait** — one set of code dispatches sandbox ops in
  both the in-process (mode=all) and gRPC (mode=coordinator) cases.
  Adding a new RPC means adding one method, not threading wire
  formats through five layers.
- **Content-addressed chunked storage** (ADR 0007) — manifests,
  rootfs, memory, harness substrates all refer to identity, not
  location. Dedup is implicit. This is what makes warm-pool refill
  affordable and is the right pattern.
- **The `template_ref` value** (ADR 0014) — warm pool reasons about
  templates without coupling to OCI tags.

**What isn't working.** The codebase has accreted a number of
patterns that were each the right response to an immediate problem
but left underlying complexity in place. They form a class:

1. **Two in-VM processes with two wire protocols.** `engram-bootstrap`
   on vsock 1025 handles `BootstrapLaunch`; `engram-agentd` on 1024
   handles `WireRequest`. Both must be probed for readiness
   independently. The "is the session ready" question requires asking
   the right one, and the no-harness create path that skips bootstrap
   entirely produces the early-eof race we reproduced today.
2. **Fuzzy `SessionStatus::Active`.** What does Active mean? Sandbox
   created? Agent reachable? Harness running? Depends on call path.
   /exec, /shell, snapshot.ensure_active all interpret it differently.
3. **Split-brain registry vs PG.** `HostRegistry` (in-memory
   `sandbox_id → host_id` map) and `Postgres.sessions` are both
   sources of truth. They diverge on MIG roll until the heartbeat
   timeout fires.
4. **Host-pinned sessions.** A session lives on one host for life.
   Host dies, session dies. Today this is policy; with snapshot/
   restore already implemented, it doesn't have to be.
5. **Bake-time canonical snapshot baked into the OCI artifact.** This
   is the design choice that made the AMD-baked → Intel-restored
   disaster possible. It conflates "what image content" with "what
   memory state at boot".
6. **Three wire protocols across the harness boundary** —
   `BootstrapLaunch` (host→bootstrap), `WireRequest::Exec` (host→
   agentd), `HarnessEvent` (harness→agentd→host→coord). They evolved
   independently; the dispatch glue between them is bespoke at each
   crossing.
7. **Path-baked FC `state.bin`.** FC's snapshot embeds host filesystem
   paths verbatim. We work around with a canonical-symlink layer
   (`<work_dir>/rootfs/<sandbox_id>.dev`) that any sibling host can
   re-create. The layer works but produces brittle one-off code (the
   relative-path-leaks-into-state.bin bug we fixed today is one
   manifestation; the symlink-rewrite on warm restore is another).
8. **`enable_image` is a non-atomic cascade.** Materialize → verify →
   cascade. Partial failure can leave a `templates` row pointing at
   blob keys that aren't in storage, and the host's warm-pool refill
   loop will spam `blob_not_found` for that row forever until manual
   intervention.
9. **Split host-coord channel.** HTTP/JSON from host → coord, gRPC
   from coord → host (ADR 0013). The split was made to keep coord
   pods stateless across restarts. The tradeoff: heartbeat is a
   POST every 5 s, not a stream; "host is going away" has no signal;
   lifecycle is fragmented across five HTTP handlers and a gRPC
   service.

**The framing this ADR adopts.** Each of these is an abstraction
opportunity, not a bug list. The right move for each is to identify
the trait or interface that subsumes the existing surfaces, ship the
new abstraction, and *delete* the code that motivated this ADR. Net
LOC should fall. We use `SandboxBackend` and `HostClient` as the bar:
they retire per-backend code, not pile on top of it.

**No backwards compat, no deprecated code.** Every milestone in this
ADR ships as an **atomic cutover**. There is no
"deprecated-but-tolerated" middle state, no `#[allow(deprecated)]`,
no hidden CLI flags retained for legacy CI, no `#[serde(default)]`
patches to keep old wire frames decodable, no stub binaries left for
old images to call. If a struct field, wire variant, RPC method, or
crate becomes unnecessary as a side effect of a milestone landing,
it is deleted in the same PR. All in-flight artifacts (bakes,
images, snapshots, FC `state.bin`s) that the cutover would break are
either re-generated in the same change set or accepted as dropped
state. The reason we accumulated the scab-fix codebase the v2
direction is trying to fix is that we kept landing changes "with
back-compat" — every deprecation slot turned into a load-bearing
permanent feature. Stop doing that. Every milestone implementor:
when in doubt, delete more, not less.

This ADR captures all nine moves so they exist as a coherent v2
direction. Only M1 ships in this PR; M2–M9 will spawn individual
implementation ADRs as we get to them. Some (notably M9) may end up
Rejected after closer examination — the goal is to make every one of
them a deliberate decision rather than a pile of latent work.

## Decision

Nine sections. Each is its own abstraction-introducing move. M1 is
Accepted and being implemented now; M2–M9 are Proposed.

| # | Abstraction | Status |
|---|---|---|
| M1 | `GuestService` — one in-VM RPC surface, one readiness signal | **Accepted, in this PR** |
| M2 | `SessionState` — explicit, gated state machine | Proposed |
| M3 | `HostRegistry` as a TTL'd cache of PG truth | Proposed |
| M4 | `Sandbox` as a content-addressed migratable value | Proposed |
| M5 | `ImageBundle` (content) vs `WarmTemplate` (host-local snapshot) | Proposed |
| M6 | Harness events as one typed stream on `GuestService` | Proposed |
| M7 | FC drive references as content hashes via a `DriveResolver` trait | Proposed |
| M8 | `enable_image` as a saga + periodic blob-orphan GC | Proposed |
| M9 | Coord ↔ host as one bidirectional trait (or accept the split as final) | Proposed, flagged |

---

### M1 — `GuestService`: one in-VM service surface

**Problem.** Two processes (`engram-bootstrap` on vsock 1025;
`engram-agentd` on 1024), two wire protocols (`BootstrapLaunch`;
`WireRequest::*`), two readiness signals. The host has to know which
process to probe for what (start_agent → bootstrap; exec/shell →
agentd) and which protocol to speak. Sessions with `harness: none`
skip the bootstrap dance entirely and produce the "Active before
agentd is reachable" race we reproduced on the dev-vm today. The
split exists because bootstrap predates agentd's exec handler — it's
not a design, it's a history.

**Abstraction.** One in-VM service exposing one `GuestService` trait,
mirror of `HostService` on the coord ↔ host side. The agentd crate
becomes its canonical implementation. The host calls into it via a
single `GuestClient` (existing `WireRequest` enum's variants become
the trait's method dispatch). Empty-argv `SpawnHarness` is a readiness
probe — any successful RPC proves the daemon is up.

**What it deletes.**

- The entire `engram-bootstrap` crate (~400 LOC) — crate dir, workspace
  member, all references in CI / justfile / scripts.
- `BOOTSTRAP_VSOCK_PORT`, `BOOTSTRAP_READY_BYTE`, `BootstrapLaunch`
  from `engram-harness-proto`. The `serde_json` dev-dep added for
  back-compat tests goes with them.
- The `&` fork in `DEFAULT_INIT_SHIM` in `engram-image-builder`,
  plus all stale comments referencing the two-process boot dance.
- The `bootstrap_binary` field on `AgentInjection` and the bake-
  injection branch that consumed it.
- The `--inject-bootstrap` CLI flag on `engram-cli image build`,
  along with its plumbing through `image_build_local`.
- The bootstrap-1025 retry path in
  `engram-sandbox-firecracker::start_agent` (the helper
  `connect_fc_vsock_with_retry` we already landed today covers
  agentd-1024).
- The VZ backend's BootstrapLaunch path —
  `engram-sandbox-vz::start_agent` switched to SpawnHarness on
  agentd-1024 like FC, and the `engram-harness-proto` dependency on
  the VZ crate is dropped.
- The `(None, _)` branch fork in `coord/api/sessions.rs` — both
  harness and no-harness sessions go through the same start_agent
  call; the early-eof race becomes structurally impossible.

**What gets added.**

- `WireRequest::SpawnHarness { argv, env, harness_dev, harness_mount }`
  + `WireResponse::HarnessSpawned { pid }` in
  `crates/engram-agentd/src/proto.rs`.
- `crates/engram-agentd/src/harness_supervisor.rs` — owns the harness
  child process handle; kill+respawn on subsequent `SpawnHarness`
  (needed for resume, identical semantics to bootstrap's existing
  loop).
- One new branch in `crates/engram-agentd/src/handler.rs::dispatch`.
- Harness-drive mount logic lifted verbatim from
  `engram-bootstrap/src/main.rs` into the new supervisor module.

**Migration.** Atomic cutover. The `engram-bootstrap` crate, the
`BOOTSTRAP_*` constants, `BootstrapLaunch`, the `--inject-bootstrap`
CLI flag, the `bootstrap_binary` field on `AgentInjection`, and the
init-shim fork are all deleted in this PR. Existing in-flight bakes
become unbootable; the new bake replaces them. Per the no-back-compat
principle above, we do not leave the bootstrap crate as a stub.

**Open questions.**

- Should we rename `agentd` → `agent` since it now does more than
  daemonize? No — it's still a long-lived daemon at PID 1. Naming
  unchanged.
- Should the SpawnHarness response carry the child pid? Yes — useful
  for guest-side debugging; trivial to populate.

**Expected net diff.** ~−500 LOC.

---

### M2 — `SessionState`: explicit, gated state machine

**Problem.** `SessionStatus` today is `{Pending, Active, Idle, Dead,
Failed, Paused}`. Active fires whenever the host returns from create;
that doesn't mean usable. Each call site (`/exec`, `/shell`,
snapshot.ensure_active) has its own interpretation and its own
retry/wait logic. The harness=none race today is one symptom; the
"why does /shell sometimes fail right after create" complaint is
another.

**Abstraction.** A typed state machine where every transition has a
trigger (a specific event) and a guarantee (what's true post-
transition). `SessionState::Active` means: sandbox exists, agentd is
reachable, harness (if any) is running. /exec and /shell against a
non-Active session return 409 with the actual state, not a vsock
race. New states:

- `Pending` — row written, sandbox not yet picked
- `Created` — sandbox bound to a host; nothing else proven
- `GuestReady` — agentd RPC succeeded once (post-M1, this is when
  start_agent returns)
- `Active` — harness is running (or harness=none and GuestReady)

Transitions are owned by `coord/api/sessions.rs`; the state machine
itself lives in `engram-core/src/types/session.rs`. Event emission
becomes the formal "we have crossed this barrier" signal.

**What it deletes.** The per-caller retry loops in `/exec`,
`/shell`, snapshot.ensure_active that exist because "Active" doesn't
mean usable.

**Open questions.** How do we express the typed transitions in Rust
ergonomically? Probably an enum with explicit `transition_to`
methods (not typestate — typestate's compile-time guarantees aren't
worth the API churn here).

---

### M3 — `HostRegistry` as a TTL'd cache over PG

**Problem.** `HostRegistry` keeps `sandbox_id → host_id` in memory;
`Postgres.sessions` keeps the same mapping on disk. They diverge on
MIG roll — the host vanishes, the DB row points at a dead host, the
registry has stale entries until heartbeat-timeout fires. The
divergent failure messages we see (`tcp connect error` from gRPC,
`sandbox not found` after registry timeout) are this split brain.

**Abstraction.** PG is authoritative. `HostRegistry` is a strict
read-through cache. Heartbeat-loss explicitly invalidates the cache
entry; subsequent RPCs against a stale ID fail-fast with a
`SessionState::HostLost` rather than a TCP connect error. Once M2
lands, the state machine has a clear transition: heartbeat-loss →
SessionState becomes `HostLost`, the cache is invalidated, the next
call gets a clean 410.

**Open questions.** Cache freshness — TTL or strictly invalidate-on-
heartbeat-loss? Probably the latter, with a fallback TTL for
operator-paused hosts.

---

### M4 — `Sandbox` as a migratable value

**Problem.** Today a session is pinned to one host for life. Host
dies → session dies. The snapshot/restore primitives are already
built (M1 of ADR 0014); they're used for warm pool but not for
session evacuation. With M3 in place (PG truth, cache invalidation),
the missing piece is policy: "when a host disappears, snapshot any
evacuable sessions to BlobStorage, restore them on a healthy peer".

**Abstraction.** `Sandbox` becomes a value whose identity (the
`sandbox_id` token in the registry / DB) can be substituted: snapshot
→ restore-on-peer → new sandbox_id, but same logical Sandbox from the
session's perspective. `HostClient::evacuate(sandbox_id, target_host)`
becomes a first-class op. Operator drains use the same primitive as
panic-evacuation.

**What it deletes.** The "your session vanished after a MIG roll"
UX class. Per-host operator drains as a separate concept.

**Open questions.** Eligibility: which sessions can be migrated?
Ones in `Active` state with a recent snapshot, probably. Idle
sessions are easier (snapshot already in BlobStorage from ADR 0011).
Cold sessions mid-boot might not be migratable; fail-loud is fine.

---

### M5 — `ImageBundle` vs `WarmTemplate`

**Problem.** Bake-time canonical memory snapshot is baked into the
OCI artifact. This makes "image" and "memory state of a booted
sandbox of that image" the same concept, even though they're not.
The cross-CPU-vendor restore problem we hit (T2CL Intel-baked,
AMD-restored; AMD-baked, Intel-restored) is the direct manifestation:
images become CPU-vendor-coupled because their snapshot is. Also: the
bake step is ~30 s instead of ~3 s; `enable_image` blocks ~60 s
waiting for the first warm slot.

**Abstraction.** Split the concepts:

- **`ImageBundle`**: rootfs ext4 (chunked) + manifest. Pure content.
  CPU-agnostic, kernel-agnostic, snapshot-free. This is what the
  bake produces. Push time ~3 s.
- **`WarmTemplate`**: an `ImageBundle` plus a memory snapshot
  *generated locally at runtime* on the actual host CPU. First
  session on a fresh host pays cold-boot cost and produces the
  template; subsequent sessions lease from it.

**What it deletes.** The bake's `--capture-canonical-memory` flag
and all its supporting code; the bake → templates row cascade in
`enable_image`; the cross-vendor compatibility tax. Bake CI gets
~10× faster.

**Open questions.** Where does the first WarmTemplate snapshot get
generated — explicit operator action, first session, or a host-side
warm-template-generator process? Probably "first session of a
fresh-on-this-host image triggers template generation as a side
effect".

---

### M6 — Harness events as one typed stream on `GuestService`

**Problem.** Three protocols across the harness boundary:
`BootstrapLaunch` (host→bootstrap; gone after M1),
`WireRequest::Exec` (host→agentd, replies with exec event stream),
and `HarnessEvent` (harness→agentd→host→coord via a separate
JSON-shaped stream on a different vsock port). Two of them survive
M1, with bespoke dispatch glue between them.

**Abstraction.** All harness↔coord communication flows over the same
`GuestService` stream introduced in M1. `HarnessEvent` becomes a
typed message on the same trait, alongside `Exec` events and shell
bytes. Single multiplexed channel; coord's subscription path
collapses.

**What it deletes.** The separate harness-event vsock port + its
proxy; the JSON-on-vsock framing for harness events; ~one bespoke
streaming protocol.

**Open questions.** Backpressure semantics — the harness can produce
events faster than coord can drain them. Pick a bounded channel size
and document the spill policy.

---

### M7 — FC drive refs as content hashes, via `DriveResolver`

**Problem.** FC's `state.bin` embeds host filesystem paths verbatim
(rootfs path, harness path). We work around with the canonical-
symlink layer at `<work_dir>/rootfs/<sandbox_id>.dev`, which any
sibling host can rebuild on restore. The layer works but is brittle:
the relative-path-leaks-into-state.bin bug we fixed today (commit
95ad20a), the symlink-rewrite-on-warm-restore dance in
`swap_harness_drive`, and the entire "destroy must not touch the
canonical entries" contract are all evidence of the impedance
mismatch.

**Abstraction.** Wrap FC behind a small `DriveResolver` shim. State.
bin embeds a content-addressed `DriveRef` (the manifest hash). On
restore, the host's `DriveResolver` translates the hash to whatever
local path serves that content (NBD device, materialized file). The
canonical-symlink layer goes away; warm-restore stops needing a
symlink rewrite step; the relative-path bug class is structurally
impossible.

**What it deletes.** `paths::install_symlink` and its callers;
`canonical_parent_dirs`, `canonical_entries_for`, `assert_rootfs_canonical`;
the symlink rewrite in `swap_harness_drive`; the entire concept of
a "canonical path outside the jail".

**Open questions.** Does FC need to change to support this, or can
we wrap it? Most likely a wrapper around FC's APIs that lies about
paths (always tells FC to use a stable per-sandbox local path,
controls what's there via `DriveResolver`). Doesn't require upstream
FC changes.

---

### M8 — `enable_image` as a saga + blob-orphan GC

**Problem.** `enable_image` is a multi-step cascade
(materialize → verify → cascade) that's not atomic. Partial failure
can leave a `templates` row pointing at blob keys that aren't in
storage. The host's warm-pool refill loop then spams `blob_not_found`
WARNs forever. We hit this on the dev-vm today and dodged it by
adding `just integration-reset` (which nukes the DB volume).

**Abstraction.** `enable_image` is a saga with explicit rollback:
each step has a compensating action; partial failure unwinds
cleanly. Independently, a periodic GC runs over `templates` and
detects orphans — templates whose snapshot blobs are missing from
`BlobStorage` — and reaps them. Same shape as ADR 0011's existing
GC over `chunk_storage`.

**What it deletes.** The fragile multi-step `materialize_and_cascade`
in `enable_image.rs`; the operator-side cleanup that today is "drop
the DB volume" (i.e., `just integration-reset`); the per-stale-row
WARN noise.

**Open questions.** Saga library or hand-rolled? Hand-rolled — only
~5 steps; pulling in a saga crate would be over-engineering.

---

### M9 — Coord ↔ host as one bidirectional trait (or accept the split)

**Problem.** Per ADR 0013, host → coord is HTTP/JSON (heartbeat,
events, auth resolution) and coord → host is gRPC. The split was
made to keep coord pods stateless across restarts. Cost: lifecycle
is fragmented across five HTTP endpoints + a gRPC service; heartbeat
is a 5 s POST loop with no streaming; "host is going away" has no
clean signal.

**Abstraction.** A single bidirectional `HostChannel` trait (host
opens a long-lived stream; coord serves reverse RPCs on it).
Mirrors gRPC bidirectional streams' shape.

**Flagged.** This is a real tradeoff against the k8s LB ergonomics
ADR 0013 was designed for. We may end up Rejecting this in favor of
keeping the split and tightening the heartbeat protocol. The
decision needs prod usage data we don't yet have (how often do coord
pods restart in practice? does the 5 s heartbeat tick produce
meaningful overhead at fleet scale?). Until that's measured, this
section stays Proposed with low priority.

---

## Consequences

**Net code.** We expect the v2 direction overall to remove on the
order of 2-3 kLOC of bespoke scab-fix code (canonical-symlink layer,
bootstrap process + protocol, per-call-site retry loops, stale-
templates papering). New abstractions add code (the `GuestService`
trait, the state machine module, the `DriveResolver` shim) but each
is small. The shift is from breadth-of-bespoke to depth-of-trait.

**Tests.** Several test suites collapse (bootstrap's tests → agentd's;
the per-call-site retry tests → state machine transition tests; the
canonical-symlink tests → DriveResolver tests). New tests for the
trait boundaries.

**What gets harder.** Cross-version compatibility — once M1 lands,
mixed old/new host-agents and bakes have a brief window of
incompatibility. The atomic-cutover migration plan in each section
covers this. The bigger M5 (runtime snapshot) and M7 (drive refs)
moves require careful state.bin handling for in-flight snapshots —
likely a one-time migration script.

**What stays unchanged.** All the load-bearing existing abstractions:
`SandboxBackend`, `HostClient`, chunked storage, template_ref. The
v2 direction is additive to those, not replacing them.

## Phased rollout

- **Phase 1 (this PR):** M1 only. Validates the "one wire surface"
  pattern on the simplest payload (in-VM service collapse).
- **Phase 2 (next few PRs):** M2 + M8. Both contained, both fix
  real recurring bugs. M2 generalizes the readiness probe M1
  introduces; M8 eliminates the integration-reset crutch.
- **Phase 3 (medium-term):** M3 + M5. Both touch the warm-pool
  story (M3: who owns truth; M5: where warm snapshots come from).
  Ship together so the warm-pool semantics get a single coherent
  refactor.
- **Phase 4 (large investments):** M4 + M6 + M7. Each is its own ADR
  + multi-PR effort. Sequencing depends on which bugs surface first.
- **Phase 5 (decide later):** M9. Decide after we have prod
  observability on the channel-split cost.

Each phase ships its own implementation ADR. This document is the
v2 direction summary, not the implementation plan for any single
move beyond M1.
