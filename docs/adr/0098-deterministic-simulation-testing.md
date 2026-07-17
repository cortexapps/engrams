# ADR 0098: Deterministic simulation testing for the control plane

Status: 2026-07-17 — **Accepted.** Landed as the D1–D7 chain: #697 (D1
clock/entropy seam + clippy gate), #700 (D3 bind-param now()), #714 (D4
engram-sim + SimMetadataStore + the conformance suite + the dead_host
leasing-row preamble), #715 (the Phase-2 acked-write oracle, from the
#712 RCA), then the D5–D7 stack (#720 the simulator + first sims, #723
the full fault menu + driver inventory + oracle suite, and the D7 PR:
CLI + `just sim`/`sim-swarm` + regression seeds + the CI swarm lane).
ADR 0099's H1/H2 (#695/#696) supplied the per-test-database substrate
the conformance suite runs on.

**The thesis held before the harness was even finished.** The
conformance suite's FIRST run caught a latent scanner-breaking decode
bug (bare transition-to-Queued leaves NULL queue columns; guarded in
the follow-ups PR). Building D5 caught two SimMeta fidelity divergences
(registration writing heartbeat-only host columns; raw-vcpu CPU budgets
missing the overcommit factor) — each now pinned by a conformance case.
And D6's unconditional placement-accounting oracle found a REAL
production over-reservation hole (issue #722: the crash-orphan
exclusion vs the ADR 0079 pending-revival backstop), deterministically
reproducible from a seed; the fix (live-op pendings always reserve;
the backstop fails stale orphans instead of reviving them; boot
retries refresh freshness) landed in the D7 chain WITH conformance
coverage, and the oracle tightened back to the unconditional form.
The nightly long-run swarm landed post-acceptance
(`.github/workflows/nightly-sim.yml`: 1600 seeds × 5000 steps nightly
over a date-derived, never-repeating seed window).
Deviations still open, tracked:
Agent-mode workload (needs the harness-catalog surface in SimMeta);
capture-job reservations in SimMeta placement.
**Phase 2 (host-agent, the P-series) opened 2026-07-17** — the "future,
own ADR" sketch is superseded; the full design and P0–P9 chain live in
§"Phase 2: host-agent simulation" below.

Companion to ADR 0099 (correctness hardening & test isolation — the weeks-scale
arc; this ADR is the months-scale one). Builds on ADR 0011 (the
`HostClient`/`SandboxBackend` seam), ADR 0015 (the typed `SessionState`
machine), ADR 0047 (stateless coordinator — Postgres is the only authority),
and ADR 0079 (the session op log, Proposed — the simulator's first big
customer). Inspired by TigerBeetle's VOPR and FoundationDB's simulation
framework; the survey of which TigerBeetle practices we adopt, adapt, and
reject lives in ADR 0099.

## Context

