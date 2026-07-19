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
**Phase 3 (the R-series, the honest-gap program) opened 2026-07-18** from a
full audit of this ADR against the shipped code and TigerBeetle's actual
practice — including a corrections list for claims above that overstated
what landed. See §"Phase 3: the honest-gap program".

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
6. **Shared on-disk state + unsorted directory listings are a
   PLATFORM-divergent replay class** (added R3, wave3-placement-authority).
   Two failure modes compound: (a) an `fs::read_dir` listing feeding a
   decision yields keys in filesystem-dependent order, so the same seed can
   diverge across filesystems/platforms (green on macOS, invariant on
   Linux) — the replay-twice self-check (#5) is same-machine and CANNOT see
   this; and (b) a PROCESS-GLOBAL on-disk store shared across worlds lets one
   seed's world observe/mutate another's state. Both bit the faithful-host
   blob store: a single `std::env::temp_dir().join("engram-dst-blobs")` shared
   by every `SimWorld` let one world's snapshot-blob GC sweep list a sibling
   world's live `state.bin` blobs (unpinned in ITS metadata) and delete them,
   in `read_dir` order — stranding the sibling's queued resume
   (`quiescence-queued-with-capacity`), Linux-only, only under
   `nextest --workspace` concurrency. Fixes: each `SimWorld` owns a PRIVATE
   `TempDir` bucket (no cross-world residue), and `LocalBlobStorage::list_prefix`
   returns lexicographically SORTED keys (the GCS/S3 `list` contract — the
   local backend must match prod, and no consumer inherits a `read_dir`-order
   leak). Audit rule: any real-filesystem or otherwise process-global resource
   a driven path reads must be per-world isolated, and every directory listing
   sorted at the source.

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

**Status 2026-07-17: COMPLETE.** The original sketch here said "future, own
ADR" — superseded: Phase 2 extends THIS ADR in place as the P-series,
mirroring the D-series (decision recorded the day the D-chain merged; the
incidents this phase targets — 85e0298a acked-write loss, torn
base-capture, the NBD teardown flake family, the teardown mis-reap — all
lived in host-agent paths the coordinator sim cannot see).

**The commit chain (opened and closed 2026-07-17):** P0 #733 → P1 #736 →
P2 #737 → P3 #738 → P4 #740 → P7 #742 (pulled forward) → adversarial
clarification #747 → P4.5 #749 → gap record #750 → P5 #753 → G1 #754 →
G2 #756 → P6 #757 → P8 #758 → P9 #759 (the `test-host-sim` lane + the
nightly host swarm). Six flows extracted and
simulated (A shutdown, B slot/reattach, C reconcile, D eviction finalize,
E migration, F flush-seam), eight of the nine oracles standing (#1 acked-
write, #2 no-plane-leak, #3 slot accounting, #5 single-device +
served-never-dead, #6 finalize-stage monotone, #7 migration decision
table, #8 finalize convergence, #9 None-arm; #4 spool-only-copy is
subsumed by the P4.5 published-floor split — recorded in the P5 row),
both #743 coverage gaps closed, and every portable historical hazard
pinned as a regression seed. Kernel truth stays in the FC lane forever
(non-goals below).

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
remain ADR 0099 H5's exhaustively-constructed static tests; H5 owns
exhaustiveness at byte granularity. The seam owns reachability at
operation granularity — crashes injected *between* the real durable
operations. **Landed in P5**: `durable_record` and the spool perform every
durable op through the injected `HostFs` (the earlier draft's
raw-`tokio::fs` + self-mapping crashpoint meta-test — which only proved
the model self-consistent — is retired along with the P2 static
`CrashPoint` catalogue and the post-hoc `crash_state` construction). The
consequences held:
- `HostFs` gained `create_dir`/`remove_dir` (create_dir_all /
  remove_dir_all semantics), covering spool *replacement*'s destructive
  `remove_dir_all` → `create_dir_all` window the write/rename ops don't.
