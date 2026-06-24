# CLAUDE.md

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

## The tiers (request → execution)

1. **web/** — React/Vite dashboard. Talks Connect/gRPC + SSE to the orchestrator.
2. **orchestrator/** — Bun + Hono TypeScript app tier (ADR 0051): human auth
   (better-auth), the task model, and authorization. The **only** web backend; it
   fronts the coordinator over app-gRPC. (Drizzle ORM, its own Postgres DB.)
3. **engram-coordinator** (Rust, axum + gRPC) — the control plane: session lifecycle,
   scheduling, idle eviction, the append-only **session event log**, GC. Stateless;
   **Postgres is the authority**; replicas coordinate via `LISTEN/NOTIFY`. Hosts
   **dial in** over WebSocket (NAT-friendly, ADR 0013).
4. **engram-host-agent** (Rust, per-host daemon) — wraps a `SandboxBackend` + the
   harness hub + chunked-OCI cache + NBD/UFFD (FC) + the egress proxy. Boots the VM.
5. **In-guest** — `engram-agentd` (PID 1 control surface over vsock/virtio-console)
   supervises a **harness** (`engram-harness-claude`) that drives the agent CLI and
   reports events back up.

## Repo map

**Rust workspace** (`crates/`, ~36 members). Key binaries:
`engram-coordinator`, `engram-host-agent`, `engram-host-operator` (K8s rollout, ADR
0044), `engram-uffd-handler` (FC snapshot page-faulting, Linux-only), `engram-agentd`,
`engram-cli` (admin CLI → coordinator app-gRPC), `engram-harness-claude`,
`engram-image-builder`. Notable libraries: `engram-core` (shared traits:
`SandboxBackend`, `HarnessHub`, `BlobStorage`, `MetadataStore`, `GitForge`, …),
`engram-protocol` (control-plane wire types), `engram-sandbox-{firecracker,vz,process}`,
`engram-chunk-store` (content-addressed storage, ADR 0007), `engram-postgres`,
`engram-crypto`, `engram-egress-proxy`, `engram-transport`, `engram-telemetry`.

**Other top-level:** `orchestrator/` (Bun), `web/` (pnpm), `deploy/` (helm,
migrations, packer, dev/, otel, bundles, kernel), `docker/` (per-image Dockerfiles),
`docs/` (`adr/`, `runbooks/`, `history.md`, `known-issues.md`), `third_party/firecracker`
(vendored fork), `.github/workflows/`.

## Daily commands (justfile)

- `just dev` — `tilt up`: the whole stack (docker-compose deps + coordinator + host-agent
  + web). Backend is **auto-detected** (`deploy/dev/detect-backend.sh`, ADR 0024):
  `/dev/kvm` → Firecracker, macOS+arm64 → VZ, else → Process. Tilt UI at `:10350`.
  `just dev-down` to stop. First-time: `just bootstrap` (KEK) then `just pull-kernel`.
- `just check` — **the pre-commit gate**: `cargo fmt --check`, `cargo clippy -D warnings`,
  `cargo hakari verify`, `cargo nextest run --workspace`. Run before every commit; CI
  enforces the same. (After adding/removing workspace deps, run `just hakari`.)
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
- **Firecracker integration** (`crates/engram-sandbox-firecracker/tests/`): `#[ignore]`'d,
  Linux+KVM only; run in CI on KVM runners. New FC/NBD regression tests **must** be wired
  into `ci.yml`'s `--test` list (not gated as local-only) or they never run.
- **e2e stack** (`test-e2e-stack`): boots the full prod-shape stack — the only lane that
  exercises the coordinator HTTP/gRPC path end to end. Never delete e2e coverage without a
  replacement landing in the same change.
- **macOS/VZ** (`engram-sandbox-vz`), **web** (`pnpm lint/build/test`), **orchestrator**
  (`bun run typecheck` / `bun test`).
- Gotchas: `SQLX_OFFLINE=true` in CI — keep the `.sqlx/` query cache in sync. **Doc-tests
  are intentionally out of scope** — don't add `cargo test --doc`.

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
will wedge the merge queue. `.github/**` is outside `just check`: validate workflow YAML
(`yaml.safe_load` / actionlint) and prefer `run: |` block scalars. Keep the `merge_group`
trigger in the required workflow.

## Conventions

**Commits & ADRs**
- One logical change per commit; don't bundle unrelated changes.
- Substantive work gets **ADR bookends**: author the ADR (`Proposed`) before code, update it
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
- **0051** the TypeScript orchestration tier · **0003** the VZ backend · **0006** the egress proxy

When in doubt about *why* something is shaped the way it is, grep `docs/adr/` before assuming.