We keep paying for one class of bug: timing/ordering races in the control
plane. They surface as flaky tests — the NBD teardown/migration races
(#582, #598, #629), the shared-Postgres cross-talk that forces the entire
coordinator PG test lane to run `--test-threads=1`, `sweep_stale_eviction_leases`
racing SQL `now()` against Rust `Utc::now()` (fixed in f97a1c2c) — and
occasionally as production incidents (the torn base-capture corruption, the
idle-evict snapshot-durability incident behind ADR 0028). Every instance gets a
bespoke deflake or regression commit *after* the fact. The pattern is reactive
by construction: real-time, real-Postgres, real-process tests can only observe
the interleavings the wall clock happens to produce.

Deterministic simulation testing (DST) inverts this. Run the whole control
plane single-threaded under a seeded scheduler; replace time, randomness,
Postgres, and hosts with deterministic fakes; inject crashes, partitions, and
clock skew from a seeded fault plan; check invariants after every step; and
replay any failure exactly from its seed. TigerBeetle runs decades of
simulated cluster-time per day this way; FoundationDB famously shipped for
years with bugs found almost exclusively by its simulator.

**The codebase is unusually well-positioned for this.** Three deliberate
architectural choices, made for other reasons, are exactly the seams DST
needs:

1. **The `Services` bundle** (`crates/engram-coordinator/src/lib.rs`) already
   routes 100% of the coordinator's external world through `Arc<dyn Trait>`
   objects: `MetadataStore`, `HostClient`, `CloudBackend`, `SecretStore`,
   `BlobStorage`, plus the OCI/crypto seams. Nothing in a handler or scanner
   reaches the outside except through these.
2. **Every background driver is a thin `spawn()` timer loop around a pure
   `run_once(&cfg, &state)`** (queue_scanner, reconcile, dead_host,
   idle_detector, idle_evictor, enable_scanner, evac_resumer, outbox_delivery,
   session_ops, preemption_drain, retention/GC). The live-PG tests already
   call these `run_once` fns directly — the deterministic step primitive
   exists; only the scheduling around it is wall-clock.
3. **The state machines are pure**: `SessionState::can_transition_to` is a
   const transition table (ADR 0015), as are `OpState` (ADR 0079) and
   `EnableJobState`. They run in a simulator verbatim.

What is missing:

- **No clock abstraction.** ~293 `Utc::now()` + ~57 `Instant::now()` calls sit
  inline in coordinator logic, and — worse — **126 SQL `now()` sites** in
  `engram-postgres` mean time decisions execute *inside Postgres* (lease TTL
  expiry, `shell_pinned_until > now()`, op backoff, heartbeat staleness).
  f97a1c2c was exactly a Rust-now-vs-SQL-now skew bug.
- **No seeded entropy.** `Uuid::new_v4()` is called inline (~95 coordinator
  sites); session/sandbox/op IDs are unreproducible.
- **No faithful in-memory `MetadataStore`.** Eleven per-test partial mocks
  (MiniMeta, FakeMeta, MockMetadataStore, ReconcileMeta, …) each stub the
  handful of methods one test needs.

## Decision

Build a DST harness for the coordinator control plane: two new crates, a
clock/entropy seam, a faithful in-memory metadata store kept honest by a
conformance suite, and a seeded single-threaded simulator with a fault menu
and an invariant suite, wired into CI as a swarm lane. Host-agent-internal
simulation is a sketched future phase, not this ADR.

### New crates

- **`crates/engram-sim`** — the deterministic substrate. Depends on
  `engram-core` (+ chrono, parking_lot, rand_chacha, tokio) and **not** on the
  coordinator, so coordinator unit tests can take it as a dev-dependency and
  retire the mock zoo. Contents: `SimClock`, `SimEntropy`,
  `SimMetadataStore` (module per table family), `SimHostClient` +
  `SimHostWorld`, `SimCloud` (extending `engram-cloud-mock` if it fits), and
  `tests/meta_conformance.rs`.
- **`crates/engram-dst`** — the simulator. Depends on `engram-sim` and
  `engram-coordinator` (lib) + tower. Contents: `SimWorld`, the seeded step
  scheduler, the workload generator, the fault menu, the invariant suite,
  trace/replay, `src/bin/sim.rs`, and `tests/regression_seeds.rs`.

### The Clock / Entropy seam (D1)

New traits in `crates/engram-core/src/traits/clock.rs`:

```rust
pub trait Clock: Send + Sync + Debug {
    fn now_utc(&self) -> DateTime<Utc>;
    fn now_mono(&self) -> Duration;   // replaces Instant::now()/elapsed pairs
    fn sleep(&self, d: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}
pub trait Entropy: Send + Sync + Debug {
    fn uuid(&self) -> Uuid;                    // replaces Uuid::new_v4
    fn u64(&self, range: Range<u64>) -> u64;   // seeded backoff jitter
}
```

`SystemClock`/`OsEntropy` prod impls live next to the traits. Both hang on
`Services` as `clock`/`entropy` fields — time and randomness *are* inputs from
the world, and every `run_once` already receives `&SharedState`, so no
signature churn. `now_mono()` returns a `Duration` since a per-process epoch
rather than an `Instant`, because a fake cannot mint `Instant`s.

**Enforcement, so the sweep never regresses**: a root `clippy.toml` with
`disallowed-methods` for `chrono::Utc::now`, `std::time::Instant::now`,
`tokio::time::Instant::now`, and `uuid::Uuid::new_v4`. `just check` runs
clippy with `-D warnings`, so a new raw call is a hard failure. The only
`#[allow(clippy::disallowed_methods)]` sites are the `SystemClock`/`OsEntropy`
impls themselves (the one honest caller — consistent with the never-launder-
lints rule) plus an explicitly-listed metrics helper if profiling shows one is
needed. The gate initially covers coordinator + core + postgres + sim + dst;
host-agent joins in Phase 2 P1 (per-crate clippy.toml, decision-feeding
scope + the `metrics_now()` carve-out — see §Phase 2).

`SimClock` keeps **one time source, not two**: it is a thin view over tokio's
paused clock (`base_utc + skew + (paused Instant − birth)`). The simulator
runs on a current-thread runtime with `start_paused = true` and advances time
only via explicit `tokio::time::advance(d)` steps, so `clock.sleep()` is
plain `tokio::time::sleep` and sleeps, timeouts, and `now_utc` move in
lockstep. Per-replica clock skew is a mutable offset on each replica's
SimClock, perturbed by a fault action.

### Bind-parameter `now()` in PostgresStore (D3)

Independently of the simulator, `PostgresStore` stops calling SQL `now()`:
every one of the 126 sites takes `now: DateTime<Utc>` as a bind parameter
sourced from `services.clock` (and inserts pass explicit timestamps instead of
relying on `DEFAULT now()`). This removes the SQL-now-vs-Rust-now skew class
outright, makes live-PG tests time-controllable, and gives the conformance
suite (below) a shared time model across both store implementations. Requires
a `.sqlx` query-cache regeneration (`SQLX_OFFLINE` gotcha).

We considered driving the *simulator* against real Postgres with bind-param
time and rejected it: ~0.5–1 ms per statement × 10⁵–10⁶ statements per seed ×
hundreds of seeds is orders of magnitude too slow for a swarm; SKIP LOCKED
winner selection and equal-key ordering are decided inside PG and are not
seed-reproducible; and it reintroduces a shared stateful service into a lane
whose point is hermetic parallelism.

### SimMetadataStore + the conformance suite (D4)

One `parking_lot::Mutex<SimDb>`; every trait method locks, mutates
synchronously, unlocks — the async signatures never actually suspend. All
collections are `BTreeMap`/`BTreeSet` (deterministic iteration; never
HashMap). A global `serial: u64` mimics BIGSERIAL for tie-breaks.

**The fidelity contract that makes this tractable**: each `MetadataStore`
method corresponds to one SQL statement or one transaction in `PostgresStore`
— the trait was designed so no app logic sits between round trips of a single
method. Per-method mutex atomicity therefore reproduces PG's per-statement/
per-txn atomicity exactly, and the nondeterminism PG exhibits (*which*
replica's `FOR UPDATE SKIP LOCKED` claim wins, ON CONFLICT race winners) is
reproduced by the **scheduler's interleaving of method calls**, not modeled
inside the store:

- `INSERT … ON CONFLICT DO NOTHING` leasing row → `BTreeSet::insert`
  returning bool.
- `FOR UPDATE SKIP LOCKED` head-claim → atomically pick the smallest due row
  not already claimed; a "concurrent" peer's call runs before or after under
  the chosen step order and gets a disjoint row or nothing.
- Partial unique indexes (one-running-op, idempotency keys) → explicit checks
  returning the conflict error.
- Every SQL `now()` predicate → `self.clock.now_utc()`.
- Fencing CAS (`current_epoch`, `binding_epoch`) → compare-and-set, and
  SimMeta **internally asserts** no stale-epoch write ever lands — a free
  invariant checker at the exact enforcement point.

Coverage: implement the ~19 required methods plus everything the driven
surface touches (realistically 60–90 of the 193). Every *other* method is
overridden with `panic!("SimMeta: <method> not implemented — add it plus a
conformance case")` rather than inheriting trait defaults, so a newly-driven
code path fails loudly instead of silently observing a defaulted empty.

**The conformance suite** (`engram-sim/tests/meta_conformance.rs`) is the
defense against fidelity drift: each case is a generic scenario executed
against BOTH `SimMetadataStore` and `PostgresStore` (fresh template-cloned
database per test, via the `engram-testkit` helper from ADR 0099 H1),
asserting identical observable outcomes. **Process rule: any PR that adds a
`MetadataStore` method or changes PostgresStore SQL semantics must extend the
conformance suite in the same PR.**

Preamble work in the same PR: `dead_host.rs` is today the one driver holding a
raw `PgPool` for `pg_try_advisory_lock` — convert it to the repo's own
documented pattern (a PG leasing row, per AGENTS.md), retiring the raw-pool
parameter and making it fully trait-driven and simulable.

**NOTIFY/LISTEN position**: the simulator runs on polling alone. NOTIFY is a
latency optimization; correctness must hold without it — running the sim
notify-blind is itself a valuable property check. SimMeta records would-be
notifies so a later phase can model wake delivery (with drops and delays) as
scheduler actions against the drivers' `Notify` wake handles.

### The simulator core (D5)

**A hand-rolled seeded step scheduler on a current-thread tokio runtime with
paused time.** We evaluated and rejected the two off-the-shelf options:

- *turmoil* simulates the socket layer between nodes. Our inter-node IO is
  already entirely behind trait objects; the sim never opens a socket (API
  calls go through `tower::ServiceExt::oneshot` against the real axum
  `Router`). Turmoil would add a runtime wrapper and buy nothing at our seam.
- *madsim* achieves determinism via global `--cfg madsim` rebuilds with
  patched tokio/sqlx across the dependency graph — maximally intrusive,
  fights sqlx, and its guarantees mostly cover primitives we bypass by
  driving `run_once` directly.

TigerBeetle's answer (no async runtime at all; explicit ticks) is the north
star, and we get ~95% of it for free: every driver is already a `run_once`,
and SimMeta never suspends, so an executor that polls **one logical action's
future to completion before picking the next** leaves tokio's scheduler
almost no freedom.

```rust
enum Step {
    AdvanceTime(Duration),           // tokio::time::advance — fires due sleeps
    Driver(ReplicaIdx, DriverKind),  // call the real run_once on that replica
    Api(WorkloadOp),                 // tower-oneshot the replica's real Router
    HostTick(HostIdx),               // heartbeat / host-side lifecycle progress
    Fault(Fault),
}
```

Entropy: `ChaCha8Rng` (portable, seed-stable), with the master seed forked
into per-component streams (scheduler picks, workload content, `SimEntropy`
uuids, fault timing) so adding a consumer doesn't shift every other stream —
the FoundationDB replay-stability trick.

`DriverKind` enumerates the real `run_once` fns. A meta-test asserts
`DriverKind` covers every coordinator module exposing a `run_once`, so a new
driver cannot be silently unsimulated.

**Determinism audit (checklist in the D5 PR)**, stated honestly:

1. Detached `tokio::spawn` inside driven paths (e.g. session_ops'
   `OpClaimHeartbeat`): on a current-thread runtime these run on the same
   thread at await points we control, and the heartbeat writes are
   idempotent — audited, and anything non-idempotent gets refactored.
2. `tokio::select!` randomizes branch order from a global RNG: the spawn
   loops that use it are bypassed; any `select!` in driven code becomes
   `biased;` or is refactored.
3. HashMap/DashMap *iteration* feeding decisions is a determinism leak —
   convert to BTreeMap where order matters; keyed lookups are fine.
4. Current-thread tokio's ready queue is FIFO and practically deterministic
   for a fixed tokio version, but not documented: **a seed replays on the
   same commit** (the lockfile pins tokio); cross-version replay stability is
   not promised.
5. CI runs each PR seed **twice** and diffs the traces — determinism leaks
   are caught the day they land, not when a failing seed won't replay.

### The world model, faults, and invariants (D5–D6)

`SimWorld`: one shared `SimMetadataStore` (== PG, the single authority),
N coordinator replicas (default 2 — each a real `SharedState` + axum Router;
**replica crash = drop the `SharedState`, restart = rebuild over the same
SimMeta**, directly testing ADR 0047's statelessness claim), and M `SimHost`s
(default 3) behind `SimHostClient`, which returns the same typed retryable
`SandboxError`s the real transport surfaces and models RPC delay/reorder via
an effect queue applied at later steps.

Fault menu, weighted per profile (`calm` / `chaos` / `pg-flaky` /
`partition-heavy`): replica crash/restart · host crash/restart · asymmetric
heartbeat partition (heartbeats lost, RPC fine — and vice versa) · RPC
partition/delay/reorder/duplicate · PG outage window (SimMeta flag → the
`MetaError::Db` the drivers already classify) · per-replica clock-skew step ·
preemption via SimCloud · workload burst.

Workload generator: weighted session create (varied resource classes so queue
partitioning interacts) / prompt / idle-age / evict / resume / destroy /
enable-image — through the real Router wherever a route exists.

Invariants, checked after **every step** (SimDb is in-memory; a sweep is µs):

1. **Transition legality** — every observed state change satisfies
   `can_transition_to` (via a SimMeta write log; defense-in-depth over the
   existing enforcement, and it checks SimMeta itself).
2. **Single ownership / fencing** — at most one live sandbox per session
   across all hosts; `sandbox_owners` agrees with world truth (ADR 0090);
   epochs strictly monotonic.
3. **At most one running op per session** (ADR 0079's structural invariant,
   re-derived from world state).
4. **Placement accounting** — Σ reservations per host ≤ capacity (ADR 0046).
5. **Snapshot safety** — no action destroys the last recoverable copy of a
   non-terminal session (ADR 0028).

And at **quiescence** (faults off, then all drivers ticked round-robin K
rounds with generous time advances):

6. **No session lost or leaked** — every session terminal or stable-serving;
   the queue drains given capacity; no `HostLost`/`Evacuating`/`Pending`
   stragglers.
7. **No op dropped** — every enqueued op reaches done/failed/cancelled.
8. **No orphan sandboxes** — every world-side live sandbox maps to a session
   that claims it.
9. **Convergence bound** — quiescence within K driver rounds (catches
   livelock, e.g. detector-vs-evictor ping-pong).

### Seed replay + CI (D7)

Failure artifact: seed, git SHA, profile, step count, violated invariant, and
the last ~200 trace events (step index, action, virtual time, key state
deltas). Replay = same binary + `--seed`. Justfile: `just sim SEED=
[STEPS=] [PROFILE=]` and `just sim-swarm [SEEDS=200]`.

CI: a new `test-sim` job — PRs run a fixed seed range (e.g. 0..200 × 3k
steps across profiles, target < 5 min; fixed seeds so a CI failure is always
locally reproducible), plus a nightly scheduled long run (~45 min, seed
offset logged in the job summary). Detector: a `sim` flag in
`detect-rebake-lanes.py` keyed on engram-dst's cargo dep closure (which
transitively includes coordinator/core/postgres — exactly the right trigger
set). **`test-sim` is added to `CI Gate`'s `needs:` and is never an
individually-required check** (the path-gating rule in AGENTS.md). Every CI
failure that gets fixed pins its seed in
`engram-dst/tests/regression_seeds.rs` with a comment naming the bug.

## Phasing

| PR | Content | Size |
|----|---------|------|
| D1 | This ADR flips in + `Clock`/`Entropy` traits + `Services` fields + clippy.toml gate + coordinator sweep (~445 sites) | XL, mechanical |
| D3 | Bind-param `now()` in PostgresStore (126 sites, `.sqlx` regen) | L |
| D4 | `engram-sim`: SimClock/SimEntropy/SimMetadataStore + conformance suite + dead_host leasing-row conversion + retire 2–3 test mocks as proof | XL — the hard one |
| D5 | `engram-dst`: SimWorld + scheduler + SimHostClient + first end-to-end sim (queue_scanner + reconcile + dead_host + session_ops; host-crash fault) + determinism audit | L |
| D6 | Full fault menu + invariant suite + driver/workload breadth + quiescence checker | L |
| D7 | CI lane + just targets + regression-seed scaffold; flip this ADR **Accepted** with the commit chain | S |

(D2 in the working sequence is ADR 0099's H1/H2 — per-test PG isolation —
which D4's conformance suite consumes.) D1 and D3 are order-independent and
should land early: they are the largest rebase surfaces. Sequencing against
ADR 0079: hammer the pre-0079 lease ecosystem first and capture its bugs as
regression seeds, then verify the op-log kernel retires them — an unusually
strong migration-safety story for both ADRs.

## Phase 2: host-agent simulation (the P-series)

**Status 2026-07-17: active.** The original sketch here said "future, own
ADR" — superseded: Phase 2 extends THIS ADR in place as the P-series,
mirroring the D-series (decision recorded the day the D-chain merged; the
incidents this phase targets — 85e0298a acked-write loss, torn
base-capture, the NBD teardown flake family, the teardown mis-reap — all
lived in host-agent paths the coordinator sim cannot see).

**Scope**: the host-agent's six multi-step lifecycle flows, extracted into
pure typed state machines (the `SessionState` pattern) with side effects
behind focused trait seams, plus a host-internal simulator. Real-kernel
behavior (netlink, actual NBD devices, KVM, the block data plane, page
cache — incl. BLKFLSBUF/cross-tenant invalidation) stays in the
Firecracker CI lane forever; the sim owns ordering and crash logic, and
runs on macOS.

**Seams — `HostEffects` is a bundle STRUCT, not a mega-trait** (the
`Services` analog; a god trait would recreate the mock-zoo D4 retired):
`clock`/`entropy` (existing engram-core traits) + four new focused traits —
`CoordControlPlane` (the 3 coordinator calls the flows make:
`sandbox_ownership`, `sandbox_owner`, `publish_live_manifest`; the concrete
reqwest `CoordClient` renames to `HttpCoordClient` and implements it),
`HostFs` (granular write/sync_file/rename/sync_dir — the SimFs boundary
under `durable_record` + the spool), `DeviceSync` (`/dev/nbdN` sync +
abandon-in-place), `NbdKernel` (connect/reconfigure/disconnect/
backend-identifier; Linux impl wraps `disk_daemon::runtime`). Trait defs +
extracted state machines live in a new portable crate
**`engram-host-core`** (deps: engram-core only — this is what makes the
logic macOS-runnable); prod impls stay in `engram-host-agent`. Traits sit
only at orchestration boundaries (30 s reconcile tick, once-per-death
shutdown, backoff-paced redrive, attach/reattach) — the NBD serve loop,
`read_chunk`/`write_chunk`, the migrate_peer page server, and the flush
lock ladder stay concrete: **the flush pipeline is deliberately NOT
extracted** (data plane; wrapping the #199/#204 lock ladder would add
hot-path cost). Its `#[cfg(test)]` `flush_local_handoff_seam` generalizes
instead into a 3-point `SchedulerSeam` (dirty→pending handoff,
post-upload/pre-publish, pre-rebase) giving the sim deterministic control
of both documented flush hazards at zero prod cost.

**Clock/entropy sweep**: decision-feeding sites only (~57 `Utc::now`, ~23
`Uuid::new_v4`, the budget/TTL/backoff `Instant::now`s and driven sleeps) —
NOT the data-plane latency metric timers, which go through one documented
`metrics_now()` helper carrying the single scoped `#[allow]` (a `Box::pin`
per page-serve op would trade latency for nothing: the data plane is never
simulated). Enforcement joins the shipped per-crate pattern:
`clippy.toml` for engram-host-agent + engram-host-core (the D1 "root
clippy.toml" wording was aspirational; per-crate is what shipped).

**The simulator — new crate `engram-dst-host`**, sibling of `engram-dst`,
NOT an enrichment of it: deps engram-sim (SimClock/SimEntropy, the
paused-tokio + ChaCha8-forked-streams + BTreeMap discipline) +
engram-host-agent + engram-host-core; **no engram-coordinator dep** — the
two sims' dep closures (and their `detect-rebake-lanes.py` triggers) stay
disjoint, and coordinator×host-internal state-space product is avoided.
The coordinator here is a deliberately ADVERSARIAL scripted stub
(`SimCoordClient`: ownership flips mid-export, lost publish acks,
vanishing coordinator) — the real handlers would be cooperative and drag
the wrong closure in. Sim impls of the effect traits live inside this
crate.

**World model**: `SimHost` = RAM state (dirty/pending tiers, scheduler
timers, warm pool, export registry, bindings — dies on `CrashProcess`) +
disk state (a real per-run tempdir behind `SimFs`: records/finalize/spool/
L1 cache, plus a shared `LocalBlobStorage` chunk store standing in for
GCS — survives). Restart rebuilds over the same tempdir through the REAL
recovery entry points (`reattach_pass`, `reattach_manifest` +
`adopt_unflushed`, `resume_pending_finalizes`, the stale-binding sweep,
`load_all`). Guest writes are `(sandbox, chunk_idx, content_tag)` through
the real `write_chunk`, appended to an acked-write ledger at the ack
instant; reads resolve the tier ladder and return the tag.

**SimFs + the H5 composition contract**: intra-op byte-level torn states
remain ADR 0099 H5's exhaustively-constructed static tests (SimFs does not
re-derive them); H5 owns exhaustiveness at byte granularity. What SimFs
owns is reachability at operation granularity — crashes injected *between*
the real durable operations. **P5 wires `durable_record` and the spool
through the injected `HostFs` seam so the interception is real** (an
earlier draft here proposed leaving those helpers on raw `tokio::fs` with
a crashpoint meta-test mapping onto the H5 states; that only proves the
model is self-consistent, not that it mirrors the production call
sequence — dropped). Consequences P5 must honor:
- `HostFs` MUST include `create_dir`/`remove_dir` (not just
  write/sync_file/rename/sync_dir/read), because spool *replacement*
  begins `remove_dir_all` then `create_dir_all` — a destructive crash
  window the write/rename ops don't cover.
- The crash-point list is **derived from the production op sequence** of
  `persist()` and `write_spool()` (each real fs call is a boundary), not a
  separately-enumerated parallel list a `crashpoint_coverage` test checks
  against itself.

**Phase 2 invariant #1 — the acked-write durability oracle** (added
2026-07-16 from the session-85e0298a corruption RCA, PR #712). The precise
guarantee — sharpened after an adversarial review, then again by the P4.5
implementation: *every guest-acked write whose tag sits at or below the
**published-manifest floor** — the highest tag a flush actually published
(read back from the real manifest, so the store-ahead case is covered) — is
recoverable after restart, read back through the REAL recovery path
(`from_blob(published_ref)` + spool `adopt_unflushed`), not by checking a
raw blob exists.* A legitimate post-restart read of a chunk is any tag in
`[published_floor, latest_ack]`; the violation is a read **below** the
floor (a published write rolled back — the 85e0298a class) or **above** the
latest ack (impossible / corruption). Two boundaries this makes explicit:
- **The shutdown spool is a TRANSIENT carry, not a durability floor.**
  It prevents loss across one orderly roll (the #712 fix, the common
  case), but the successor's `adopt_unflushed` seeds the spooled chunk
  back into the *volatile* dirty tier and discards the spool — so the
  write is once again only as durable as the next flush. A *later* abrupt
  crash of the successor legitimately loses it. Only a flush→publish
  raises the floor; a spool does not. (P4.5's first cut keyed the floor on
  the spool and produced false violations on
  `SpoolExport→adopt→AbruptCrash→Restart` — diagnosed as honest loss, the
  fix in the oracle not the product.)
- **A write acked from the RAM dirty tier and lost to abrupt process
  death BEFORE it is flush-published is an accepted, bounded loss** —
  bounded by the flush cadence and the periodic checkpoint (ADR 0028), NOT
  a violation. The oracle does not claim "no acked write is ever lost by
  any process death"; it verifies the *durability pipeline*
  (flush→publish, with the spool as a best-effort roll-boundary carry)
  never loses a write below the published floor. The `AbruptCrash` step
  (drop RAM, no spool — the window every spool-first crash primitive
  skipped) plus a mandatory `post_ack_pre_handoff_crash_is_honest_loss`
  seed pin both directions: an un-published write is lost with no
  violation; a flush-published write survives abrupt death.
- **Recoverability means reachability through a durable/uploaded MANIFEST
  or the spool** — both carry the chunk's disk offset. A content-addressed
  chunk blob PUT with no manifest referencing it is NOT recoverable (no
  production path can place it); the store-ahead case (85e0298a: manifest
  uploaded before the coord publish landed) recovers via the spool's ref
  naming that manifest, which is why the read-back goes through the real
  attach, not a blob-existence check.

The RCA found a class of guest-silent acked-write loss with three
mechanisms in one day; two were kernel page-cache behavior (permanently
the FC lane's job — see non-goals), but the third — the SIGTERM
flush-deadline overrun dropping the RAM dirty tier — was a pure
shutdown-pipeline ordering bug, exactly what this phase's seeded
crash-point injection explores systematically. The world model's ledger
tracks per-write durable-handoff state and asserts the oracle after every
injected crash/restart, so the *class* is covered rather than one bespoke
regression per incident. The full oracle suite: (1) acked-write
durability; (2) no-plane-leak (every claimed slot serving or
parked/quarantined; no dirty tier abandoned without a spool export); (3)
slot accounting (warm+in_use+parked+free == universe, no double-claim);
(4) spool never silently dropped while holding the only copy of an acked
write; (5) single-device ownership; (6) `FinalizeStage` monotonicity +
resume-at-persisted-stage; (7) the migration decision table
(`state_served` ⇒ never abort-unpause — the #216 split-brain guard); (8)
convergence at quiescence for every redrive loop (the "checkpoint driver
retries forever against a dead data plane" follow-up reads as
non-convergence); (9) the reconcile session=None arm stays fixed (the
2026-07-11 mis-reap — already fixed in code; the oracle pins it).

**Known bugs ride the arc** (user decision): O_DIRECT for the NBD device
sync was evaluated in P4 and is a **NO-OP** — verdict recorded in the P4
row and `engram_host_agent::device_sync`: all opens of `/dev/nbdN` share
one bdev page cache, so the existing buffered `sync_all` (`fsync`) already
writes back FC's buffered-dirtied pages; O_DIRECT only governs `read`/
`write` on the fd (this fd is sync-only) and never changes `fsync`
semantics, so it would add block-alignment fragility for zero benefit. The
sync path is unchanged (now routed through the `DeviceSync` seam); the
read-side residual (a successor reading stale/dropped pages) is a different
mechanism — cross-tenant slot reuse is already closed by the `BLKFLSBUF`
invalidate-on-CONNECT, and verify-on-read (post-RECONFIGURE read observes
the seeded acked bytes) is the documented fallback, riding P7. The None-arm
mis-reap needs only its oracle (P3). Regression seeds double as the migration-safety
proof: each historical hazard (#224 insert-after-sweep, #225
deadline-overrun, #204 tier-less window, #199 flush reorder, #216 family,
the slot validation-window race, the stale-binding TOCTOU, 85e0298a
store-ahead recovery, the None-arm, checkpoint tail-cancellation) is
reproduced where possible against pre-extraction logic, then pinned as
proof the extraction retires it.

**The P-series** (labels are stable; **execution order** was revised
2026-07-17 after P4: **P7 (Flow B — NBD slot/reattach) is pulled FORWARD to
run immediately after P4**, and P5 (eviction finalize) + P6 (flush
SchedulerSeam) slide back to after it — two device-lifecycle ordering
incidents in two days, 85e0298a and 731df805/#739, make Flow B the
highest-impact remaining flow; P8/P9 unchanged):

| PR | Content | Size |
|---|---|---|
| P0 | This section (bookend open) | S |
| P1 | engram-host-core; the four traits + HostEffects bundle; HttpCoordClient rename; decision-feeding clock/entropy sweep + `metrics_now()`; per-crate clippy gates. Zero behavior change; full FC lane runs on it despite the mechanical label | XL |
| P2 | engram-dst-host scaffold: SimFs + SimHost RAM/disk split + acked-write ledger + coord stub + SimNbd + scheduler skeleton + determinism audit + flow_coverage & crashpoint_coverage meta-tests | L |
| P3 | Flow C (reconcile): `reconcile_once` extraction (pure `classify` + `ReconcileBackend` seam); the lib.rs inline loop collapses to a thin spawn wrapper; ReconcileTick + None-arm oracle (#9) + stale-binding TOCTOU scenarios. **#224 abandoning moved to P4** — its insert-after-sweep gate lives in `abandon_nbd_data_planes_for_shutdown` (the SIGTERM path, not extracted until Flow A), so it is not cleanly drivable on today's portable surface | M |
| P4 | **Landed.** Flow A (SIGTERM ladder): the pure decisions — `ShutdownStage` (+ `admits_new_plane`), `flush_budget`/`plan_shutdown`, `FlushProbe`/`classify_survivor`, `is_straggler` — extracted into **`engram-host-core::shutdown`** (cleanly-typed `std` types → the portable crate, not host-agent-local; contrast Flow C's PooledBackend-coupled `classify`). The driver (`lib.rs` shutdown handler + `flush_nbd_data_planes_for_shutdown` + the overrun sweep) becomes a thin executor off those verdicts; the `abandoning` SeqCst flag + drain-twice + the per-survivor `tokio::spawn`/`timeout` + `abandon_for_shutdown`'s ownership-consuming semantics stay byte-identical (concurrency gates, not decisions). The `spawn_blocking` device sync is now wired through the P1 `DeviceSync` seam (`HostDeviceSync` prod impl). Sim: the `Sigterm(budget)` step drives the real ladder (seeded budgets small→large; a tiny budget overruns → stragglers → spool), and `CrashAt(CrashPoint)` seeds crash-point injection over all eight H5 boundaries (spool boundaries → real recovery under the acked-write oracle; persist boundaries → reachability + `load_all` tolerance, folding into the ledger in P5's Flow D). Regression seeds: #225 deadline-overrun, 85e0298a store-ahead (world-model `rebuild` now honors the spool's store-ahead ref), #224 insert-after-sweep (the extracted ordering contract; the literal DashMap race stays FC-lane residue). **The acked-write oracle stays unconditional.** O_DIRECT rider: evaluated → **no-op** (see the known-bugs note + `device_sync`), so the sync path is unchanged and no FC regression was warranted; verify-on-read stays P7. | L |
| P4.5 | **Oracle honesty** (from the 2026-07-17 adversarial review): the acked-write ledger tracks per-write durable-handoff state (flush-published or spooled); the oracle requires recoverability only for handed-off writes and reads back through the real recovery path; a mandatory seed crashes post-ACK / pre-handoff and asserts honest loss (rolled-back manifest, not false recovery). Sharpens the core oracle all later Ps depend on; ADR §invariant-#1 already reworded. | S |
| P5 | Flow D (eviction finalize): route through HostEffects; EvictionFinalizeLeg + crash-between-legs; FinalizeStage + convergence oracles; checkpoint tail-cancellation. **Also lands the real `HostFs` interception** (durable_record + spool wired through the seam, incl. `create_dir`/`remove_dir` for the destructive spool-replacement window; crash points derived from the production op sequence — see §SimFs) | M |
| P6 | Flow F seam: SchedulerSeam generalization; #204/#199 as seeded interleavings | M |
| P7 | **Landed.** Flow B (NBD slot/reattach). **Extraction:** the `NbdKernel` seam (P1's unwired trait) is rewired — `HostNbdKernel` (the Linux prod impl over `nbd_netlink` + sysfs) is bound at the public attach/reattach entry points and the CONNECT/RECONFIGURE/backend-identifier touches funnel through `&dyn NbdKernel` at `serve_at` (public signatures unchanged; no blind FC-test churn). The decision content is pure in **`engram-host-core::reattach`**: `plan_reattach` (backend-id echo-else-fallback + the seed-dirty-BEFORE-RECONFIGURE ordering as an explicit `ReattachPlan`/`ReattachStep` property), `sweep_verdict`/`PidLiveness` (the stale-binding "dead-owner-only" core, wired into `recover_one_stuck_device`), `is_local_survivor_candidate` (the #739 filter core, wired into `local_survivor_candidates`), and `resume_data_plane_served` (the un-pause gate). `SlotState` (Free/Warm/Claimed/Parked) + a transition table make the allocator's implicit FSM auditable next to the (unchurned, portable) allocator. **Riders:** verify-on-read — post-RECONFIGURE, when a spool was adopted, a single-chunk probe (`first_seeded_probe`/`probe_matches`) proves the device serves the seeded acked bytes (not rolled-back base) or returns the slot for park; a new minimal `#[ignore]`'d FC lane test (`nbd_verify_on_read`, wired into `ci.yml`) proves it at the O_DIRECT device plane. Un-pause data-plane gate — `PooledBackend::resume` fails fast into `evict_local → resume` (a `soft_invariant!`, ADR 0099 H6 site #7) when the rootfs device isn't served by the current generation. **Sim (`engram-dst-host`):** the device-serving model (generation + `served_by`/`kernel_owner`/`parked` + the real `NbdSlotAllocator`) with steps `Park`/`Unpause`/`RegisterRehydrate`/`StaleSweepTick`/`SlotClaim`/`SlotPopulateTick`; oracles #3 slot-accounting (`free + warm + held == capacity`, no double-claim) and #5 single-device-ownership + *served-device-never-dead* (the 731df805 property) as standing invariants; the local-rehydrate leg adds a recovery arm to oracle #1's closure. **The 731df805 scenario is pinned** (`park → roll → register → sweep → un-pause`): the FIXED variant (buggy coord list omits the parked survivor, the #739 local ChainHeadRecord pass re-serves it, the sweep skips it, un-pause serves) and the UNGATED variant (local pass off → the sweep disconnects the live device → the un-pause gate is the last line and fires, no dead-plane serve). The tight concurrent claim-vs-populate validation-window stays the multi-thread host-agent test (paused single-thread tokio can't hold a `claim` mid-populate, and `claim`'s retry `sleep` hangs on the paused clock — the sim drives the transitions as explicit steps). | L |
| P8 | Flow E (migration): TTL clock → `now_mono`; #216 decision-table oracle; no-plane-leak/single-device tightening; #582/#598/#629 seeds | M |
| P9 | CI: `test-host-sim` lane (fixed seeds, <5 min, own rust-cache key, replay-twice self-check) in `CI Gate.needs:` + a host-sim detector flag keyed on engram-dst-host's dep closure; nightly job with `--failure-report` issue auto-filing (the shipped nightly-sim pattern); regression_seeds populated; this section closed with the commit chain | S |

### Coverage gaps surfaced by real incidents (tracked follow-ups)

A gut-check against the 2026-07-17 incident on session `03e6535e` (PR #743 —
two independent compounding failures: a 40-minute resume-op stall, and NBD
disk corruption → SIGBUS) found both failures are squarely the *classes*
this program targets, yet neither is catchable by the harness as it stands.
Both are the honest boundary of "would DST have cut this," recorded so the
answer is a plan, not a hope:

- **G1 — the op-stall belongs to the coordinator sim (`engram-dst`) and is
  one fault-knob away.** A resume op wedged forever inside `start_agent`
  (no gRPC/host deadline) while the within-step heartbeat kept the row
  fresh, so stale-op reclaim never fired — pinned 40 min until a pod roll.
  The `no_op_dropped` / quiescence-convergence oracle is the exact catch
  (an op that never reaches done/failed fails quiescence), but it can't
  fire today: `SimHostClient` (`crates/engram-dst/src/world.rs`) models
  only fail-fast RPC *partition* (returns `Unavailable`), never the
  *hang/delay* the fault menu specced (§"World model + faults", "RPC
  partition/delay/reorder"). **To close G1:** a hang/delay knob on
  `SimHostState` + a `Step`/fault to toggle it (cleared in the
  quiescence-heal block) + a regression seed asserting a wedged op still
  reaches done/failed via the new `op_deadline`. #743's fix (`op_deadline`
  on the tokio/paused clock) was *deliberately built* to be fired
  deterministically by the sim — the seam is ready; the fault isn't.
- **G2 — the disk-corruption belongs to the host sim (`engram-dst-host`)
  and is a recurring class.** Two silent-fallback paths keyed disk
  durability on `nbd_sandboxes.get(id)` and skipped for a post-roll
  survivor never rehydrated: capture recorded a `recoverable` snapshot
  with `disk_manifest=None` (dropping acked writes), and resume then booted
  onto the stale literal `/dev/nbdN`. The **acked-write durability oracle**
  is precisely the catch (a lost published-floor write / a boot onto the
  wrong device is a below-floor read), but the host world model drives
  `ChunkedDiskBackend` directly and does not yet model `PooledBackend`'s
  `nbd_sandboxes` tracking, the capture/snapshot orchestration, or the
  resume NBD-attach decision — those flows are extracted only past P7.
  **The recurring shape:** this is the *second* incident in two days
  (after 731df805/#739, register/sweep) whose mechanism is identical —
  **a lookup keyed on a tracking map that a post-roll survivor isn't in,
  causing a silent skip that corrupts.** #739 is register/sweep; #743 is
  capture/resume. **To close G2:** generalize P7's 731df805 scenario into a
  **survivor-invisibility family** spanning register, sweep, *capture*, and
  *resume*, each asserted by the acked-write oracle — landing as the
  capture (P5-adjacent) and resume-attach flows are extracted behind the
  host-core seam.

**The program already shapes fixes ahead of catching bugs:** #743 followed
the D4 conformance rule (implemented `SimMetadataStore::rewind_session_to_cursor`
+ a dual-store conformance case for its PostgresStore SQL change) and made
`op_deadline` paused-clock-deterministic — so even where the harness cannot
yet reproduce an incident, its discipline is already load-bearing on how the
incident's fix is written and tested. G1 and G2 are the next increments that
turn that discipline into detection.

## Non-goals

- No packet/socket-level network simulation — the trait seam is the boundary.
- No simulation of guest/harness behavior, the kernel, the NBD data plane, or
  FC/VZ internals — only their control-plane contracts.
- No performance or latency modeling — correctness and liveness only.
- No coverage of the web or orchestrator tiers.
- Environmental flakes (KVM variance, fake-gcs stalls, runner contention)
  are explicitly out of scope; they need runner-level mitigation.
- Cross-tokio-version seed replay stability is not promised; a seed pins to a
  commit.

## Risks

1. **SimMeta fidelity drift** — a stale fake that green-lights bugs is worse
   than no simulator. Mitigations: the conformance suite runs against both
   impls in CI; panic-not-default for unimplemented methods; the same-PR
   conformance rule; bind-param-now unifying the time model. Residual:
   conformance cases only cover what someone thought to write — accepted and
   budgeted.
2. **Determinism leaks** making seeds non-replayable — the most demoralizing
   DST failure mode. Mitigations: run-step-to-completion scheduling, the D5
   audit checklist, BTreeMap-everywhere in sim code, and the replay-twice-
   and-diff CI self-check.
3. **Sweep churn and maintenance tax** — D1/D3 touch ~570 call sites while
   feature work continues, and every new driver/handler must join
   `DriverKind`/the workload or it is silently unsimulated. Mitigations: land
   the mechanical PRs fast and early; the clippy gate makes post-sweep
   regressions impossible; the DriverKind coverage meta-test.
