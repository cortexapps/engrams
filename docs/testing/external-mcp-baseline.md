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
expected a QuickJS `SandboxError`, but observed a stack error mismatch. The
focused file then passed all 15 tests. A 100-repeat recursion probe observed
`StackError`. This is an unrelated known baseline issue. Keep its assertion and
suite intact; do not use the focused pass to describe the full suite as green.

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

The rollout/CI task must wire these commands into the path detector and required
`CI Gate` dependency graph before the feature ships. The fixture foundation does
not modify the workflow or enable any production dispatch path.
