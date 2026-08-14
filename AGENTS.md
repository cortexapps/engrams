# AGENTS.md

Guidance for working in this repo (Claude Code agents and humans alike). Keep it
accurate — if something here drifts from reality, fix it in the same change.

## What engrams is

engrams runs AI coding agents (e.g. the `claude` CLI) inside isolated microVMs as
a service: an API/UI to create a **session**, which boots a sandbox from an OCI
image, runs an agent harness inside it, streams the conversation + tool calls back,
and snapshots/evicts/resumes the VM for density. Production isolation is
**Firecracker** on Linux+KVM; macOS development uses **Apple Virtualization (VZ)**
against the same code paths.

The deep architecture lives in `README.md` and `DESIGN.md`; design decisions live in
`docs/adr/` (the source of truth — see [ADRs](#adrs)). This file is the operational
layer: how to build, test, and the conventions we hold.

## Daily commands (justfile)

- `just dev` — `tilt up`: the whole stack (docker-compose deps + coordinator + host-agent
  + web). Backend is **auto-detected** (`deploy/dev/detect-backend.sh`, ADR 0024):
  `/dev/kvm` → Firecracker, macOS+arm64 → VZ, else → Process. Tilt UI at `:10350`.
  `just dev-down` to stop. First-time: `just bootstrap` (KEK) then `just pull-kernel`.
- **Git worktrees share one dev identity.** The dev Postgres/GCS are machine-global, so
  the KEK and baked bundles live with the PRIMARY checkout (found via
  `git rev-parse --git-common-dir`): `just bootstrap` writes the KEK to the shared
  `.env` (and strips any KEK from a worktree-local one), Tilt reads shared-then-local
  (local wins for overrides like `ENGRAM_SANDBOX_BACKEND`), and `just dev-link-shared`
  (run at Tilt parse time) symlinks `var/shared` + `var/bundles` to the primary
  checkout's — no per-worktree re-bake, no broken sealed keys.
- `just check` — **the full Rust workspace gate**: `cargo fmt --check`,
  `cargo clippy -D warnings`, `cargo hakari generate --diff`,
  `cargo nextest run --workspace`. Run it once before a PR when the change affects Rust,
  Cargo, coordinator SQLx state, protobuf shared with Rust, or shared backend behavior. Do
  not run it for a docs-only, web-only, or orchestrator-only change. CI applies its own
  path-gated full check. (After adding/removing workspace deps, run `just hakari`.)
  **`workspace-hack/Cargo.toml` is generated** — never hand-edit it and never bump a
  version inside it; `generate --diff` fails unless it matches `cargo hakari generate`.
- `just test [args]` / `cargo nextest run -p <crate>` — fast inner-loop tests.
- `just vz-codesign` / `just vz-test` — macOS VZ tests (need the virtualization entitlement).
- `just bake-demo`, `just integration-test`, `just integration-session` — local stack smokes.

Prefer `nix develop` for the toolchain (pinned via `rust-toolchain.toml` + `flake.nix`:
rust, just, tilt, nextest, hakari, sqlx-cli, protobuf, node/pnpm, musl cross). `web/`
is pnpm; `orchestrator/` is bun.

## Sandbox backends

`Firecracker` (Linux, production) · `VZ` (macOS Apple Silicon, local parity) · `Process`
(dev fallback, zero isolation). All implement the `SandboxBackend` trait; the coordinator
is backend-agnostic. **Production drives the design**: change shared code to match the
Firecracker path; VZ exists to exercise that path on macOS, not to fork it.

## Testing

- **Unit/integration**: `cargo nextest run -p <crate>` (in `just check`).
- **Live-PG tests** use `engram_testkit::pg::fresh_db()` — every test gets its own
  template-cloned database (ADR 0099), so the lane runs fully parallel. Never call
  `.migrate()` on the returned store (the template is pre-migrated), and never
  point a second pool at the admin URL — use `db.url`.
- **The conformance rule (ADR 0098 D4)**: any PR that adds a `MetadataStore`
  method or changes PostgresStore SQL semantics MUST extend
  `crates/engram-sim/tests/` in the same PR — every scenario runs against BOTH
  `SimMetadataStore` and real Postgres. In `SimMetadataStore`, an unimplemented
  PG-semantic method panics; never let it inherit a silent no-op default.
- **Property tests** (proptest, ADR 0099 H3/H4): persisted `proptest-regressions/`
  files are committed — a CI property failure means *grab the counterexample from
  the log, pin it, fix it*; rerunning until green is the banned paper-over.
  Integration-test targets must pin `FileFailurePersistence` explicitly (the
  default can't find `tests/` files). Wire-proto strategies carry exhaustiveness
  guards (a wildcard-free `match` over the enum) so a new variant is a compile
  error, not a silent coverage gap; new decode surfaces get decode-never-panics
  suites.
- **Crash-state tests** (ADR 0099 H5): for durable on-disk formats
  (`durable_record`, the shutdown spool), externally construct every post-crash
  state — torn files at every byte offset, missing completeness markers, garbage
  siblings — and assert tolerant recovery. No `fail` crate, no test-only traits;
  if the states aren't externally constructible, the format is the problem.
  Scripted `engram_testkit::storage::FaultyBlobStorage` covers the blob tier.
- **Firecracker integration** (`crates/engram-sandbox-firecracker/tests/`): `#[ignore]`'d,
  Linux+KVM only; run in CI on KVM runners. New FC/NBD regression tests **must** be wired
  into `ci.yml`'s `--test` list (not gated as local-only) or they never run. **Size a test
  to the property it asserts, not to realism — minimize CI time.** Prove a
  correctness/isolation/head-of-line-freedom property with the least data, iterations, and
  wall-time that still demonstrates it (a small, tightly-bounded exercise); scaling it up
  (big transfers, many rounds, long sleeps) only measures throughput/load, which is slow on
  the 2-vcpu microVM, gets flagged SLOW, and reinflates the FC lane we worked to trim.
  Measure scale/throughput ad hoc on the dev VM, never in a CI test.
- **e2e stack** (`test-e2e-stack`): boots the full prod-shape stack — the only lane that
  exercises the coordinator HTTP/gRPC path end to end. Never delete e2e coverage without a
  replacement landing in the same change.
- **macOS/VZ** (`engram-sandbox-vz`), **web** (`pnpm lint/build/test`), **orchestrator**
  (`bun run typecheck` / `bun test`).
- Gotchas: `SQLX_OFFLINE=true` in CI — keep the `.sqlx/` query cache in sync. **Doc-tests
  are intentionally out of scope** — don't add `cargo test --doc`.
- **Dead-public audit** (occasional, not in CI): `hawk.toml` configures
  [astral-sh/hawk](https://github.com/astral-sh/hawk), which finds unused / over-public
  `pub` items. Run it with hawk's own pinned toolchain (`cargo +<pin> hawk check`), once
  per cfg world — host (macOS) and `--target aarch64-unknown-linux-musl` (inside
  `nix develop`, for the bindgen kernel headers) — and only act on findings present in
  **both** runs; a single-platform finding usually has consumers behind the other
  platform's `cfg`. Hawk does not see `#[cfg(test)]` consumers: confirm each deletion
  with `cargo check --workspace --all-targets` on both targets.

## Change-scoped validation

Use the smallest local gate that covers the changed paths. The current-head `CI Gate` is
the final full-repository result.

- **Docs only**: run `git diff --check`. Run a document-specific formatter or validator
  when the changed format has one.
- **Web only**: from `web/`, run `pnpm format:check`, `pnpm lint`, focused `pnpm test`
  targets, and `pnpm build`.
- **Orchestrator only**: from `orchestrator/`, run `bun run typecheck` and the focused
  `bun test` targets. Run live-Postgres tests only when the changed behavior needs them.
- **Rust or shared backend**: use per-crate checks and tests while editing, then run
  `just check` once before the PR. Add the target-specific checks required elsewhere in
  this file when the changed code is platform-gated.
- **Mixed changes**: take the union of the applicable gates. Do not add Rust checks only
  because a PR also changes web or orchestrator code.
- **Conflict-only rebase**: run the formatter and type checker for each affected package,
  plus a focused regression test when the resolution changes behavior. Push promptly and
  use current-head CI as the complete gate.
- **Workflow changes**: validate the YAML and run `actionlint` when available. They remain
  outside `just check`.

Run each package formatter before every push, including a conflict-only rebase. Do not wait
for an unrelated local lane after the affected gates pass.

## CI

`.github/workflows/ci.yml` is the **single** CI workflow (Linux + macOS/VZ + the e2e
stack all live here — there is no separate macOS workflow), **path-gated** by
`.github/scripts/detect-rebake-lanes.py` (a cargo-dep-closure detector): only the lanes a
change can affect run (orchestrator-only → only orchestrator; web-only → only web;
a coordinator-crate change → Rust lanes incl. macOS/VZ + firecracker + e2e). `bake-images.yml`
bakes only the changed images. A CI-workflow or detector change re-runs everything.

**The only required status check is the aggregator `CI Gate`** — it always runs, `needs:`
EVERY lane (Linux, macOS, firecracker, AND the e2e stack), and passes iff each lane
succeeded-or-skipped. When you add a new lane, add it to the gate's `needs:` (and give it a
detector flag) — **never add an individual lane as a required check**, or a path-skipped lane
will wedge the merge queue. **Admin mass-merges skip combined-state validation** —
each PR was green in isolation, not together (2026-07-16: two PRs adding the same
workspace dep auto-merged into a duplicate TOML key, and a test PR + an
invariant PR collided on a contract — main broke twice in one day). Land batches
through the merge queue, or tight-burst them only after a combined local check.
`.github/**` is outside `just check`: validate workflow YAML
(`yaml.safe_load` / actionlint) and prefer `run: |` block scalars. Keep the `merge_group`
trigger in the required workflow.

## Multi-PR and multi-agent work

One coordinating agent owns the Git graph for a multi-PR effort. Before implementation,
it records a stack manifest with each issue, branch, base, dependency, migration number or
`none`, shared file, and required focused gate.

- Use real stacked bases when several dependent PRs must stay clean at the same time. If
  every PR targets `main`, keep only the next PR in merge order green. After it merges,
  rebase its immediate successor. Do not rebase every descendant after every merge.
- Reserve and record migration numbers before parallel work starts. The coordinating agent
  verifies the migration chain and is the only agent that changes stack bases or migration
  order.
- Before a force-push, verify the latest base SHA, the remote branch lease, and queued
  predecessor merges. A force-push can dismiss approvals and remove a merge-queue entry;
  report both effects after the push.
- Use one implementation session per issue. Reuse that session for review fixes. Do not
  create separate review, rereview, and final-review sessions when the Engrams review is
  already active.
- Treat only the current PR head as live. Ignore superseded CI and stale review findings.
  Create a fix task only for a valid current-head finding.
- An implementation session finishes after it opens the PR and its focused local gates
  pass. One coordinating monitor owns current-head CI, Engrams review, and merge-order
  updates for the complete stack.
- Require a concise progress update from a child session at least every 15 minutes. If it
  has no useful event for 30 minutes, interrupt it and inspect the blocker before retrying
  or replacing it.

## Conventions

**Writing**
- Adhere to **ASD-STE100** (Simplified Technical English) in all communications, including
  written artifacts (ADRs, commit messages, PR descriptions), code comments, and messages
  with the user. The product ships the same rule to every session
  (`WRITING_STYLE_SYSTEM_PROMPT` in `orchestrator/src/prompts/base.ts`).

**Commits & ADRs**
- One logical change per commit; don't bundle unrelated changes.
- **ADRs are for large architectural changes only** — a new crate/service, a new backend or
  trait seam, a schema / wire-protocol / on-disk-format change, a cross-cutting invariant, or
  a decision that later work has to build on. A feature, a fix, a refactor, a test, or a
  tooling change does **not** get one: the commit message and the PR description carry the
  reasoning. When in doubt, ship without one — an ADR nobody needed is worse than a good
  commit message, because it dilutes the set future readers have to grep.
- **When an ADR is warranted it gets bookends**: author it (`Proposed`) before code, update it
  between phases with divergences/pitfalls, flip to `Accepted` at the end with the commit
  chain. ADRs are numbered sequentially and are the record for non-obvious decisions.

**Code philosophy**
- **Clean breaks over compat shims.** Favor simple one-path code; backwards-incompatible
  changes (image re-bakes, schema changes) are acceptable. No "deprecated" stubs, no dangling
  references to old versions.
- **Simplify via abstractions** — a refactor should *retire* code; a new trait should subsume
  existing surfaces, not sit alongside them.
- **New crate over stuffing** — shared logic that deserves its own crate gets one (workspace
  member + workspace deps + `just hakari`); don't wedge it into a convenient crate.
- **Reliability and low latency are non-negotiable** — never trade them for transitional
  convenience; refuse "skip the work if it looks empty" shortcuts.
- **Never launder types/lints to silence the checker.** `as unknown as X` (TS), a blanket
  `@ts-ignore`/`@ts-expect-error`, or an `#[allow(...)]` slapped on to mute an error hides
  real bugs — a type mismatch is the compiler telling you the runtime shapes don't line up.
  Fix the shape instead: use the runtime-correct API, or *honestly widen* a type for a
  real-but-untyped field (e.g. `RequestInit & { duplex?: "half" }`). A plain `as` is only OK
  when you can state why it's sound. (ADR 0064 P2b: an `as unknown as ReadableStream` masked
  that Bun's `http.request` ignores `createConnection` — the proxy only worked once
  re-architected onto `fetch` + a loopback socket.)

**Determinism (ADR 0098)**
- **Time and randomness are injected world inputs.** In coordinator/postgres code,
  never call `Utc::now()`, `Instant::now()`, or `Uuid::new_v4()` — read
  `services.clock` / `services.entropy` (clippy `disallowed-methods` makes a raw
  call a hard error). PostgresStore never uses SQL `now()`; it binds the injected
  clock. Test modules that drive a live system carry ONE scoped
  `#[allow(clippy::disallowed_methods)]` with the standard justification comment.
- **Keep the `spawn()`/`run_once()` split** in every background driver: the timer
  loop is a thin wrapper, `run_once(&cfg, &state)` is the pure step the tests and
  the simulator drive directly. A driver whose logic lives in its loop is
  unsimulable.
- **Targeted invariants** (ADR 0099 H6): `engram_core::invariant!` (always-on
  panic — safe because pods are stateless over PG and host machines are
  redrive-safe) for corruption-class violations; `soft_invariant!` (log + skip)
  inside reconcilers, whose job is repairing what they detect. The site list is
  deliberate — each addition gets a line in ADR 0099.

**Discipline**
- **Investigate, never paper over.** Read the production code before changing a test
  assertion; a failing test usually means a real bug. Flakes / dead code / stale comments
  noticed mid-task get a follow-up commit, not a workaround.
- **Research over guess-and-check** — for protocol/API behavior look up the spec; for
  non-obvious design, survey prior art / comparable OSS before committing.
- **New tests must run in CI** — trace the workflow to confirm a new test is actually invoked,
  and that any env/services it needs are satisfied there.

**Patterns that recur here**
- **State machine over sync orchestration**: for multi-host / multi-stage lifecycle work,
  prefer "mark state in PG + a scanner drives transitions" over doing it all in one handler
  (survives restarts/evictions).
- **PG leasing row over advisory lock**: for cross-pod one-at-a-time-per-session enforcement,
  use `INSERT … ON CONFLICT DO NOTHING` + `DELETE` (connection-pool friendly, operator-visible),
  not `pg_try_advisory_lock`.

**Platform**
- **Apple frameworks from Rust** via `objc2` bindings — don't reach for a Swift driver.
- **Linux-only code is invisible to macOS clippy.** Cross-check `cfg(target_os = "linux")`
  paths with `cargo clippy --target aarch64-unknown-linux-musl -p <crate> --all-targets`
  (needs Linux UAPI headers).
- Keep arch/capability **detection above** cfg-gated binaries (in the orchestrator/script),
  not inside a leaf binary whose variants are themselves `cfg(target_os=…)`-gated.

**Cargo / inner loop**
- Don't run workspace-scale cargo (`--workspace` / `just check`) mid-edit — it queues behind
  rust-analyzer's background check. Per-crate `cargo check -p X` stays responsive.
- `cargo -p X -p Y` invalidates the `--workspace` cache (feature unification differs); use
  `--workspace -E 'package(X)|package(Y)'` for targeted runs that coexist with `just check`.
- Run long cargo in the background to a log file and `Read` it — don't `| tail` (it buffers
  and hides "Blocking waiting for file lock"). Validate in its own command; never chain a
  commit to a piped validation (the pipe masks the exit code).

## Gotchas

- **Applied migrations are checksum-immutable.** Never edit a migration that's already been
  applied (even a comment) — `sqlx` embeds checksums and the coordinator crashes on boot with
  a version mismatch. Add a new migration instead.
- The coordinator runs migrations at boot; the orchestrator applies its own via the
  `drizzle-orm` migrator (not the `drizzle-kit` CLI — that's a devDependency stripped from the
  runtime image).

## ADRs

`docs/adr/NNNN-title.md`, each with a `Status:` line (`Proposed` → `Accepted`, or
`Superseded by …`). Read these first to ground yourself:

- **0007** chunked-immutable storage (the durability primitive under all snapshots/dedup)
- **0011** the `HostClient` / `SandboxBackend` seam · **0013** stateless dial-on-demand transport
- **0015** system-design v2 (the typed `SessionState` machine) · **0024** unified dev orchestration
- **0028** eviction durability under host roll · **0034** idle-eviction state machine
- **0025** owning the FC guest kernel · **0044** the Kubernetes host fleet
- **0098** deterministic simulation testing (the clock/entropy seam, engram-sim,
  the conformance rule) · **0099** correctness hardening (per-test PG isolation,
  proptest, fault injection, invariants)
- **0051** the TypeScript orchestration tier · **0003** the VZ backend · **0006** the egress proxy
- **0066** the vsock port relay · **0118** session apps (named ports, reserved hostnames,
  and the orchestrator-enforced login wall that replaces IAP on preview hosts)

When in doubt about *why* something is shaped the way it is, grep `docs/adr/` before assuming.
