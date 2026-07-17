# ADR 0098: Deterministic simulation testing for the control plane

Status: 2026-07-16 — **Proposed.**

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
host-agent joins when its phase starts.

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

**Phase 2 (future, own ADR)**: the host-agent's NBD/teardown/migration
ordering logic — the top real-world flake source — extracted into pure state
machines (the `SessionState` pattern: explicit typed states +
`can_transition_to`) with side effects behind a `HostEffects` trait, plus a
`SimFs` with crash-point injection at every write/rename boundary of
`durable_record.rs`'s fsync-rename-fsync-parent contract. Real-kernel
behavior (netlink, actual NBD devices, KVM) stays in the Firecracker CI lane
forever.

**Phase 2 invariant #1 — the acked-write durability oracle** (added
2026-07-16 from the session-85e0298a corruption RCA, PR #712): *every
guest-acked write is recoverable from (published manifest ∪ shutdown spool ∪
uploaded chunks) after any crash, at every crash point.* The RCA found a
class of guest-silent acked-write loss with three mechanisms in one day; two
were kernel page-cache behavior (permanently the FC lane's job — see
non-goals), but the third — the SIGTERM flush-deadline overrun dropping the
RAM dirty tier — was a pure shutdown-pipeline ordering bug, exactly what
this phase's seeded crash-point injection explores systematically. The
Phase 2 world model tracks acked writes explicitly and asserts the oracle
after every injected crash/restart, so the *class* is covered rather than
one bespoke regression per incident. Interim coverage until Phase 2 lands:
the spool's post-crash on-disk states are exhaustively tested at the file
level (the ADR 0099 H5 pattern — every state externally constructible), and
the "checkpoint driver retries forever against a dead data plane" follow-up
is a D6 convergence-invariant customer (retry-forever reads as
non-convergence at quiescence).

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
