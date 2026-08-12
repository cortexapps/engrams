# ADR 0099: Correctness hardening & test isolation

Status: 2026-07-17 — **Accepted.** The build items all landed: H1/H2
per-test template-cloned databases (#695/#696 — the coordinator PG lane
went from `--test-threads=1` to 16-way parallel, 143 tests in ~12s);
H3 proptest across three PRs (#698 manifest properties, #710 codec
round-trip identity + variant-exhaustiveness guards, #708 the mkext4
flagship); H4 decode-never-panics for all five wire crates (#699 +
BrowserActivity coverage in #710); H5 FaultyBlobStorage + chunk-store
atomicity (#701), durable_record torn-write sweeps (#707), and the
shutdown-spool crash-state sweep (#718 — whose acked-write oracle
caught a real torn-chunk silent-adopt hole in #712's fix before it
merged); H6 invariant!/soft_invariant! + the deliberate site list
(#702). The follow-ups PR closed the accumulated findings (the
transition-to-Queued fifo guard from the D4 conformance suite's first
catch; EvictionFinalizeRecord onto durable_record, closing its missing
parent-dir fsync; the SessionState walk properties).

Dispositions for the investigation items: **H7 (#582)** was
root-caused and fixed in #598 (the parked-probe vs in-flight-doomed
request ambiguity; request timeout pinned under the completion budget,
tracing wired) and the issue is closed — no recurrence across the
heavy FC-lane activity since. **H8 (#403)**: timeline correlation
identified the unpinned Claude CLI crossing to 2.1.187 (the ADR 0054 /
#431 headless-regression release) as the cause; CI has since pinned
2.1.185 and the un-quarantine PR re-adds the test with its own e2e
lane as arbiter. One sibling flake filed with evidence during the
program: #721 (the spool-adoption timing window in #712's FC test,
same family as its documented claim race).

Open design decision (deliberately NOT blocking this flip): the H6
site-5 audit found `mark_host_dead_and_orphan_sessions` bulk-flips
non-terminal sessions to `host_lost` more broadly than
`can_transition_to` allows. The DST harness's transition-legality
oracle carries a documented HostLost-only exemption for it; the call —
widen the FSM edges or formally bless host-death as the one FSM-exempt
mass transition — is recorded here for the maintainers.

Companion to ADR 0098 (deterministic simulation testing). That ADR is the
months-scale spine of the "TigerBeetle lessons" program; this one is the
weeks-scale arc: per-test Postgres isolation, property-based testing, decode
fuzzing, storage fault injection, a small set of targeted runtime invariants,
and dispositions for the currently-known flaky tests. It also records — so we
stop re-litigating them — which TigerBeetle practices we deliberately
**reject** and why.

## Context

The trigger is the same as ADR 0098's: we fix timing/ordering bugs reactively,
one bespoke deflake commit at a time. But not everything needs a simulator.
Several structural problems have cheap, standalone fixes with immediate
payoff, and several TigerBeetle practices (property testing, fault injection,
assertion density) apply at the unit/component level with no DST dependency.

Current state, surveyed 2026-07-16:

- **One shared test database.** 21 `crates/engram-coordinator/tests/*_live_pg.rs`
  files plus `ha_listener.rs` all connect to the same `engram_test` database.
  CI runs the whole PG-gated lane `--test-threads=1` — documented as
  load-bearing because both `ha_listener` tests `LISTEN` on the same
  `session_events` NOTIFY channel and cross-talk. Only two files
  (`admin_evac_live_pg.rs`, `enable_reuse_live_pg.rs`) already do per-test
  `CREATE DATABASE` — the latter added by a deflake commit (c8f8ef4f) whose
  comment documents exactly the global-hosts-table pollution class this ADR
  eliminates.
- **Zero property-based testing.** No proptest/quickcheck/arbitrary/cargo-fuzz
  anywhere in the workspace. The golden wire tests are the closest thing to
  structured invariants.
- **Decode surfaces are unfuzzed.** The positional-bincode protos even
  document a decode panic hazard in a comment (`engram-harness-proto`:
  an untagged enum panics at decode on this wire).
- **Storage fault tolerance without fault injection.** `durable_record.rs`
  codifies a torn-write contract (write `.partial` → fsync → rename → fsync
  parent; `load_all` tolerates torn files), and the fidelity gate guards the
  torn-base-capture incident — but every such test was written *after* its
  incident. There is no way to inject faults systematically.
- **Assertion density ~zero, by culture.** `debug_assert!` appears ~4 times
  across the core crates. The repo's invariant checking is architectural —
  Result propagation, typed retryable-vs-fatal errors, reconcilers, CAS
  transitions — which is genuinely strong, but nothing checks invariants
  *inline at the point of violation*.

## Decision

Six work items, H1–H8, each landable independently of ADR 0098.

### H1 + H2 — Per-test Postgres isolation (highest ROI, first)

**A new dev-only crate `crates/engram-testkit`** (workspace member + hakari;
only ever a dev-dependency, so it never enters prod dep closures) providing:

```rust
/// None => ENGRAM_TEST_DATABASE_URL unset; caller prints the skip line.
pub async fn fresh_db() -> Option<TestDb>;  // TestDb { url, store: PostgresStore, .. }
```

Mechanics:

1. **Template trick.** Bootstrap a template database
   `engram_test_tmpl_<fingerprint>` once, where the fingerprint hashes the
   `sqlx::migrate!` migrator's (version, checksum) pairs — a new migration
   automatically mints a new template. Migrate into the template, then close
   that pool before cloning from it.
2. Per test: `CREATE DATABASE "engram_test_<uuid>" TEMPLATE
   "engram_test_tmpl_<fp>"` — ~100–300 ms versus running the full migration
   chain per test.
3. **Serialize bootstrap AND clones under a `pg_advisory_lock`** on the admin
   connection: concurrent `CREATE DATABASE … TEMPLATE x` fails with "source
   database is being accessed by other users", and nextest is
   process-per-test so an in-process `OnceCell` is not enough. The lock is
   held ~hundreds of ms — negligible against the parallelism win.
4. Per-test databases are **leaked** in CI (the Postgres service container is
   ephemeral). For dev machines the bootstrap opportunistically drops
   `engram_test_*` databases older than a few hours.

Because Postgres `NOTIFY` is per-database, this fixes the LISTEN/NOTIFY
cross-talk class (ha_listener), not just row pollution.

- **H1**: the crate + conversion of the three proof tests (`ha_listener.rs` —
  the one that justifies the whole change — plus the two files whose bespoke
  CREATE DATABASE blocks it retires).
- **H2**: mechanical conversion of the remaining ~19 files; **remove
  `--test-threads=1`** and its explanatory comment from the CI PG lane.
  Expected 3–6× wall-clock reduction on that lane, and the shared-DB flake
  class becomes structurally impossible rather than mitigated.

### H3 — Property-based testing: proptest

proptest over quickcheck: composable `Strategy` API for structured data,
integrated shrinking, and — decisive — **persisted regression seeds**
(`proptest-regressions/`, committed to git): every counterexample becomes a
permanent deterministic regression test, automating the bespoke-regression
culture we already have. Workspace dependency; property tests run in the
normal nextest lane (no new CI lane); heavier targets cap
`ProptestConfig { cases: 64..128 }` to respect the 3-minute nextest
slow-timeout.

Targets, in value order:

1. **Chunk manifest** (`engram-chunk-store/src/manifest.rs`): chunking covers
   every offset with correct bounds and reassembles to the original; manifest
   serde round-trip preserves `content_ref()` (the reuse key that
   `enable_reuse_live_pg` guards at system level — a unit property is far
   cheaper); `validate()` accepts everything the builder produces and rejects
   mutated offsets/lengths.
2. **Wire-codec round-trips** for engram-protocol / harness-proto / agentd /
   substrate-proto / migrate-proto: arbitrary valid values → encode → decode
   == identity. Complements the goldens (goldens pin byte layout across
   versions; round-trips catch encode/decode asymmetry on new fields).
3. **mkext4, the flagship**: arbitrary file trees (depths, empty files,
   block-group-spanning files, modes, symlinks) → stream-pack →
   `mkext4::reader::Fs::open` + `verify()` issue-free + byte-identical
   read-back + pack-twice byte-identical (determinism is ADR 0093's core
   claim and why the pin is exact). Lives in `engram-rootfs-materializer`
   (the read-back path already exists in its materialize tests). Caveat:
   counterexamples inside mkext4 itself need an upstream release + pin bump.
4. SessionState random-walk properties are marginal — the pairwise legality
   table is already exhaustively tested — so ~30 lines ride along with H6
   rather than being claimed as new coverage.

### H4 — Decode fuzzing, proptest-shaped

**cargo-fuzz is rejected for now**: nightly toolchain + libFuzzer + a new CI
lane wired into the Gate and detector, for a bounded decode surface. Instead,
decode-never-panics proptest suites in the normal lane, prioritized by threat
model: the **guest→host vsock boundary first** (harness-proto/agentd frames
are attacker-influenceable once user code runs in the sandbox), then
substrate/migrate protos, then the version-fenced trusted coord↔host wire.

Three shapes per proto crate: (a) arbitrary byte blobs never panic any frame
decode; (b) mutations of valid frames — truncate at every prefix, flip bytes,
extend with garbage — never panic (structure-aware without libFuzzer, reusing
H3's strategies); (c) a length prefix claiming huge sizes must not allocate
before validation (`MAX_MSG_BYTES` enforced pre-allocation; bincode decode
size-limited). Decode-side hardening needs no `WIRE_VERSION` bump. Revisit a
nightly cargo-fuzz lane only if this suite starts finding real bugs — record
that decision point here so it stays revisitable.

### H5 — Storage fault injection

Two seams, deliberately different mechanisms:

- **`engram-testkit::storage::FaultyBlobStorage`** wrapping
  `Arc<dyn BlobStorage>`, driven by a **scripted `FaultPlan`** (fail the Nth
  put, truncate a get after K bytes, ENOSPC mid-drain, NotFound on head) —
  deterministic and reproducible; *randomized* injection is ADR 0098's job,
  but the plan is seedable so the simulator reuses the same wrapper. It must
  override the trait's convenience-default methods too, or injections leak
  around the wrapper. First tests: chunk flush atomicity (no manifest version
  ever references an un-uploaded chunk — the fidelity-gate property as a unit
  guarantee); truncated get → hash-mismatch detected by the resolver (its doc
  comment says implementations *must* verify; this enforces the contract);
  cold-resume NotFound → typed retryable error, never a hang.
- **`durable_record.rs` torn-write tests: no new trait, no `fail` crate.**
  Every post-crash on-disk state is externally constructible (leftover
  `.json.partial`, truncated `.json`, garbage suffix) because `load_all`
  takes a plain directory. A `DurableFs` trait would thread through call
  sites purely for tests (violating simplify-via-abstractions), and the
  `fail` crate sprinkles global-static failpoints through prod code. Tests:
  an exhaustive truncation sweep (every byte offset of a ~1 KB record — cheap
  and total, better than proptest here) asserting other records still load
  and nothing panics; and redrive-with-torn-state extensions to the eviction-
  finalize and capture-job recovery tests. Crash *during* a run (kill between
  fsync and rename) belongs to ADR 0098 phase 2's `SimFs`.

### H6 — Targeted runtime invariants (TigerBeetle assert density, adapted)

Not blanket asserts — the Result-propagation + reconciler culture stays. Two
macros in `engram-core/src/invariant.rs`:

- **`invariant!(cond, …)`** — always-on, panics, `#[track_caller]`. Cheap
  here precisely because the recovery machinery already exists: coordinator
  pods are stateless over PG (ADR 0047) and host state machines are
  redrive-safe (ADR 0028/0034/0079), so a panic is a loud restart, not data
  loss. ADR 0083 (fail-closed bind) is the precedent.
- **`soft_invariant!(cond, …)`** — error-log + metric, no panic. For
  reconciler-class sites whose *job* is repairing anomalies: panicking the
  checker on detection would prevent the repair.

The deliberate site list (each addition gets a one-line entry here):

1. Binding-epoch monotonicity at mint/dispatch sites — always-on (a regressed
   fencing epoch voids the fencing story).
2. Chunk offset/length arithmetic in chunk-store read paths — `debug_assert`
   (hot NBD path; on under tests and the simulator, off in prod).
3. Chunk digest verification centralized in the tiered resolver — always-on
   (serving corrupt bytes into a VM's block device is strictly worse than an
   I/O error). If per-tier impls already verify, this is consolidation.
4. Sandbox-ownership uniqueness in reconcile/dead_host — `soft_invariant!`
   plus fail-closed refusal to act on the conflicting row.
5. Audit for raw session-state `UPDATE`s bypassing `try_transition_to`; route
   stragglers through it (the assert mechanism the repo already has — the
   work is closing bypasses).
6. `terminal_target` legality — upgrade the existing debug assertion to the
   shared macro for `#[track_caller]` diagnostics.
7. Cold-boot argv-slot agreement in `boot_materializer::materialize_cold_boot`
   (ADR 0116) — always-on: a spec whose harness argv points at an unbacked
   `dyn` slot converts every downstream resume attempt into a deterministic
   spawn-ENOENT loop (the 2026-08-12 incident); failing the op loudly is
   strictly better, and the coordinator is stateless over PG so the redrive
   is safe.

**Landed (H6 macro PR).** The two macros live in `engram-core/src/invariant.rs`
(the `tracing` facade — no subscriber, no I/O — is the only new dep; the no-I/O
charter holds). `soft_invariant!` logs at `error!` with the stable, greppable
prefix `soft-invariant violated:` and a `name` field; engram-core carries no
metrics registry, so a counter rides the log pipeline (alert on that prefix /
the `soft_invariant` field). Per-site dispositions:

1. **Added** (always-on) — `session_boot.rs` asserts the freshly minted binding
   epoch clears the floor (`>= 1`) at the dispatch site where it is stamped into
   the AgentSpec + bind RPC. Strict pairwise monotonicity is already enforced
   fail-closed one layer down at the host-agent binding record
   (`bindings::bind` refuses `existing > presented`); the coordinator's
   fresh-spawn mint sites don't co-locate a prior epoch, so the floor check is
   the assertion available without an added DB read.
2. **Added** (`debug_assert`) — `Manifest::chunk_at` (query offset aligned to
   `chunk_size`) and `RegionChunker::chunk_len` (start in-bounds; length within
   one chunk). Not covered by `validate()`, which guards manifest entries, not
   the query offset.
3. **No-op / consolidation** — both production resolver tiers already verify:
   `BlobStorageResolver` re-hashes on read and `OciChunkResolver` re-verifies
   against the bootstrap's expectation on top of `pull_chunk`'s digest check.
   The `ChunkResolver` contract already documents "byte-correct or error", so
   adding a re-hash at the `TieredChunkResolver` composition seam would be
   redundant hashing on the hot page-in path. No code change; the invariant is
   the existing per-tier verification.
4. **Added** (`soft_invariant!`) — `reconcile::flip_missing` fires when a
   sandbox drawn from `host_id`'s own PG assignments is cached under a different
   host (same sandbox on two hosts). The reconciler proceeds to repair
   (cache-row drop + the issue-#211 CAS-guarded flip is the existing
   fail-closed behavior); the invariant only adds the alertable line.
5. **Audited — no real bypass to close.** `transition_session` /
   `fenced_transition_session` route through `try_transition_to`. The remaining
   raw status writes each encode a single legal edge as a SQL CAS
   (`pending → created`, `idle → queued`, `queued → pending`, all in the FSM
   table). **One divergence to flag for follow-up:**
   `mark_host_dead_and_orphan_sessions` bulk-flips every non-terminal session on
   a dead host to `host_lost` via a `WHERE status NOT IN (terminal)` guard,
   which is broader than the FSM's legal in-edges to `HostLost` (only
   `Created/Active/Evicting/Unreachable`; `Pending/Queued/Idle/Evacuating` are
   not in the table). Routing this per-row through `try_transition_to` would
   risk stranding a session on a dead host, so it is deliberately left as a
   bulk transition — recommend a separate change to either widen the FSM edges
   or formally document host-death as an FSM-exempt mass orphan.
6. **Added** — `SessionState::terminal_target`'s debug-assert upgraded to
   `invariant!` (cold forced-termination path; now `#[track_caller]` and fires
   in prod).
7. **Added** (`soft_invariant!`, ADR 0098 P7) — `PooledBackend::resume` (the
   un-pause data-plane gate) fires when a rung-cancel resume would un-pause a
   guest onto a rootfs NBD device this host-agent generation does not serve
   (`rootfs_device().is_some()` yet the sandbox is absent from `nbd_sandboxes`).
   The 731df805 dead-plane class: a rung-parked survivor's device was
   disconnected by the stale-binding sweep and an un-pause landed on it. The
   handler proceeds to route the caller into the `evict_local → resume` ladder
   (an explicit early-return; the macro only adds the alertable line), so a
   future listing-bug recurrence can never serve dead-plane reads.
8. **Added** (`soft_invariant!`, ADR 0098 §Phase 3 Wave 6 / R6, #784 layer 1)
   — the startup stale-binding sweep (`recover_one_stuck_device`) fires
   `sweep-blocked-live-holder` when a dead-owner NBD device it would otherwise
   DISCONNECT still has a live process holding its node open (the
   `device_has_live_holder` proc-scan returned `LiveHolder`/`Unknown`). The
   #769 gap-A class: a survivor whose rehydrate was missed upstream still has a
   live guest reading its rootfs. The sweep leaves the device RECONNECTABLE and
   parks (the macro only adds the alertable line; a companion
   `engram_nbd_sweep_blocked_live_holder_total` counter carries the same signal
   to Prometheus). Should stay at/near zero — a firing is the gap-A recurrence
   counter until layers 2–4 land.
9. **Added** (`soft_invariant!`, ADR 0098 §Phase 3 Wave 7b, #784 layers 2–3) —
   the startup classification barrier (`PooledBackend::classify_startup_slots`)
   fires `rehydrate-unknown-device` when the kernel-derived inventory
   (`NbdKernel::connected_devices`) reconciled against the tracked records finds
   a CONNECTED `/dev/nbdN` with a live (or unprovable) holder that NO record
   accounts for — the #769 gap-A survivor, invisible to both the coordinator
   rehydrate list and the #739 local pass (`SlotClass::QuarantinedUnknown`). The
   device is left RECONNECTABLE (kernel-bound, kept out of new-claim circulation
   by the `nbd_kernel_busy` probe) and NEVER handed to the destructive sweep (the
   `ReapList` type makes that structurally impossible); a companion
   `engram_nbd_rehydrate_unknown_device_total` counter carries the signal to
   Prometheus. Distinct from site #8: THAT is the per-device sweep declining to
   sever; THIS is the reconcile finding a device it cannot account for at all.
   Should stay at zero — a firing means a survivor's records were lost upstream
   and an operator/runbook must reconcile the device.

### H7 / H8 — Dispositions for the known flaky tests

| Test | Disposition |
|---|---|
| `ha_listener` + the `*_live_pg` cross-talk class | Fixed by H1/H2 (NOTIFY is per-database). |
| `nbd_netlink_reconfigure` (#582) | **Targeted investigation now.** Kernel-netlink timing on real KVM — DST will never model netlink; waiting on ADR 0098 is a category error. |
| `two_host_live_teleport` | Hardware smoke; keep. Its *logic* races wait for ADR 0098 phase 2; audit poll loops opportunistically when touched. |
| `e2e_claude_with_bogus_key` (#403, quarantined) | Not flaky — **broken** ("consistently times out"). Standalone investigation of the SSE auth-error path; un-quarantine by restoring it to the e2e filter. |
| e2e bring-up variance (#487/#618/#117), `e2e_vnc` | Environmental (cold bakes, runner variance) — runner-level posture, not code. |

**nextest retries: considered, rejected for now.** "Investigate, never paper
over" is the standing rule, and a blanket `retries` line silently converts
every future real race into a 2×-slower green run. If revisited after H2
re-ranks the flake budget with data, the only defensible shape is
`retries = 1` scoped to the e2e-stack binary only, paired with a CI step that
greps for `FLAKY` and files an issue — retries as a *detector with a paper
trail*, never a suppressor.

## Addendum 2026-07-18 (the ADR 0098 Phase 3 audit): corrections + H9/H10

The full-code audit that opened ADR 0098's Phase 3 (the R-series) touched
this ADR in four places; recorded here so this document stays honest:

1. **H5 correction.** "The plan is seedable so the simulator reuses the same
   wrapper" did not happen: `engram-dst-host` has no engram-testkit
   dependency and no swarm run injects storage faults —
   `FaultyBlobStorage` is used only by targeted chunk-store unit tests.
   Wiring it into the host chaos profile is R3.
2. **H6 correction + change.** The `soft_invariant!` `name` field was
   `stringify!(cond)` — the P7 site's condition is a variable named `ok`, so
   every prod violation logged `name="ok"`, useless as an alert-grouping
   key. R0 changes the macro to take an explicit slug as its first argument
   (all sites updated). More importantly: when the P7 un-pause gate fired in
   prod on 2026-07-18 (three times per sandbox, two sandboxes — the
   731df805 class recurring), **nothing consumed the log prefix** — "a
   counter can ride the log pipeline" had no consumer. R0 wires a real
   log-based alert (engrams-internal) and a triage SLO for the nightly
   `sim-failure` issues; detection without response was the program's most
   acute gap.
3. **H6 chokepoint gaps** found by the audit, queued as R1 site-list
   additions (each will get its numbered entry here when it lands): chunk
   read-path length validation against the manifest (a hash-valid short blob
   currently panics the NBD slice); the NBD serve loop's pre-validation
   allocation of a corrupt header's claimed length (violates H4's
   no-alloc-before-validation principle on a non-proto surface);
   first-`bind` accepting epoch 0. The open `mark_host_dead` FSM divergence
   (site 5 above) is now load-bearing: the nightly sim's first real catch
   (#762) and the live prod stall (session 5941d947) are both
   HostLost-convergence failures — the design call should be made as part
   of that RCA, not deferred again.
4. **H9 (new, rides R3): checksums on durable formats.** `durable_record`,
   the spool *metadata*, chunk-store manifests, and chain-head records are
   plain JSON — a syntactically-valid corruption is trusted, so H5's
   torn-write tolerance and the resolvers' chunk-digest checks guard only
   part of the durable surface. R3 adds envelope checksums (clean-break
   format bumps) plus a detection→response policy: the tiered resolver
   currently *falls through* on a hash mismatch and leaves the corrupt copy
   in place (cache-fill even skips because the object exists) — detection
   without repair.
5. **H10 (new, rides R1): mechanical teeth for the process rules.** The D4
   same-PR conformance rule and the DriverKind coverage claim were
   convention only (the shipped "meta-test" checks five function pointers;
   three `MetadataStore` methods silently inherit trait defaults contra
   panic-not-default; the clippy gate is effective in 4 of 45 crates, with
   `engram-host-agent` missing `[lints] workspace = true`). R1 replaces
   convention with mechanism: a real run_once/DriverKind inventory test,
   panic stubs for the default-inheriting methods, lints inheritance fixed,
   and the gate extended to every decision-bearing crate.

## TigerBeetle practices we explicitly reject

- **Static allocation.** TigerBeetle fixes all memory at startup because it
  is a single-purpose replicated state-machine kernel with a fixed message
  budget. engrams is a multi-tenant orchestrator over Postgres and object
  storage with elastic session counts, on an ecosystem (tokio/sqlx/tonic)
  that assumes allocation. The *intent* — bounded work — is already captured:
  bounded channels, semaphore budgets, named limit constants, durable retry
  budgets. Adopt the spirit as a convention: no new unbounded channels; every
  queue names its bound.
- **Single-threaded control loop.** TigerBeetle derives determinism from one
  core owning all state. Our control plane is deliberately multi-replica for
  HA (ADR 0047); the serialization point is Postgres (single-writer rows,
  CAS, leasing), not a thread. Collapsing to one loop would forfeit HA for a
  determinism we instead recover *in tests* via ADR 0098.
- **Zero dependencies.** TigerBeetle vendors nothing because a consensus
  kernel must be auditable to the byte. Our reliability comes *from*
  battle-tested dependencies (Firecracker, sqlx, tonic); re-implementing them
  is strictly worse. Keep the existing hygiene instead: exact pins where byte
  layout is contractual, hakari, minimal features.

## Phasing

1. **Week 1**: H1, H2 (PG isolation; drop `--test-threads=1`); H3 PR 1
   (proptest dep + manifest properties).
2. **Week 2**: H3 PR 2 (codec round-trips) + H4 (decode-never-panics — shared
   strategies, one review arc); start the #403 and #582 investigations; the
   H6 macro PR (ADR 0098 wants `invariant!`s firing under simulation, so the
   macro lands before the harness).
3. **Weeks 3–4**: H5 (FaultyBlobStorage + chunk-store tests; durable_record
   torn-state tests); H3 PR 3 (mkext4); remaining H6 sites opportunistically
   with adjacent work.
4. **Deferred, recorded**: cargo-fuzz nightly lane; nextest retries.

This ADR flips **Accepted** when H1–H6 have landed and the H7/H8
investigations have issues or fixes filed; the commit chain is recorded here
per the bookend convention.

## Risks

1. **Template-DB concurrency** — clones racing the template fail with
   "source database is being accessed"; serialized under the advisory lock,
   with the measured per-test cost reported in the H1 PR.
2. **Leaked test databases on dev machines** — mitigated by the age-based
   sweep in the bootstrap.
3. **Proptest CI-time inflation** — capped cases per target; heavy targets
   watched against the nextest slow-timeout.
4. **"Random tests are flaky tests"** — answered by persisted regression
   seeds: failures shrink to a checked-in deterministic case.
5. **Always-on panics in an HA pod** — the defense is the crash-recoverable
   architecture argument above, the deliberately tiny site list, and
   `soft_invariant!` for reconcilers; spelled out here so it isn't
   re-litigated per PR.