- The crash-point schedule is **derived from the production op sequence
  by running it**: `CrashFs` (the sim's `HostFs`) records the op trace of
  the real `persist()`/`write_spool()` bodies and cuts at a seeded op
  index (ops before the cut ran for real — the on-disk state IS the
  crash state); `crashpoint_coverage.rs` pins the recorded traces against
  the production sequences and the schedule `0..=trace.len()`.

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
| P5 | **Landed.** Flow D (eviction finalize) + the real `HostFs` interception. **Extraction:** `FinalizeStage` moved to **`engram-host-core::finalize`** verbatim (serde names unchanged → records byte-compatible) with `LADDER`/`next`/`can_advance_to` (the `ShutdownStage` shape), plus `plan_finalize_retry` → `FinalizeRetry{Retry{backoff}\|Quarantine}` (the ≥max→Quarantine arm is what guarantees every redrive loop terminates); the driver loop is a thin `run_eviction_finalize_attempt` (pub — the REAL loop body: sleep-free pass + verdict handling) + a sleep. `EvictionFinalizer` decoupled from `PooledBackend` through the 1-method **`EvictionSandbox`** seam (the only coupling was the terminal best-effort destroy; prod adapter `PooledDestroyer` upgrades the weak ref at call time, gone-backend = success). Tail-cancellation split three ways: the pure predicate `checkpoint_tail_admits_publish` extracted (host-core) and called by `persist_at_epoch`; the detached-tail RACE stays the host-agent unit test (needs real threads); the capture-lock lockout is asserted as `snapshot_begin` idempotency (same-id re-observation) in the sim. **The fs seam is real:** `HostFs` gained `create_dir`/`remove_dir`; `durable_record::{persist,load_all,delete_acked}` + `spool::{write_spool,read_spool,discard_spool}` take `&dyn HostFs` and issue every op through it (`write_spool`'s `spawn_blocking` dir-fsyncs became `fs.sync_dir` — behavior-equivalent, and no detached blocking task for the sim); `PooledBackend` carries `host_fs` (prod `TokioFs`); flows not yet extracted (capture-job, chain-head, the periodic-checkpoint writer) pass `TokioFs` at their wrappers — the seam lands with the flows that cross it. **Sim:** `CrashFs` (op-trace recorder + seeded op-index cut) replaces `CrashPoint`/`crash_state`/the self-mapping coverage test (see §SimFs); steps `SnapshotBegin` (real drain → `persist_disk_pending_chunks` → durable record; idempotent) / `FinalizeTick` (real `run_eviction_finalize_attempt`) / `FinalizeCrashAt(idx, op)` / `SpoolCrashAt(op)`; the restart resume leg re-drives via the real `load_all` (the `resume_pending_finalizes` analog); a completed finalize's disk manifest **raises the published floor** (a real durable publish); quarantine leaves the floor at the prior tier (honest bounded rollback). Oracles **#6** (stage monotone across crashes + stage⇒fields — a `DiskUploaded`+`disk_manifest=None` record is the #743 shape) and **#8** (convergence at quiescence: every started finalize completes or quarantines within the attempts budget after a bounded drain) join the suite; #2/#4 stay open (#4 substantively covered by the P4.5 floor split; #2 needs the G2-era capture/resume extraction). Seeds: `spool_cut_at_every_op_recovers_every_acked_write` (replaces the static-boundary sweep), `finalize_completes_publishes_the_floor_and_destroys`, `finalize_crash_at_every_op_resumes_and_completes`, `finalize_quarantine_after_max_attempts_is_convergent` (the "finding 1" ENOENT class), `snapshot_begin_idempotent_under_pending_finalize`. **Two real catches while landing:** the swarm found the sim world resurrecting a captured sandbox's backend mid-finalize (the capture-lock lockout now gates `spool_adopt`/`reserve_and_serve`), and — fidelity — the sim's `rebuild` was missing the production lineage gate's VERSION half (`meta.version >= disk_manifest.version`, `rehydrate_sandbox`): a stale spool could adopt old bytes over a newer durable pointer once Flow D could advance it. | L |
| P6 | **Landed.** Flow F — the flush scheduler seam. The P2-era `#[cfg(test)]` #204 handoff barrier generalized into the 3-point **`FlushSeamPoint`** seam on `ChunkedDiskBackend` (`DirtyPendingHandoff` — drained chunks in `pending`, both tier locks held; `PostUploadPrePublish` — puts durable, fence re-check + publish ahead; `PreRebase` — manifest published, rebase not) with `arm_flush_seam(point)`/one-shot fire. Deliberately un-`cfg`'d so `engram-dst-host` can drive it: the cost is one uncontended mutex check per flush STAGE on a ~30 s cadence — the per-op data plane never touches it (the ADR's original zero-hot-path-cost intent holds). The existing host-agent #204 atomicity test rides the new arm API unchanged. **Sim:** steps `FlushHandoffRace` (#204 — a guest read+write race the parked handoff; the read must decode the drained or racing tag, never pre-drain stale base; the racing write is ledger-acked and must survive the published floor), `FlushFenceAbort` (#199 fence leg — the migration fence rises while parked post-upload; the publish aborts with the manifest unmoved and dirty re-queued, the floor never moves on the aborted attempt, the post-heal flush publishes the re-queue), and `FlushPreRebaseCrash` (the store-ahead crash window — the parked task dies between `put_manifest` and the rebase; the orphaned manifest occupies next-version, the floor never rose so the rollback is honest, and the successor's next flush recovers through the REAL version-conflict retry). All three ride both swarm profiles (the crash variant Chaos-only). Pinned seeds: the four above plus `concurrent_flushes_serialize_never_reorder_publishes` (#199 ordering leg — a second flush blocks on the pipeline guard behind a parked first; drain order == publish order, or the floor would sit above the served content and oracle #1 fires). | M |
| P7 | **Landed.** Flow B (NBD slot/reattach). **Extraction:** the `NbdKernel` seam (P1's unwired trait) is rewired — `HostNbdKernel` (the Linux prod impl over `nbd_netlink` + sysfs) is bound at the public attach/reattach entry points and the CONNECT/RECONFIGURE/backend-identifier touches funnel through `&dyn NbdKernel` at `serve_at` (public signatures unchanged; no blind FC-test churn). The decision content is pure in **`engram-host-core::reattach`**: `plan_reattach` (backend-id echo-else-fallback + the seed-dirty-BEFORE-RECONFIGURE ordering as an explicit `ReattachPlan`/`ReattachStep` property), `sweep_verdict`/`PidLiveness` (the stale-binding "dead-owner-only" core, wired into `recover_one_stuck_device`), `is_local_survivor_candidate` (the #739 filter core, wired into `local_survivor_candidates`), and `resume_data_plane_served` (the un-pause gate). `SlotState` (Free/Warm/Claimed/Parked) + a transition table make the allocator's implicit FSM auditable next to the (unchurned, portable) allocator. **Riders:** verify-on-read — post-RECONFIGURE, when a spool was adopted, a single-chunk probe (`first_seeded_probe`/`probe_matches`) proves the device serves the seeded acked bytes (not rolled-back base) or returns the slot for park; a new minimal `#[ignore]`'d FC lane test (`nbd_verify_on_read`, wired into `ci.yml`) proves it at the O_DIRECT device plane. Un-pause data-plane gate — `PooledBackend::resume` fails fast into `evict_local → resume` (a `soft_invariant!`, ADR 0099 H6 site #7) when the rootfs device isn't served by the current generation. **Sim (`engram-dst-host`):** the device-serving model (generation + `served_by`/`kernel_owner`/`parked` + the real `NbdSlotAllocator`) with steps `Park`/`Unpause`/`RegisterRehydrate`/`StaleSweepTick`/`SlotClaim`/`SlotPopulateTick`; oracles #3 slot-accounting (`free + warm + held == capacity`, no double-claim) and #5 single-device-ownership + *served-device-never-dead* (the 731df805 property) as standing invariants; the local-rehydrate leg adds a recovery arm to oracle #1's closure. **The 731df805 scenario is pinned** (`park → roll → register → sweep → un-pause`): the FIXED variant (buggy coord list omits the parked survivor, the #739 local ChainHeadRecord pass re-serves it, the sweep skips it, un-pause serves) and the UNGATED variant (local pass off → the sweep disconnects the live device → the un-pause gate is the last line and fires, no dead-plane serve). The tight concurrent claim-vs-populate validation-window stays the multi-thread host-agent test (paused single-thread tokio can't hold a `claim` mid-populate, and `claim`'s retry `sleep` hangs on the paused clock — the sim drives the transitions as explicit steps). | L |
| P8 | **Landed.** Flow E (migration). **TTL clock → `now_mono`:** `MigrationExport` carries the injected clock; `created_at`/`last_activity` are `now_mono` readings and `expired()` subtracts against the injected clock — expiry DECIDES destroy/abort, so it is decision-feeding time (D1), off the `metrics_now` carve-out it previously rode; the prod constructors bind `PooledBackend.clock`, and the paused sim clock drives the REAL `expired()` deterministically. The decision table was already pure (`ttl_verdict`, `reattach_source_verdict` — extracted with #216's fix); P8 wires it into the sim. **Sim:** steps `MigrationBegin` (a REAL `MigrationExport` in the REAL `MigrationRegistry`; deterministic entropy-minted export id — the prod OsRng nonce must not launder into the replayable stream; the guest freezes exactly as the export's capture lock excludes writes/flushes/captures) / `MigrationServeState` / `MigrationTouch` / `MigrationTtlSweep` (REAL `expired()` + `ttl_verdict` over the scriptable coordinator, applied as `lib.rs` does) / `MigrationCommit` / `MigrationAbort`, in both swarm profiles. **Oracle #7** (the #216 decision table): `state_served` ⇒ the dumb-host TTL sweep never abort-unpauses — the SWEEP structurally records any un-pause it applies to a served export, so a `ttl_verdict` regression fires it. The EXPLICIT coordinator abort is deliberately exempt: prod's `migration_abort` RPC allows it even post-ship (the coordinator carries the postcopy-never-loaded knowledge, ADR 0045 C2) — **the P9 lane's very first CI run caught the sim being STRICTER than prod here** (calm seeds 14/16 fired #7 on a legal explicit abort; the model fix + the `explicit_abort_after_state_served_is_legal_and_resumes` pin rode this row). **Oracle #2** (no-plane-leak, the P8 tightening): `migrating` ⟺ an open registry export, with a live backend — a frozen guest with nothing to end it (or an export on an un-frozen guest) is the leak. Seeds: `ttl_expired_unshipped_export_aborts_in_place_zero_loss`, `state_served_export_never_unpauses_then_destroys_on_ownership_flip`, `actively_serving_export_never_expires_mid_transfer` (#216 Gap 1), `unreachable_coordinator_stays_paused_never_guesses`, `reattached_source_verdict_never_destroys_on_a_transient_binding` (#216 Gap 3). **Scope notes:** #582/#598/#629 (named in the original row) turned out to be FC-lane/test-hygiene issues (a netlink parked-IO flake + two test races) whose portable content the P5/P7 machinery already absorbed — no hollow seeds manufactured; the literal races stay FC-lane residue. The full resume-attach flow extraction (G2's rider) is subsumed by the G2 decision seams + this row's registry modeling. **`HostEffects::production` consolidation: deliberately retired rather than done** — every seam reaches its flow through a dedicated field (`clock`, `host_fs`, the coord publish pair, `DeviceSync`/`NbdKernel` at their entry points), the sim injects per-seam, and folding them into one bundle field now would churn `PooledBackend`/`lib.rs` + the FC lane for zero new simulability; the bundle ctor remains the sim's assembly point. | M |
| P9 | **Landed.** CI: the `test-host-sim` lane — fixed windows (chaos 0..60 × 1000, calm 0..30 × 1000; `just sim-host <n>` replays any failure) behind a whole-binary replay-twice self-check (one seed run twice, stdout diffed — the cheapest guard against a nondeterminism leak), own rust-cache key (`workspace-release-host-sim`), gated on the new `test_host_sim` detector flag (the release closure of `engram-dst-host` itself — engram-host-agent/host-core/sim/chunk-store ride in as normal deps; disjoint from engram-dst's coordinator closure by construction), and in `CI Gate.needs:` (never individually required — the aggregator rule). Nightly: `nightly-sim.yml` gains the `host-swarm` job — date-derived non-overlapping windows (360 chaos + 120 calm × 4000 steps; sized below the coordinator swarm's volume for the host sim's real-fs step cost), `--failure-report` → the same slug-deduped `sim-failure` issue auto-filing, title-prefixed `nightly host sim:` so host slugs never collide with same-named coordinator invariants. Regression seeds: populated across P3–P8 (26 pinned scenarios + the two swarm suites). This section closed with the commit chain above. | S |

### Coverage gaps surfaced by real incidents (tracked follow-ups)

A gut-check against the 2026-07-17 incident on session `03e6535e` (PR #743 —
two independent compounding failures: a 40-minute resume-op stall, and NBD
disk corruption → SIGBUS) found both failures are squarely the *classes*
this program targets, yet neither is catchable by the harness as it stands.
Both are the honest boundary of "would DST have cut this," recorded so the
answer is a plan, not a hope:

- **G1 — CLOSED.** A resume op wedged forever inside `start_agent`
  (no gRPC/host deadline) while the within-step heartbeat kept the row
  fresh, so stale-op reclaim never fired — pinned 40 min until a pod roll.
  The close is exactly the increment specced here: `SimHostState.rpc_hang:
  Option<Duration>` replaces the never-read fail-fast `rpc_partitioned`
  flag (every `SimHostClient` verb sleeps the configured stall at entry —
  above the verb's `op_deadline` it is the wedge, only the tokio-timer
  deadline drops the dispatch; below it a plain delay), toggled by
  `Step::RpcHang(host, bool)` (arming 3600s, far above every deadline),
  cleared in the quiescence-heal block, and in the Chaos pick menu. The
  pinned scenario `wedged_boot_op_reaches_terminal_via_op_deadline`
  hand-drives the #743 shape: a Placed create's boot op dispatches onto
  fully-hung hosts, the attempt returns ONLY because `op_deadline` drops
  it (the step would hang the test forever pre-#743), the op requeues
  (attempts advanced, never finished, never booted), and the healed
  quiescence pass converges it — `no_op_dropped` +
  `quiescence-no-stragglers` are the standing catch.
- **G2 — CLOSED (the decision seams + the four-leg family; full resume
  flow extraction stays P8-adjacent).** Two silent-fallback paths keyed
  disk durability on `nbd_sandboxes.get(id)` and skipped for a post-roll
  survivor never rehydrated: capture recorded a `recoverable` snapshot
  with `disk_manifest=None` (dropping acked writes), and resume then
  booted onto the stale literal `/dev/nbdN`. **The recurring shape** — a
  lookup keyed on a tracking map that a post-roll survivor isn't in,
  causing a silent skip that corrupts — now has all four legs pinned as
  the **survivor-invisibility family**: register + sweep cores in
  `engram-host-core::reattach` (P7, the 731df805 pair), capture + resume
  cores in **`engram-host-core::survivor`** (`plan_capture_disk_drain` →
  `Drain|RefuseUntracked|NoNbdDisk`, `plan_resume_attach` →
  `Attach|RefuseStaleLiteral|Materialize`), with the #743 prod guards in
  `PooledBackend::snapshot`/`restore` rewired through them (extraction,
  not behavior change — the #743 unit tests pin both refusals). The sim's
  `snapshot_begin` consults the real capture verdict (an untracked
  resident survivor is `RefusedUntracked`, exercised by every swarm
  `SnapshotBegin` on a post-roll slot), and `resume_finalized` the resume
  verdict over the poisoned-lineage marker a manifestless finalize
  leaves. Three seeds: the FIXED capture refusal (+ rehydrate-then-drain
  remediation), the **ungated capture+resume double failure where the
  acked-write oracle FIRES the below-floor violation** — the direct
  "would DST have caught #743" proof — and the gated resume as the last
  line (poisoned snapshot already manufactured, refusal, no corruption),
  mirroring the 731df805 un-pause-gate seed's defense-in-depth shape.

**The program already shapes fixes ahead of catching bugs:** #743 followed
the D4 conformance rule (implemented `SimMetadataStore::rewind_session_to_cursor`
+ a dual-store conformance case for its PostgresStore SQL change) and made
`op_deadline` paused-clock-deterministic — so even where the harness cannot
yet reproduce an incident, its discipline is already load-bearing on how the
incident's fix is written and tested. G1 and G2 are the next increments that
turn that discipline into detection.

### Residual risk, recorded at close (2026-07-17)

Phase 2 is closed, and this list is the honest boundary of what closing it
bought. The durability-decision core — everything that decides what
survives a crash, roll, eviction, or migration — is covered by standing
oracles with a track record of catching real bugs pre-merge (#722, the
verify-on-read overrun, the mid-finalize resurrection, the stale-spool
version gate, the frozen-slot hang, and the P9 lane's first CI run
catching a stricter-than-prod abort model). What is NOT covered, ranked by
where the next bad incident most plausibly comes from:

1. **Kernel/data-plane behavior** — permanently out of scope by design
   (non-goals below). Two of 85e0298a's three mechanisms were kernel
   page-cache behavior; the netlink false-adopt and parked-IO classes are
   FC-lane territory, and the FC lane is deliberately minimal. The sim
   proves our ordering/decision logic **assuming the kernel behaves as
   modeled**; the prod guards (BLKFLSBUF, verify-on-read, the spool,
   device `sync_all`) are point defenses. *Named next increment (outside
   this ADR): an FC-lane "kernel assumptions audit" tier — one minimal
   test per assumption the world model bakes in (dead-conn park/replay,
   RECONFIGURE semantics, cache invalidation on CONNECT) — so the sim's
   foundation is itself pinned.*
2. **The coordinator↔host boundary is not co-simulated** — the two sims
   are disjoint by design (state-space product avoided; disjoint CI
   closures). #739 was exactly a cross-system interaction, covered today
   only because it happened and its shape was replayed into the
   adversarial stubs. A NOVEL cross-system interaction will be found in
   prod first, then pinned. *Named next increment (outside this ADR): a
   thin boundary harness — recorded coordinator wire traces replayed into
   the host sim's stub (or vice versa) — buying cross-system coverage
   without the product state space.*
3. **Oracle coverage is what someone thought to assert.** The #743
   gut-check proved both failures were in-scope classes yet uncatchable
   until G1/G2 landed the knob and the family. The standing mitigation is
   the gut-check ritual itself: every incident gets "which oracle should
   have caught this?" answered in this section, as G1/G2 were.
4. **Deliberate point-test residue** — the real-thread races (claim/
   populate validation window, the ChainHeadStore epoch race, the #224
   DashMap literal) stay targeted host-agent tests; targeted tests don't
   explore.
5. **The ops layer is untouched by this program** — rolls, capacity,
   cordons, disk pressure, wire skew, bakes. A large fraction of recent
   operational pages came from this layer; no simulator here covers it,
   and sim seeds are the wrong tool for it.
6. **Exploration volume is modest** — ~2k nightly seeds against an
   astronomically larger interleaving space, biased by hand-chosen pick
   weights. Rare multi-fault pileups may still need directed scenarios
   when suspicion arises.

## Phase 3: the honest-gap program (the R-series)

**Opened 2026-07-18** (bookend), the day after Phase 2 closed, from a
full-code audit of this ADR + ADR 0099 against both the shipped code and
TigerBeetle's actual practice (VOPR fault model, the auditor/state-checker
oracles, storage-fault injection, swarm-tested fault parameters). The audit's
verdict, recorded honestly: **what shipped is a deterministic lifecycle
regression harness with a real track record, not yet a simulator of the
system.** Three structural absences dominate — no client-visible model
oracle, no in-flight interruption (a step runs to completion, so
committed-but-unacked crash windows are structurally unreachable), and no
silent-corruption fault model anywhere (nothing injects bitrot/misdirection,
and several durable formats carry no checksum to detect it) — and this ADR's
own narrative overstated what landed in enough places that the corrections
below are themselves part of the record.

### Corrections to the record (claims above vs what actually shipped)

Coordinator sim (`engram-dst`), as of 2b4b89ea:

- Profiles are `Calm`/`Chaos` only; `pg-flaky`/`partition-heavy` never
  existed. Preemption-via-SimCloud was never a step.
- There is **no `Api(WorkloadOp)` step and no Router**: nothing tower-oneshots
  the real axum/gRPC surface (0/17 HTTP routes, 0/~69 RPCs driven). The
  workload is fixed-shape DevVM create + resume-first-idle + create-burst —
  2/7 `OpKind`s; Agent mode absent (the "open deviation" above understated
  this).
- The world has **no effect queue**: `SimHostClient` verbs mutate the world
  synchronously after one optional delay. RPC reorder/duplicate/loss — listed
  in the fault menu above — are structurally impossible. `RpcHang` is one
  host-wide stall; `PgOutage` one global fail-before-call bit; `ClockSkew` a
  constant offset (no drift/step model).
- **7 oracles, not 9**: snapshot-safety (#5 in the list above) and
  orphan-sandboxes (#8) were never written; single-ownership checks neither
  `sandbox_owners` nor epoch monotonicity. `Queued` is accepted
  unconditionally at quiescence (the capacity qualifier lives in a comment,
  not code). The placement oracle has drifted from the corrected store
  predicate (#722's "tightened back to unconditional" did not hold).
  — *Closed by R3 (wave3-placement-authority):* the unconditional
  placement-accounting oracle is RESTORED, and the store predicate now
  satisfies it by construction — a `pending` reserves UNCONDITIONALLY (no
  wall-age / live-op exclusion; ONE reservation authority), reclaimed only by
  the ADR 0079 backstop's real `pending → failed` transition. RCA of the
  12/200 faithful-chaos firings: the failing path was NOT the crash-orphan
  pending exclusion (the D6/D7-era hypothesis) but the UNRESERVED RESUME
  soft-pick (`pick_from`'s capacity-soft fallback binding a resume onto a
  measured-full host); resume now honors the same hard reserved-budget bound
  as create (queue-when-no-fit via `placement_preview`). With #722 and #790
  closed, faithful hosts are the swarm DEFAULT.
- The "DriverKind coverage meta-test" is five function-pointer visibility
  checks; it inspects neither `DriverKind` nor the run_once inventory.
  Coverage is ~8/14 production task families — `enable_scanner::run_once`
  (the whole durable enable/capture pipeline, extant since June) was never a
  `DriverKind` variant; pg_listener, checkpoint retention, base-snapshot
  retention (both of which keep their logic inline in spawn loops,
  contradicting the "every driver is a run_once" claim above), preemption
  drain, and chunk GC are absent.
- SimMeta: 136/198 methods implemented, 59 panic stubs, and **3 methods
  silently inherit trait defaults** (`terminate_session`,
  `fc_snapshot_version_for_host`, `notify_session_delta` — the last a silent
  no-op), contradicting panic-not-default. The conformance suite is 17
  scenarios touching ~49/198 methods; the same-PR rule has no mechanical
  enforcement.
- "CI runs each PR seed twice and diffs the traces" is false: three fixed
  unit-test seeds are replayed twice; the swarm ranges run once. The failure
  artifact is the last 40 Debug-formatted steps — no virtual time, no state
  deltas. Per-component forked RNG streams were not implemented (scheduler
  RNG + one shared world entropy).
- The D6 weight patch silently failed to apply, so the D6/D7-era swarms
  never picked the partition/skew/burst arms (since corrected in the
  scheduler with a confession comment). The lesson generalizes: **the harness
  has no sensitivity proof** — nothing demonstrates the swarm can re-find a
  known bug (R5 adds the canary lane).

Host sim (`engram-dst-host`) + storage:

- The acked-write oracle is a numeric interval check
  (`floor <= tag <= latest`) over globally-incremented tags: a misdirected
  read serving another chunk's in-range tag passes. The ledger carries the
  per-chunk history needed for a membership check; the oracle discards it. —
  *Closed by R1.5:* the oracle (and `guest_read`) now require the observed tag
  to be a MEMBER of that chunk's acked-tag set (or the tag-0 base), still
  bounded below by the floor; the third violation class
  (in-range-but-never-acked misdirection) is pinned by a regression test that
  fails against the old interval form.
- "Bounded by the flush cadence and the periodic checkpoint" is not modeled:
  the host scheduler has **no periodic-checkpoint step** and enforces no
  cadence bound — arbitrarily old unpublished acked writes are accepted loss.
  — *Bounded at quiescence by R1.5:* the run's quiescence pass now drives a
  final REAL flush per live sandbox and then asserts no surviving chunk's
  content sits above the published floor (`quiescent-floor` — with oracle
  #1's lower bound, content == floor: everything that survived is
  published), so accepted loss is exactly 'un-flushed at crash', never
  'never flushed'. (Asserting `floor == latest_ack` was the first cut and
  is over-strict: a write lost to an earlier abrupt crash keeps its
  `latest_ack` above the floor forever — the chaos swarm fired it on every
  crash-loss seed; content-based is the honest form.) The mid-run
  periodic-checkpoint step and cadence bound remain open (the rest of the
  R1 row).
- The restart leg runs the sim's own `rebuild`, not production
  `reattach_manifest`; the ledger has no per-write handoff enum (one floor
  per chunk).
- "Everything that decides what survives a crash, roll, eviction, or
  migration is covered" was overstated: capture-job records, chain-head
  records, periodic-checkpoint records, and **eviction disk-pending staging**
  (files that can hold the only durable copy of acked writes) all bypass
  `HostFs` — zero crash-point exploration; migration transfers no disk bytes
  and commit drops the backend, exiting the sandbox from the durability
  oracle entirely.
- The "adversarial" coordinator stub is benign in swarm runs: its
  adversarial arms are one-shot queues used only by pinned tests — no
  scheduler step scripts them — and it structurally cannot express the
  applied-commit-but-lost-ack window (it returns before recording), ignores
  `host_id` scoping, and models 3 of ~15 host→coord routes.
- ADR 0099's claim that the seedable `FaultyBlobStorage` plan "is reused by
  the simulator" is false: `engram-dst-host` does not depend on
  engram-testkit; no storage-fault injection runs in any swarm.
- The kernel-assumptions audit named in residual risk #1: 2 of 4 assumptions
  pinned (park/replay, RECONFIGURE identifier); BLKFLSBUF cross-tenant
  invalidation and cross-fd-fsync-through-the-bdev-page-cache have no test.

Enforcement perimeter:

- The clippy time/entropy gate is effective in **4 of 45** workspace crates.
  `engram-host-agent` has the list but not `[lints] workspace = true` (its
  gate holds only via CI's command-line `-D warnings`). Ungated raw calls sit
  in decision paths: the `engram-core` ID macro mints `Uuid::new_v4()`
  directly, host-operator roll/autoscale deadlines, agentd readiness,
  egress-proxy token freshness. Three `#[allow(disallowed_methods)]` sites
  are outside the blessed list (`mint_export_id`, `build_heartbeat`,
  `unique_path_token`).
- Two determinism leaks in driven paths survived the D5 audit:
  `session_ops::enqueue` detached-spawns `drive_claimed` (mutating work
  escapes the step boundary), and `MigrationRegistry::expired()` feeds
  DashMap iteration order into decisions consumed by both the prod TTL sweep
  and the host sim.

Product bugs found by the audit (fixes ride R1, not the harness):
eviction-finalize uploads staged disk-pending bytes without re-hashing and
stamps the *recorded* hash into the published manifest (corrupt staged bytes
→ chunk stored under a new digest, manifest referencing the old one —
dangling ref, unresumable snapshot, silent); the NBD serve loop allocates a
corrupt header's claimed length (≈4 GiB) before range validation; chunk
length is never validated against the manifest on the read path (a
hash-valid short blob panics the slice); a first bind accepts epoch 0.

### Prod anchors (2026-07-16..18) — why this phase, ranked by these

The nightly swarm's first real catch (#762, seed 33043259: a session stuck
at HostLost after convergence) landed the same day prod had session
5941d947 wedged at `host_lost`: its evict op died on the P7 un-pause gate
(the 731df805 survivor class RECURRED upstream — the gate held, three
firings per sandbox), the coordinator destroyed the VM promising "reconcile
drives HostLost → Idle", and that convergence never happened. **No alerting
consumes the `soft-invariant violated:` prefix** — the firings went
unnoticed until this audit — and the macro's `name` field logs
`stringify!(cond)` (literally `name="ok"`). Two days earlier both
coordinator pods OOMKilled simultaneously (the #704 trigger; no root-cause
issue existed). #570 — the coordinator-unbind vs host-teardown-reconcile
race destroying eviction snapshots mid-upload — remains open: a live
instance of residual risk #2. Detection without response is currently the
program's most acute gap.

### The R-series

**Wave status (2026-07-18):** R0 + R1 are MERGED — #766/#767/#770 (R0; the
straggler sweep drained a live prod backlog on its first ticks) and
#771/#772/#775/#776/#779/#780 (R1; burst-merged after a combined-state
check per the AGENTS.md rule). The #777 design calls are decided
(honest-Dead stage-2 predicate; ask-the-host bound-row policy) and in
implementation. R2 is MERGED: #782 (the #777 calls, incl. the flip_missing
reverse-lie fix), #783 (CoSim rung 1 — **#570 reproduced against real code
on both sides and fixed**; the capture_in_flight reconcile exemption;
issue closed), and #786 (the effect queue, the real tonic+axum-driven
workload, and the acked-only expected-state model oracle — which caught a
live D1 entropy leak in the API create path in its first swarm). The
faithful-host work behind #786 unmasked **#787** (suspected
queue-scanner/OpReclaim double-boot split-brain — the pre-R2 world's
wire-skewed hosts had silently suppressed the entire digest-gated
placement path, the exact "stale fake green-lights bugs" risk this ADR
named). #787 is the next wave's opener; the API-verb swarm integration +
harness-idle eviction + drain workload steps are deliberately held until
it is fixed. Wave 3 (the R3 chain) is MERGED — three RCAs, each of which
OVERTURNED its going-in hypothesis: #791 (the #787 "double-boot" was the
dead-host #231 probe dialing an empty sim host_pool — fidelity gap, prod
analog epoch-fenced; plus the #790 snapshot-fidelity fix: model the
artifact, never fake the flag) and #795 (the #722 over-reservation was
the unreserved resume soft-pick, NOT the crash-orphan exclusion — the
D7-era fix had hardened an untriggered path; one reservation authority
now holds on every placement path, the UNCONDITIONAL placement oracle is
restored, and faithful hosts are the swarm default). #795 also closed a
class the harness could not see about itself: the first landing (#793)
was reverted (#794) after main's Linux lane caught cross-world blob-GC
contamination — every SimWorld shared one process-global temp blob dir,
victims chosen by read_dir order. Per-world TempDirs + a SORTED
LocalBlobStorage::list_prefix (the GCS/S3 contract) fix it, and
"shared on-disk state + unsorted listings ⇒ concurrency/platform-
divergent replay the replay-twice check structurally misses" is now
determinism-audit item 6. Deferred to next waves: the API-verb/drain
workload Steps, #792's recoverable-before-Idle guard,
touch_session_activity retirement. Rung 2 lives in #784. Historical note superseded:
the R2 model-oracle/Router-workload/effect-queue track deliberately waits
for rung 1 to land (the effect queue restructures the same SimHostClient
seam the cosim bridge consumes). Invariant alerting is live-pending-apply
in engrams-internal #89 (Slack #project-engrams).


| Phase | Content |
|---|---|
| R0 | Truth + the response loop: this addendum; explicit `soft_invariant!` name slugs; nightly infra-failure filing; a log-based alert on the soft-invariant prefix (engrams-internal) + a triage SLO for `sim-failure` issues; RCAs for #762, the dead-plane recurrence, and the double-OOM. |
| R1 | Soundness of what exists: the missing coordinator oracles (snapshot-safety, orphan, epoch/`sandbox_owners`), capacity-aware Queued-at-quiescence, placement-oracle realignment; per-chunk membership in the acked-write oracle + a periodic-checkpoint step + a cadence bound; the eviction-finalize hash fix + NBD length validation (prod); a real DriverKind meta-test + the missing drivers (enable_scanner first) + run_once extraction for the two inline-loop retention drivers; panic-stub the 3 default-inheriting SimMeta methods; lints inheritance + gate extension to the decision-bearing crates; fix the two determinism leaks. |
| R2 | The auditor: an expected-state model diffing acked API responses against world truth (acked-create never lost; acked results never contradicted; completed runs never repainted); workload through the real Router/gRPC surface as originally specified; an effect queue giving in-flight interruption + message loss/reorder/duplication. |
| R3 | Storage lies: corruption injection (CrashFs byte-flip/misdirect/stale-read; seeded FaultyBlobStorage in the host chaos profile); checksums on the unprotected durable formats (durable_record envelope, spool metadata, manifest digest, chain-head) as clean-break bumps; detection→response policy (resolver deletes/refetches corrupt primary copies); route the four HostFs-bypassing writers through the seam; the two missing kernel-assumption pins in the FC lane. |
| R-CoSim | Coordinator↔host co-simulation — the boundary where #570, #739/731df805, #602, #216, and 85e0298a all live and which both sims exclude by construction. Rung 1: `engram-dst-cosim` — the REAL coordinator handlers/drivers over SimMeta bridged to the REAL extracted host flows (transport faked, both sides' code real), one interleaved scheduler, directed scenarios for the four known handshakes (#570 must reproduce first). Oracles: coordinator-says-Idle ⇒ the resume-visible snapshot is durable and covers the acked ledger; ownership agreement across the boundary. Rung 2 (post-R2 effect queue): a seeded boundary-fault swarm, small world, own detector flag + CI-Gate membership. |
| R4 | The untouched tier + exhaustion: orchestrator DST (#704's zombie-listener class — clock/entropy injection in TS, run_once-shaped listener/lease/cursor steps, lease-held ⇒ stream-consuming oracle) + the cross-tier smoke un-skipped into the e2e lane; memory/backpressure bounds from the OOM RCA, "every queue names its bound" enforced mechanically, OOM-kill faults in both sims. |
| R5 | Exploration depth: per-seed randomized fault weights + world config (swarm testing proper); disk-pressure/ENOSPC + mixed-WIRE_VERSION faults; the canary lane (revert a known-caught fix behind a cfg, assert the swarm re-finds it); nightly volume scaled across runners. |

Execution note: implementation fans out to Codex (gpt-5.6-sol, low effort)
subagents in isolated worktrees, one PR per row-item, orchestrated in waves;
every subagent diff is reviewed in-session before human merge. The addendum
is updated between waves (bookend convention).

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
