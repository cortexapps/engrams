# External MCP validation baseline

This records the primary development setup before external MCP changes on
2026-11-01. These are inherited preflight results, not new measurements of the
worker change. They are retained so later checks do not hide an existing failure.
The feature must stay disabled until the release gates pass.

| Surface                         | Recorded result                    | Evidence boundary                                        |
| ------------------------------- | ---------------------------------- | -------------------------------------------------------- |
| Orchestrator without database   | 2,427 passed, 219 skipped          | Source tests without live Postgres                       |
| Orchestrator with full Postgres | 2,619 passed, 18 skipped, 1 failed | Full local database suite                                |
| Web                             | 1,069 passed; build passed         | Local source tests and production build                  |
| Native backends                 | Builds passed; VZ demo passed      | Local native build and VZ demonstration                  |
| Firecracker production          | Not established                    | No real production run; fork runners are not provisioned |

The full Postgres suite failed in `sandbox.test.ts`: the deep-recursion case
expected a stack error (`StackError`, `InternalError`, or `RangeError`), but
observed `SandboxError`. The
focused file then passed all 15 tests. A 100-repeat recursion probe observed
`StackError`. This is an unrelated known baseline issue. Keep its assertion and
suite intact; do not use the focused pass to describe the full suite as green.

## Verification on 2026-11-08

At source `c8154095`, the unchanged sandbox file passed 15 tests in one run
with Bun 1.4.4. One complete orchestrator run then passed 3,248 tests, skipped
18, and failed none across 300 files. Its command was
`bun --no-env-file --no-install test`, with no filter or concurrency override.
The deep-recursion assertion passed in that full run. The tracked orchestrator
files did not change during the run.

The full run used two new isolated databases on the owned loopback test
cluster: one application database and one separate DBOS test database. The
existing helper verified both identities before migration or test writes.
Only those new databases were migrated or used for this check. Both were
removed after the test process and its children stopped.

This is no reproduction of the earlier failure, not a repair claim. The
original failure remains in the baseline above, and its assertion and suite
are unchanged. The local Node version was 24.13.0; CI pins 24.22.2. This local
result does not establish current-head remote CI, provider behavior, or physical
Firecracker acceptance.

The new `mcp-worker` package uses Node 24. Its deterministic local fixture has
real TLS and the pinned official MCP client. It covers modern and legacy
Streamable HTTP, JSON and SSE, catalog pages and schema drift, read/write
counters, held calls, transport and notification cancellation, hostile result
bodies, and OAuth metadata, PKCE, code replay, and refresh rotation. Its private
CA and loopback transport adapter are test-only. Local fixture success does not
prove isolation, live provider behavior, or production Firecracker acceptance.

Run the package gates from the repository root:

```sh
npm ci --prefix mcp-worker --ignore-scripts
npm run format:check --prefix mcp-worker
npm run typecheck --prefix mcp-worker
npm test --prefix mcp-worker
npm run build --prefix mcp-worker
```

The current worker CI job runs these commands, is selected by the path detector,
and participates in the required `CI Gate`. Workflow wiring does not establish
a passing current-head result or enable production dispatch.

## Unreleased migration numbering

Upstream uses migrations 0126–0128 for session-disk storage. The unreleased MCP
migrations use 0129–0138, in their original order with unchanged SQL checksums.
The migration-version test rejects duplicate numbers after later merges.

An earlier development build applied the MCP migrations as 0126–0135. A database
with those versions is not compatible with the merged build. Keep it unchanged;
do not edit its migration ledger or bypass checksum validation. Use a fresh
isolated PostgreSQL cluster for acceptance. A new database on the old cluster is
insufficient because testkit cleanup can remove older test databases and templates.
This development reset is not a data-preserving upgrade path for a deployed MCP
installation. Such an installation requires a separate migration plan.
