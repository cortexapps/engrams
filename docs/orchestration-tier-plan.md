# ADR 0039 Implementation Plan — TypeScript Orchestration Tier

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development
> (recommended) or superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking.

**Written against [ADR 0039](adr/0039-typescript-orchestration-tier.md) as
revised 2026-06-10** (authz in the orchestrator, AWS-style control plane,
task model). This plan supersedes the 2026-06-09 version (commit `727c380`),
which targeted the pre-revision ADR.

**Goal:** Split the web-application layer out of the Rust coordinator into a
TypeScript orchestration tier, leaving user-observable session behavior
unchanged (verified by a Playwright characterization net written first, plus
visual screenshot validation at every UI-touching task).

**Architecture:** Three tiers. The Rust coordinator becomes an AWS-style raw
resource API — tonic gRPC, **ServiceBearer machine auth only, no users, no
per-user authz** — plus a KEK-sealed `SecretService`. The new `orchestrator/`
(Hono on Node) owns human auth (better-auth + its **admin plugin** for roles),
**all authorization** (a CASL ability + a policy gate on the generic Connect
passthrough), and the **task model** (`task`/`task_session`; TaskService
implemented natively). The browser keeps SSE for events and WebSocket for the
shell, both terminated at the orchestrator; the UI's primary list becomes
tasks.

**Tech Stack:** Rust (tonic 0.12 / prost 0.13), Buf, TypeScript on Node ≥ 22
(Hono, `@hono/node-server` ≥ 1.13, `@hono/node-ws`, `@connectrpc/connect` v2 +
`@connectrpc/connect-node`, `@bufbuild/protobuf` v2, better-auth + admin
plugin, **@casl/ability**, drizzle-orm, Postgres), React
(`@connectrpc/connect-query` v2), Playwright.

**This plan's predecessor was adversarially reviewed task-by-task; the
corrections that survive the ADR revision are kept inline** (marked
"verified"). Trust the compiler over the plan if they disagree.

---

## Decisions this plan encodes (from the revised ADR)

1. **Control plane sheds identity entirely** (ADR §2.1/§5/§6): no `users`
   table, no `sessions.user_id` (made nullable mid-migration, dropped at
   cutover), no `require_admin`/`require_session_owner` on the gRPC path, no
   JWKS/forward-auth. The app-gRPC surface authenticates a **static service
   bearer** and trusts the caller fully.
2. **Orchestrator owns authz** (ADR §6): CASL ability (one file, shared with
   React), roles from better-auth's admin plugin, ownership via the task
   join. The passthrough gains a per-method **policy gate** that fails closed.
3. **Task model** (ADR §3): `task` + `task_session` in the orchestrator's
   Postgres; **TaskService is orchestrator-native** (same proto package,
   implemented on the Connect router, never proxied). The UI's "new session"
   becomes `CreateTask(type:'chat')`; the sessions list becomes a task list.
   Chat = degenerate task, no DBOS.
4. **Secrets** (ADR §2.3): `SecretService` (PutSecret/HasSecret/DeleteSecret,
   opaque keys, KEK-sealed in Rust) replaces `/me/claude-token` storage. The
   orchestrator keys entries by its user id and relays opaquely — it is **not**
   in the passthrough surface (the key must come from the session, never the
   client). Existing saved Claude tokens are **not migrated** — users re-save
   once (small user base; re-keying old coordinator user ids to better-auth
   ids isn't worth the machinery).
5. Carried from the previous round: Hono on Node; generic passthrough; SSE
   browser event leg with the hand-built `{idx, kind, payload_json}` envelope;
   `ShellRelayService` as its own service; protos at `proto/engram/app/v1/`;
   `Exec` and `GetArtifact` are streaming RPCs; event payload typing deferred
   (union pinned by a fixture contract test).
6. **`webhooks/`, `integrations/`, `workflows/` directories stay uncreated**
   (reserved for the DBOS fast-follows; YAGNI).

## Out of scope (fast-follows)

DBOS and workflows; Slack/Linear webhooks + the `connections` store; per-ticket
on-behalf-of attribution; typed event payloads / transcript shaping; team or
sharing semantics (the OpenFGA trigger, ADR §6); task types beyond `chat`;
per-type task status machines and the task activity table; full rolling-deploy
stream draining (basic SIGTERM-close only).

## Visual validation protocol (use throughout)

UI-touching tasks carry a **Visual check** step. The mechanism:

- `web/e2e/snap.spec.ts` (created in Task 2) navigates a comma-separated
  `SNAP_PATHS` env list using the suite's auth storageState and writes
  full-page screenshots to `web/e2e/__shots__/<slug>.png`.
  `just snap "/path1,/path2"` wraps it.
- **Phase 0 captures the baseline** into `web/e2e/__shots__/baseline/`
  (committed): the task/sessions list, a session detail in each tab
  (transcript/raw/shell), Fleet, Storage, Settings, Members, and the
  new-session form.
- At each Visual check, the executing agent re-snaps the affected paths and
  **reads both PNGs** (current vs baseline), verifying the named assertions in
  the step — e.g. "rows render with status chips, no unstyled fallback text,
  admin nav present for admin / absent for member." Visual diffs that are
  *intended* (e.g. the list becoming a task list, the login page existing)
  are named in the step; anything else is a regression — stop and fix.
- The shots directory (except `baseline/`) is gitignored.

## Stage smoke gates (`just smoke-*`)

Every phase ends with a runnable smoke against the live dev stack, layered so
each stage's gate exercises the deepest seam that exists so far. The merge
gate for any task = `just e2e` + every `smoke-*` recipe that exists at that
point. Umbrella: `just smoke` runs all of them in order.

| Recipe | Exists from | What it proves |
|---|---|---|
| `just e2e` | Task 2 | the user-observable journey (browser-black-box) |
| `just smoke-control-plane` | Task 10 | the tonic surface live: bearer accepted/rejected, create→list→get→stream-events→snapshot→delete on a no-harness image, secret put/has/delete (extends as Tasks 11–13 land — it is simply `cargo test -p engram-coordinator --test grpc_smoke -- --ignored` over the env-gated live tests, one recipe, one entry point) |
| `just smoke-orchestrator` | Task 18 | the orchestrator live, end to end: healthz+DB, sign-up/sign-in, the **authz matrix** (anon→401, member-on-other's-session→404, member→ListHosts 403, admin→200), CreateTask→SSE-event-flows→DeleteTask once Task 19/20 land (env-gated vitest file `orchestrator/src/smoke.live.test.ts`, recipe = `SMOKE=1 pnpm -C orchestrator vitest run smoke.live`) |
| `just smoke-parity` | end of Phase 3 | **the passthrough is faithful**: differential old-vs-new (below) |
| `just snap …` | Task 2 | visual regression (protocol above) |

**How we validate that things actually pass through** — three layers, each
catching what the previous can't:

1. **Reflection-driven conformance (in-process, every method, automatic).**
   The passthrough is generic, so its test must be too — a hand-written test
   per RPC would rot the moment the contract grows. Task 18's conformance
   test iterates `SURFACE` × `service.methods` and, for each method: builds a
   **fully-populated request** via schema reflection (every field set with
   deterministic values — see Task 18 Step 6), sends it through the
   orchestrator as an admin (so the policy gate passes and transport fidelity
   is tested orthogonally to authz), and asserts (a) the fake upstream
   received a **byte-identical** request (`toBinary` equality), (b) the
   client received the byte-identical fully-populated response the fake sent,
   (c) response headers/trailers survived, (d) the bearer was present.
   Server-streaming methods assert two messages arrive in order. A new RPC
   added to the proto is covered with zero new test code — and a field the
   filler can't populate or that doesn't round-trip fails loudly.
2. **Differential parity, old vs new (live, scaffolding — Phases 3–4 only).**
   Conformance proves the *orchestrator* relays faithfully; it cannot prove
   the *Rust cores + `convert.rs`* preserve today's semantics — that's where
   a mechanically-extracted core can silently default a field. While both
   stacks serve the same data (legacy axum REST until Task 32, new
   `/rpc` from Phase 3), `orchestrator/scripts/parity.ts` reads the same
   resources through **both** paths and diffs normalized JSON: ListSessions
   vs `GET /api/v1/sessions`, hosts, storage summary, enabled-images,
   registries, and per-session cow-state/checkpoints. Normalization:
   camelCase↔snake_case key folding, arrays sorted by id, a per-probe
   drop-list for volatile fields (ages, timestamps). Run via
   `just smoke-parity` with the dev stack up; **deleted in Task 28** when the
   REST side dies — it is scaffolding by design, and its value window is
   exactly the migration window. Streaming parity: subscribe the legacy SSE
   and the new SSE for the same session simultaneously and assert identical
   `idx` sequences + payloads for the first N events (same script).
3. **The e2e net + visual sweep (black-box).** Proves the composed system
   behaves identically where it matters — the browser — regardless of how
   the bytes arrived.

## Orchestrator file layout (target state at end of Phase 3)

```
orchestrator/
  package.json  tsconfig.json  drizzle.config.ts  vitest.config.ts
  src/
    index.ts                # entry: config, server, listen, SIGTERM close
    server.ts               # one Node http server: /rpc → Connect adapter, else → Hono
    config.ts               # env: ORCHESTRATOR_DATABASE_URL, CONTROL_PLANE_GRPC_URL,
                            #      CONTROL_PLANE_BEARER, ports
    auth/
      better-auth.ts        # better-auth + admin plugin (roles)
      iap.ts                # IAP trusted-SSO bridge (Task 29)
    authz/
      ability.ts            # THE CASL policy — one file, shared with React
      policy-map.ts         # method → {action, subject, extract} for the gate
      resolve.ts            # sessionId → task row (the ownership join), cached
    control-plane/
      transport.ts          # gRPC transport + service-bearer interceptor + h2 keepalives
      client.ts             # typed clients (sessions, shellRelay, secrets, images…)
    rpc/
      passthrough.ts        # generic forwarder + policy gate
      surface.ts            # allowlist + per-method policy entries
      tasks.ts              # TaskService — NATIVE implementation (not proxied)
    routes/
      events.ts             # SSE browser leg ⇄ upstream StreamEvents
      artifacts.ts          # HTTP byte route ⇄ upstream GetArtifact stream
      shell.ts              # WS browser leg ⇄ upstream ShellRelayService bidi
      me.ts                 # GET/POST /api/v1/me/claude-token → SecretService (opaque relay)
      health.ts
    db/
      schema.ts  client.ts  # drizzle: better-auth tables + task + task_session
    gen/                    # buf output — never hand-edited
```

## Sequencing

```
Phase 0 (e2e net + visual baseline, vs TODAY's stack)
   └─► Phase 1 (protos + codegen) ─► Phase 2 (coordinator: bearer + tonic surface)
                                  └► Phase 3 (orchestrator: authz, tasks, routes)
Phase 2 + 3 ─► Phase 4 (web migration; tasks UI; e2e re-pointed at better-auth)
Phase 4 ─► Phase 5 (scripts/CLI port → IAP → coordinator sheds users/auth → route removal)
```

- Phase 0 merges first. **Gate protocol:** `just e2e` runs manually before
  merging any Phase ≥ 1 task (needs a live stack; deliberately not in CI).
  In-flight UI redesign branches must carry the Phase 0 `data-testid`s
  forward — testids are the contract, components aren't.
- Phases 2 and 3 parallelize after Phase 1. Phase 3 tasks each have an
  **in-process fake-upstream test** (needs only Phase 1) and a **live
  Outcome** (needs Phase 2: Task 17 ← Task 9, Task 19/20 ← Tasks 10/12/13,
  Task 21 ← Task 11, Task 22 ← Task 13). Stop at the fake test when running
  in parallel.
- **Deploy posture:** Tasks 23–32 are a single non-deployable span for any
  production environment. Intermediate states are runnable **in dev only**:
  the coordinator keeps `AuthMode::None` + SyntheticAdmin on its legacy axum
  routes until Task 32, while the new gRPC surface runs bearer-auth from
  birth. Mid-phase, tasks created via the orchestrator and legacy
  axum-created sessions coexist; legacy ones render as *unattributed*
  (admin-only) in the new task list — expected, don't "fix" it.

Dev ports: web `:5173`, coordinator HTTP `127.0.0.1:8090`, Postgres
`localhost:5435`, **coordinator app-gRPC `127.0.0.1:50061` (new)**,
**orchestrator HTTP `127.0.0.1:8787` (new)**.

---

# Phase 0 — Characterization net + visual baseline (against the current stack)

Implements the ADR's **Preparation** section. Runs against `just dev` today
(SyntheticAdmin, no login); Task 23 later re-points only the entry step, and
Task 29 the precondition probe.

### Task 1: Playwright scaffold + precondition gate

**Goal:** `pnpm e2e` exists in `web/`, fails fast with a fix-it line per
missing precondition (web up, control plane healthy, a **no-harness demo
image** enabled).

**Outcome:** Stack down → exit non-zero with the `run \`just dev\` first`
fix-it. Stack up + preconditions met → Playwright's `Error: No tests found`
(exit 1 — expected until Task 2; verified globalSetup runs *before* the
no-tests check, so the gate is exercised either way).

**Files:**
- Modify: `web/package.json` (devDependency `@playwright/test`, script `"e2e": "playwright test"`)
- Create: `web/playwright.config.ts`, `web/e2e/global-setup.ts`
- Modify: `deploy/dev/integration-session.sh` (arch fix, Step 4)

**Steps:**

- [ ] **Step 1:** `cd web && pnpm add -D @playwright/test && pnpm exec playwright install chromium`
- [ ] **Step 2:** Create `web/playwright.config.ts`:

```ts
import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: './e2e',
  globalSetup: './e2e/global-setup.ts',
  // Generous: the journey's sequential assertion budgets (30+60+90+30s)
  // must fit inside this, or a slow boot dies as a generic test-timeout.
  timeout: 240_000,
  retries: 0,                  // characterization: flake is signal, not noise
  use: {
    baseURL: 'http://localhost:5173',
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    video: 'retain-on-failure',
  },
});
```

- [ ] **Step 3:** Create `web/e2e/global-setup.ts` — note the third check is a
  **no-harness** image (the journey must not require a Claude token):

```ts
// Preconditions for the e2e characterization net (ADR 0039 "Preparation").
// 1. web dev server up  2. control plane healthy  3. a no-harness demo
// image enabled. We deliberately do NOT auto-bake one (multi-minute build).
const WEB = 'http://localhost:5173';
const COORD = process.env.ENGRAM_COORDINATOR_URL ?? 'http://127.0.0.1:8090';

async function mustFetch(url: string, fixit: string): Promise<Response> {
  let res: Response;
  try {
    res = await fetch(url);
  } catch {
    throw new Error(`${url} not reachable — ${fixit}`);
  }
  if (!res.ok) throw new Error(`${url} returned ${res.status} — ${fixit}`);
  return res;
}

export default async function globalSetup() {
  await mustFetch(`${WEB}/`, 'run `just dev` first');
  await mustFetch(`${COORD}/healthz`, 'coordinator down/unhealthy — check Tilt (http://localhost:10350)');
  const res = await mustFetch(`${WEB}/api/v1/enabled-images`, 'run `just dev` first');
  // Shape: ListEnabledImagesResponse (web/src/types.ts:500) — { images: [...] }
  const body = (await res.json()) as { images?: { image_uri: string; harness_name: string | null }[] };
  if (!body.images?.some((i) => i.harness_name === null)) {
    throw new Error('no NO-HARNESS image enabled — run `just integration-session` once');
  }
}
```

- [ ] **Step 4:** Fix `deploy/dev/integration-session.sh` for macOS/arm64: it
  hardcodes `cargo build --target x86_64-unknown-linux-musl -p engram-agentd`
  (lines 52-53) — unbootable on the VZ/arm64 backend. Port the arch detection
  from `deploy/dev/bake-demo.sh:45-57`. Verify the baked image boots locally.
- [ ] **Step 5:** Run with the stack down → fix-it + exit 1. With the stack up →
  globalSetup passes, then `No tests found` (exit 1) — both expected.
- [ ] **Step 6:** Commit:

```bash
git add web/package.json web/pnpm-lock.yaml web/playwright.config.ts web/e2e/global-setup.ts deploy/dev/integration-session.sh
git commit -m "test(e2e): playwright scaffold + stack precondition gate (ADR 0039 prep)"
```

### Task 2: The journey spec + `data-testid`s + snap harness + visual baseline

**Goal:** Pin the ADR's single linear journey, and stand up the **visual
validation protocol**: a parameterized screenshot spec plus committed baseline
shots of every major view.

**Outcome:** `just e2e` passes against `just dev` on the no-harness demo
image; `just snap "/"` writes `web/e2e/__shots__/root.png`;
`web/e2e/__shots__/baseline/` holds committed baselines for: list, session
detail (transcript/raw/shell tabs), new-session form, Fleet, Storage,
Settings, Members.

**Files** (testids live where the elements actually render — verified):
- Modify: `web/src/pages/Sessions.tsx` — `data-testid="new-session"` on the
  new-session button.
- Modify: `web/src/components/NewSessionForm.tsx` —
  `data-testid="image-select"` on the image `<select>` (native select —
  Playwright can't click `<option>`s, use `selectOption`) and
  `data-testid="start-session"` on the submit button. **Caution:** a
  Claude-harness image with no saved token replaces the submit button with a
  Link (`NewSessionForm.tsx:260-269`) — selecting the demo image first avoids
  this.
- Modify: `web/src/components/SessionManifest.tsx` —
  `data-testid="session-row"` on each `SessionRow`.
- Modify: `web/src/components/TabRow.tsx` — `data-testid={`tab-${t.id}`}` on
  the tab buttons (SessionDetail's tabs render through this shared component).
- Modify: `web/src/pages/SessionDetail.tsx` — wrap `{events.length}`
  (`SessionDetail.tsx:81`) in its own
  `<span data-testid="event-count">{events.length}</span>` (tagging the
  surrounding prose makes `Number(textContent)` return `NaN` — the poll could
  never pass); `data-testid="session-status"` on the status chip;
  `data-testid="event-row"` on each raw event row.
- Create: `web/e2e/session-journey.spec.ts`, `web/e2e/snap.spec.ts`
- Modify: `justfile` (`e2e` + `snap` recipes), `web/.gitignore`
  (`e2e/__shots__/*` except `baseline/`)

**Steps:**

- [ ] **Step 1:** Add the testids. Inert attributes; `pnpm -C web test` still
  green.
- [ ] **Step 2:** Create `web/e2e/session-journey.spec.ts`:

```ts
import { test, expect } from '@playwright/test';

// The single linear journey from ADR 0039 "Preparation". Black-box at the
// browser: these assertions must hold identically before and after the
// orchestration-tier migration.
test('create a session and watch it come alive', async ({ page }) => {
  await page.goto('/');                                   // no login (SyntheticAdmin today)
  await page.getByTestId('new-session').click();

  // Explicitly select the no-harness demo image — images[0] is auto-selected
  // otherwise and may be a Claude image (token-gated submit) on dev boxes.
  const demoUri = await page
    .getByTestId('image-select')
    .locator('option')
    .filter({ hasText: /demo|integration/ })
    .first()
    .getAttribute('value');
  await page.getByTestId('image-select').selectOption(demoUri!);
  await page.getByTestId('start-session').click();

  await expect(page).toHaveURL(/\/sessions\/[0-9a-f-]+/, { timeout: 30_000 });
  const id = page.url().split('/sessions/')[1];

  // The stream is live: the event count climbs above zero.
  await expect
    .poll(async () => Number(await page.getByTestId('event-count').textContent()), {
      timeout: 60_000,
    })
    .toBeGreaterThan(0);

  // Status advances toward active.
  await expect(page.getByTestId('session-status')).toHaveText(/active|running/i, {
    timeout: 90_000,
  });

  // RAW tab renders event rows.
  await page.getByTestId('tab-raw').click();
  await expect(page.getByTestId('event-row').first()).toBeVisible();

  // SHELL tab's WebSocket connects. Predicate-filter (vite HMR also opens a
  // websocket); 'websocket' fires on creation, so also await a received
  // frame (ttyd handshakes promptly) to pin "connected".
  const wsPromise = page.waitForEvent('websocket', {
    predicate: (ws) => ws.url().includes('/shell'),
    timeout: 30_000,
  });
  await page.getByTestId('tab-shell').click();
  const ws = await wsPromise;
  await ws.waitForEvent('framereceived', { timeout: 15_000 });

  // The new row appears in the list. (Post-migration this list shows TASKS —
  // the testid survives the noun change; assert on testid, never copy.)
  await page.goto('/');
  await expect(page.getByTestId('session-row').filter({ hasText: id.slice(0, 8) })).toBeVisible();
});
```

  The demo-image filter and status regex are the two places reality may
  differ — adjust to what the UI renders, keeping the assertions intact.
- [ ] **Step 3:** Create `web/e2e/snap.spec.ts` — the vision harness:

```ts
import { test } from '@playwright/test';

// Screenshot harness for the visual validation protocol (plan header).
// SNAP_PATHS="/,/sessions/abc?tab=raw" pnpm exec playwright test snap
// Writes web/e2e/__shots__/<slug>.png; an agent then READS the images and
// compares against e2e/__shots__/baseline/.
const paths = (process.env.SNAP_PATHS ?? '/').split(',');

for (const p of paths) {
  const slug = p === '/' ? 'root' : p.replace(/[^a-z0-9]+/gi, '-').replace(/^-|-$/g, '');
  test(`snap ${p}`, async ({ page }) => {
    await page.goto(p);
    await page.waitForLoadState('networkidle');
    await page.screenshot({ path: `e2e/__shots__/${slug}.png`, fullPage: true });
  });
}
```

- [ ] **Step 4:** `justfile` recipes (near `integration-session`, ~:204):

```make
# ADR 0039 characterization net. Requires `just dev` and the no-harness
# demo image (`just integration-session` once). Run before merging any
# task of the ADR 0039 plan.
e2e:
    cd web && pnpm e2e

# Visual validation protocol: full-page screenshots of the given paths.
# Usage: just snap "/,/sessions/<id>"
snap paths="/":
    cd web && SNAP_PATHS="{{paths}}" pnpm exec playwright test snap --reporter=list
```

- [ ] **Step 5:** `just e2e` → PASS. Then capture the baseline: create a demo
  session, `just snap "/,/sessions/<id>,/fleet,/storage,/settings,/members"`,
  click into each tab variant (raw/shell) via `SNAP_PATHS` query params or a
  manual re-run, and copy the results into `web/e2e/__shots__/baseline/`.
  **Visual check (the protocol's first use):** read each baseline PNG and
  confirm it shows a fully rendered view (no spinners, no error states) —
  these are the reference images every later Visual check compares against.
- [ ] **Step 6:** Commit:

```bash
git add web/src web/e2e justfile web/.gitignore
git commit -m "test(e2e): journey spec + snap harness + visual baseline (ADR 0039 prep)"
```

---

# Phase 1 — The contract (protos + codegen)

### Task 3: Buf tooling — lint, breaking-change CI, `just gen-proto`

**Goal:** `buf` owns the proto tree; CI fails on breaking changes; one command
regenerates all TS bindings.

**Outcome:** `buf lint` and `buf breaking --against '.git#branch=main'` clean
locally; the CI job is green on the PR that adds it.

**Steps:**

- [ ] **Step 1:** Install buf (`brew install bufbuild/buf/buf`, or the Nix
  devshell if `flake.nix` provides it).
- [ ] **Step 2:** Create `crates/engram-protocol/proto/buf.yaml`. Verified:
  `host_service.proto` fails **PACKAGE_DIRECTORY_MATCH**,
  **RPC_REQUEST_RESPONSE_UNIQUE**, **RPC_REQUEST_STANDARD_NAME**, and
  **RPC_RESPONSE_STANDARD_NAME** under STANDARD — scope exceptions to the
  legacy file only:

```yaml
version: v2
lint:
  use:
    - STANDARD
  ignore_only:
    PACKAGE_DIRECTORY_MATCH:
      - host_service.proto
    RPC_REQUEST_RESPONSE_UNIQUE:
      - host_service.proto
    RPC_REQUEST_STANDARD_NAME:
      - host_service.proto
    RPC_RESPONSE_STANDARD_NAME:
      - host_service.proto
breaking:
  use:
    - FILE
```

- [ ] **Step 3:** `cd crates/engram-protocol/proto && buf lint` → clean.
- [ ] **Step 4:** Repo-root `buf.gen.yaml` (verified: `paths` values are
  cwd-relative and include the input-directory prefix):

```yaml
version: v2
inputs:
  - directory: crates/engram-protocol/proto
    paths:
      - crates/engram-protocol/proto/engram/app   # TS needs only the app contract
plugins:
  - remote: buf.build/bufbuild/es:v2.2.5
    out: web/src/gen
    opt: target=ts
  - remote: buf.build/bufbuild/es:v2.2.5
    out: orchestrator/src/gen
    opt: target=ts
  - remote: buf.build/connectrpc/query-es:v2.1.0
    out: web/src/gen
    opt: target=ts
```

- [ ] **Step 5:** `justfile`: `gen-proto:` → `buf generate`.
- [ ] **Step 6:** CI job (verified corrections: buf-action runs `buf format`
  by default and the legacy proto fails it — disable; `.git#branch=main`
  doesn't resolve on PR runners — use the action's default PR-base behavior):

```yaml
  buf:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: bufbuild/buf-action@v1
        with:
          input: crates/engram-protocol/proto
          format: false        # host_service.proto predates buf format
      - run: buf generate && git diff --exit-code -- web/src/gen orchestrator/src/gen
```

- [ ] **Step 7:** Exclude `src/gen` from web's eslint/vitest globs if picked up.
- [ ] **Step 8:** Commit:

```bash
git add crates/engram-protocol/proto/buf.yaml buf.gen.yaml justfile .github/workflows/
git commit -m "build(proto): buf lint + breaking-change CI + gen-proto recipe (ADR 0039 §7)"
```

### Task 4: `engram/app/v1/session.proto` — SessionService + ShellRelayService

**Goal:** The session contract (ADR §2.3), lint-clean under STANDARD (files at
`proto/engram/app/v1/`, every RPC with unique wrapped Request/Response).
**Revision deltas:** `ListSessions` has **no scope param** (trusted caller
gets everything; the orchestrator filters via tasks); `CreateSession` takes
`harness_secret_id` instead of resolving a principal's token.

**Files:**
- Create: `crates/engram-protocol/proto/engram/app/v1/session.proto`

**Steps:**

- [ ] **Step 1:** Create the file — service blocks complete, load-bearing
  messages complete, the rest transcribed in Step 2:

```proto
syntax = "proto3";

package engram.app.v1;

// The orchestrator-facing app contract (ADR 0039 §2.3, rev 2026-06-10).
// The caller is a single trusted service (bearer-authed); there is no
// per-user anything here. Attribution and authz live in the orchestrator.
service SessionService {
  // Returns ALL sessions. The orchestrator filters by task ownership.
  rpc ListSessions(ListSessionsRequest) returns (ListSessionsResponse);
  rpc CreateSession(CreateSessionRequest) returns (CreateSessionResponse);
  rpc GetSession(GetSessionRequest) returns (GetSessionResponse);
  rpc DeleteSession(DeleteSessionRequest) returns (DeleteSessionResponse);
  rpc SendPrompt(SendPromptRequest) returns (SendPromptResponse);
  rpc Interrupt(InterruptRequest) returns (InterruptResponse);
  rpc StreamEvents(StreamEventsRequest) returns (stream SessionEvent);
  // One streaming RPC; the unary HTTP route's semantics are the
  // degenerate collected case.
  rpc Exec(ExecRequest) returns (stream ExecOutput);
  rpc GetLog(GetLogRequest) returns (GetLogResponse);
  rpc Snapshot(SnapshotRequest) returns (SnapshotResponse);
  rpc Resume(ResumeRequest) returns (ResumeResponse);
  rpc EvictLocal(EvictLocalRequest) returns (EvictLocalResponse);
  rpc GetCowState(GetCowStateRequest) returns (GetCowStateResponse);
  rpc ListCheckpoints(ListCheckpointsRequest) returns (ListCheckpointsResponse);
  // STREAMING: artifacts run to 512 MiB (MAX_ARTIFACT_BYTES); a unary
  // response would blow the 4 MiB message cap. First message = metadata.
  rpc GetArtifact(GetArtifactRequest) returns (stream GetArtifactResponse);
  rpc CreateArtifactFromPath(CreateArtifactFromPathRequest) returns (CreateArtifactFromPathResponse);
}

// Browser-shell relay (ADR §8), its own service: keeps the bidi
// (browser-uncallable) method out of the UI surface and avoids colliding
// with host_service.proto's ProxyShell.
service ShellRelayService {
  rpc Relay(stream RelayShellRequest) returns (stream RelayShellResponse);
}

message ListSessionsRequest {}

message CreateSessionRequest {
  string image_uri = 1;
  string mode = 2;
  optional string prompt = 3;
  // Sealed-secret reference (SecretService key). When set and the image's
  // builtin harness wants a token, the control plane unseals + injects it.
  // Replaces "auto-inject the calling user's token" — there is no calling
  // user here (ADR §2.1).
  optional string harness_secret_id = 4;
  // ...overrides transcribed in Step 2
}

// The typed event ENVELOPE. payload_json is NORMATIVELY the JSON object
// today's SSE `data:` carries: the event payload with
// _recovery_epoch/_rewound folded in via with_rewind_meta
// (api/events.rs:144) — on BOTH the replay and live arms.
message SessionEvent {
  // optional: a broadcast-lag notification has no idx (today's SSE emits
  // `event: lagged` with NO id: line so it never disturbs Last-Event-ID).
  optional int64 idx = 1;
  // The event's "type" discriminant. Special value "lagged": idx unset,
  // payload_json = {"missed": n}.
  string kind = 2;
  string payload_json = 3;
}

message StreamEventsRequest {
  string session_id = 1;
  // Replay strictly-after this idx, then tail. UNSET = from the start
  // (callers translate the HTTP -1 sentinel to unset; proto3 default 0
  // would silently skip idx 0).
  optional int64 since = 2;
}

message GetArtifactRequest {
  string session_id = 1;
  string artifact_id = 2;
}

message GetArtifactResponse {
  oneof msg {
    ArtifactMetadata metadata = 1;  // always the first message
    bytes chunk = 2;
  }
}

message ArtifactMetadata {
  string media_type = 1;
  int64 size_bytes = 2;
  string file_name = 3;
}

// Frame kinds mirror the WS frames / host ProxyShellMessage 1:1. Request
// and response are separate types (RPC_REQUEST_RESPONSE_UNIQUE) and mark
// directionality.
message RelayShellRequest {
  oneof frame {
    ShellOpen open = 1;   // first client frame: which session's shell
    string text = 2;
    bytes binary = 3;
    bytes ping = 4;
    bytes pong = 5;
    ShellClose close = 6;
  }
}

message RelayShellResponse {
  oneof frame {
    string text = 1;
    bytes binary = 2;
    bytes ping = 3;
    bytes pong = 4;
    ShellClose close = 5;
  }
}

message ShellOpen {
  string session_id = 1;
}

message ShellClose {
  uint32 code = 1;
  string reason = 2;
}
```

- [ ] **Step 2:** Transcribe the remaining messages **field-for-field** from
  the sources below. Rules: same snake_case names; `T | null`/optional →
  `optional`; arrays → `repeated`; `Record<string,string>` →
  `map<string,string>`; string-literal unions → `string` (NOT enums);
  internally-tagged serde unions → `oneof`. Wrap entities in the response
  (`GetSessionResponse { Session session = 1; }`).

  | Proto message | Transcribe from |
  |---|---|
  | `Session`, `ListSessionsResponse` | `web/src/types.ts:70` (`Session`), `:84` (`SessionListItem` — fold extras in as `optional`), `:92`. **Omit `user_id`** — it leaves the contract (ADR §2.1) |
  | `CreateSessionRequest` overrides | `CreateSessionInput`, `web/src/api.ts:205` (`secrets` → `map<string,string>`) |
  | `CreateSessionResponse` | `web/src/types.ts:232` |
  | `SendPrompt*` / `Interrupt*` | serde structs at `api/prompt.rs:21/26`, `api/interrupt.rs:26` |
  | `ExecRequest`, `ExecOutput` | **`engram_core::types::ExecEvent`** (imported at `api/exec.rs:18`) is authoritative for the stream; `api/exec.rs:27/39` informs `ExecRequest` (`web/src/types.ts:242`'s `ExecRusage` is explicitly non-exhaustive) |
  | `GetLog*` | the `/sessions/:id/log` handler in `api/sessions_inspect.rs` |
  | `Snapshot/Resume/EvictLocal` | `api/snapshot.rs` handlers |
  | `GetCowStateResponse` | wraps `web/src/types.ts:217` (+ `:159`, `:194`) |
  | `ListCheckpointsResponse` | wraps `web/src/types.ts:370` + `:358` |
  | `CreateArtifactFromPath*` | the artifact-from-path handler (ADR 0026) |
  | `GetSessionRequest`/`DeleteSession*` | `{ string session_id = 1; }`; delete response mirrors `api/sessions.rs:1277` |

- [ ] **Step 3:** `buf lint` → clean (full STANDARD, no new exceptions). Commit:

```bash
git add crates/engram-protocol/proto/engram/app/v1/session.proto
git commit -m "feat(proto): app/v1 SessionService + ShellRelayService (ADR 0039 §2.3 rev)"
```

### Task 5: `fleet.proto`, `image.proto`, `secret.proto`, `task.proto`

**Goal:** The remaining contract files. **Revision deltas:** no `user.proto`
(users/roles are better-auth's); new **`secret.proto`** (control-plane sealed
store) and **`task.proto`** (orchestrator-NATIVE — same package for uniform
clients; the coordinator never implements it).

**Steps:**

- [ ] **Step 1:** `fleet.proto`:

```proto
syntax = "proto3";
package engram.app.v1;

service FleetService {
  rpc ListHosts(ListHostsRequest) returns (ListHostsResponse);
  rpc GetHost(GetHostRequest) returns (GetHostResponse);
  rpc GetHostCowState(GetHostCowStateRequest) returns (GetHostCowStateResponse);
  // Soft drain — the member-facing POST /hosts/:id/drain (status flip,
  // the one the web calls: web/src/api.ts:176).
  rpc DrainHost(DrainHostRequest) returns (DrainHostResponse);
  // ADR-0018 cordon+evacuate — POST /admin/hosts/:id/drain. Two distinct
  // semantics today; kept distinct.
  rpc AdminDrainHost(AdminDrainHostRequest) returns (AdminDrainHostResponse);
  rpc CordonHost(CordonHostRequest) returns (CordonHostResponse);
  rpc UncordonHost(UncordonHostRequest) returns (UncordonHostResponse);
  rpc GetStorageSummary(GetStorageSummaryRequest) returns (GetStorageSummaryResponse);
  rpc FlushSession(FlushSessionRequest) returns (FlushSessionResponse);
  rpc EvacuateSession(EvacuateSessionRequest) returns (EvacuateSessionResponse);
  rpc ChunkGc(ChunkGcRequest) returns (ChunkGcResponse);
  rpc BundleGc(BundleGcRequest) returns (BundleGcResponse);
  rpc SnapshotBlobGc(SnapshotBlobGcRequest) returns (SnapshotBlobGcResponse);
}
```

  GC requests carry `bool dry_run = 1`. Transcribe `HostView`
  (`web/src/types.ts:130`), `ListHostsResponse` (`:149`),
  `StorageSummaryResponse` (`:206`), `HostCowStateResponse`
  (`api/hosts.rs:67`); admin/GC bodies from `api/admin.rs`/`api/hosts.rs`/
  `api/storage.rs`. **Intentional drops (record):**
  `GET /admin/chunk-gc/candidates` and `/admin/reap-materialize-dir` get no
  RPC — no web caller; Task 33 decides keep-behind-bearer vs delete.
- [ ] **Step 2:** `image.proto` — `ListEnabledImages`, `EnableImage`,
  `DisableImage`, `RefreshImage`, `ListEnableJobs`, `GetEnableJob`,
  `RetryEnableJob`, `ListRegistries`, `AddRegistry`, `DeleteRegistry`, wrapped
  Request/Response each. Sources: `web/src/types.ts` (`:455`, `:483`, `:496`,
  `:500`, `:411`, `:416`, `:428`, `:437`) + `api/enabled_images.rs`,
  `api/registries.rs`. **`AddRegistryAuth` (`types.ts:406`) is an
  internally-tagged serde union** → `oneof auth { ... }` with variant names
  from the Rust enum; the Task 13 convert layer maps it explicitly.
- [ ] **Step 3:** `secret.proto`:

```proto
syntax = "proto3";
package engram.app.v1;

// KEK-sealed opaque secret store (ADR §2.3). Keys are caller-supplied and
// never interpreted (the orchestrator uses its better-auth user ids).
// Replaces the storage half of /me/claude-token; sealing stays in Rust.
service SecretService {
  rpc PutSecret(PutSecretRequest) returns (PutSecretResponse);
  rpc HasSecret(HasSecretRequest) returns (HasSecretResponse);
  rpc DeleteSecret(DeleteSecretRequest) returns (DeleteSecretResponse);
}

message PutSecretRequest {
  string key = 1;
  string value = 2;   // plaintext in transit (TLS/private net); sealed at rest
}
message PutSecretResponse {}
message HasSecretRequest { string key = 1; }
message HasSecretResponse { bool exists = 1; }
message DeleteSecretRequest { string key = 1; }
message DeleteSecretResponse {}
```

- [ ] **Step 4:** `task.proto` — **implemented by the orchestrator only**
  (excluded from the Rust build in Task 6; it lives here so web gets one
  uniform generated API):

```proto
syntax = "proto3";
package engram.app.v1;

// The application aggregate root (ADR §3). ORCHESTRATOR-NATIVE: the
// control plane neither implements nor knows about tasks.
service TaskService {
  rpc CreateTask(CreateTaskRequest) returns (CreateTaskResponse);
  rpc ListTasks(ListTasksRequest) returns (ListTasksResponse);
  rpc GetTask(GetTaskRequest) returns (GetTaskResponse);
  rpc DeleteTask(DeleteTaskRequest) returns (DeleteTaskResponse);
}

message Task {
  string id = 1;
  string type = 2;            // 'chat' now; linear_issue/dependabot/incident later
  optional string title = 3;
  string status = 4;          // open | working | awaiting_review | done | failed
  optional string created_by_user_id = 5;
  string source_json = 6;     // type-specific trigger ref (jsonb passthrough)
  repeated TaskSessionRef sessions = 7;
  string created_at = 8;      // RFC3339
}

message TaskSessionRef {
  string session_id = 1;
  optional string role = 2;
  // Denormalized from the control plane at read time (state, image, …):
  optional Session session = 3;
}

message CreateTaskRequest {
  string type = 1;            // 'chat' is the only accepted value for now
  string image_uri = 2;
  optional string prompt = 3;
  optional string title = 4;
}
message CreateTaskResponse { Task task = 1; }
message ListTasksRequest {}
message ListTasksResponse { repeated Task tasks = 1; }
message GetTaskRequest { string task_id = 1; }
message GetTaskResponse { Task task = 1; }
message DeleteTaskRequest { string task_id = 1; }
message DeleteTaskResponse {}
```

  (`Task.sessions[].session` imports `Session` from `session.proto` — add the
  import. `ListTasksRequest` is empty: the server scopes by the *caller's*
  ability, never by a client-supplied filter.)
- [ ] **Step 5:** `buf lint` → clean. Commit:

```bash
git add crates/engram-protocol/proto/engram/app/v1/
git commit -m "feat(proto): app/v1 Fleet/Image/Secret services + native TaskService (ADR 0039 rev)"
```

### Task 6: Rust bindings for `engram.app.v1`

**Goal:** tonic types for the services the coordinator implements.
`task.proto` is **excluded** — the coordinator must not accrete task concepts.

**Steps:**

- [ ] **Step 1:** Extend `crates/engram-protocol/build.rs`:

```rust
let protos = [
    "proto/host_service.proto",
    "proto/engram/app/v1/session.proto",
    "proto/engram/app/v1/fleet.proto",
    "proto/engram/app/v1/image.proto",
    "proto/engram/app/v1/secret.proto",
    // task.proto is deliberately absent: orchestrator-native (ADR §3).
];
```

- [ ] **Step 2:** `src/app.rs` mirroring `src/grpc.rs:14`'s include pattern
  (`tonic::include_proto!("engram.app.v1");`), `pub mod app;` in `lib.rs`.
- [ ] **Step 3:** `cargo build -p engram-protocol` → success. Commit:

```bash
git add crates/engram-protocol/
git commit -m "feat(proto): rust tonic bindings for engram.app.v1 (task.proto excluded)"
```

### Task 7: TS bindings generation

**Goal:** `just gen-proto` emits committed bindings into
`web/src/gen/engram/app/v1/` and `orchestrator/src/gen/engram/app/v1/`
(all five protos, including task).

**Steps:**

- [ ] **Step 1:** `cd web && pnpm add @bufbuild/protobuf@^2 @connectrpc/connect@^2 @connectrpc/connect-query@^2`.
- [ ] **Step 2:** `just gen-proto` → `*_pb.ts` + `*-…_connectquery.ts` files in
  both gen dirs (the orchestrator package doesn't exist yet — inert until
  Task 14; committed so the CI drift gate covers it).
- [ ] **Step 3:** `pnpm -C web build` → clean (gen is additive). Commit:

```bash
git add web/src/gen orchestrator/src/gen web/package.json web/pnpm-lock.yaml
git commit -m "feat(proto): generated connect-es + connect-query bindings for app/v1"
```

---

# Phase 2 — Control-plane gRPC server (coordinator)

> Much smaller than the pre-revision plan: no JWKS, no VerifierChain, no
> per-user principal, no owner-check extraction. The surface trusts one
> bearer-authed caller. The axum routes keep their existing middleware
> untouched until Phase 5 deletes them.

### Task 8: tonic app-server scaffold

**Goal:** tonic serves on `APP_GRPC_ADDR` beside axum — SessionService,
ShellRelayService, FleetService, ImageService, SecretService as
`UNIMPLEMENTED` stubs — with graceful shutdown and HTTP/2 keepalives
(ADR §9.4) from day one.

**Outcome:** A Rust smoke test gets `Code::Unimplemented` from `ListSessions`;
`just e2e` still green.

**Steps:**

- [ ] **Step 1:** Clap arg in `main.rs` (pattern-match `bind_addr`, main.rs:22),
  threaded through `CoordinatorConfig`:

```rust
/// Address the orchestrator-facing app gRPC server binds to (ADR 0039).
#[arg(long, env = "APP_GRPC_ADDR", default_value = "127.0.0.1:50061")]
app_grpc_addr: std::net::SocketAddr,
```

- [ ] **Step 2:** `grpc_app/mod.rs`. **The shared handle is
  `SharedState = Arc<AppState>` (state.rs:373/:543) — `AppState` is not
  `Clone`:**

```rust
//! Orchestrator-facing app gRPC surface (ADR 0039 §2.3). Lives beside
//! the axum API during the migration; the axum web routes retire in
//! Phase 5.
use engram_protocol::app;
use tonic::{Request, Response, Status};

pub struct AppSessionService {
    pub state: crate::state::SharedState,
    pub auth: std::sync::Arc<BearerAuth>,   // Task 9
}

#[tonic::async_trait]
impl app::session_service_server::SessionService for AppSessionService {
    async fn list_sessions(
        &self,
        _req: Request<app::ListSessionsRequest>,
    ) -> Result<Response<app::ListSessionsResponse>, Status> {
        Err(Status::unimplemented("ADR 0039 phase 2"))
    }
    // ... a stub per RPC (the compiler enumerates them). Streaming RPCs
    // need their associated types, e.g.:
    // type StreamEventsStream = std::pin::Pin<Box<dyn tokio_stream::Stream<
    //     Item = Result<app::SessionEvent, Status>> + Send>>;
}
```

- [ ] **Step 3:** Spawn **inside `run_with_registry_and_local`** (lib.rs —
  `AppState` is built at lib.rs:132-135; `main.rs` never sees it), right after
  `let state = Arc::new(app)`, sharing the axum side's shutdown signal
  (lib.rs:369):

```rust
let app_grpc = tonic::transport::Server::builder()
    // ADR §9.4: keepalive PINGs so a dead orchestrator's streams are
    // detected and torn down (releases leases/subscriptions).
    .http2_keepalive_interval(Some(std::time::Duration::from_secs(20)))
    .http2_keepalive_timeout(Some(std::time::Duration::from_secs(10)))
    .add_service(app::session_service_server::SessionServiceServer::new(/* … */))
    // ... shell relay, fleet, image, secret
    .serve_with_shutdown(cfg.app_grpc_addr, shutdown_signal());
tokio::spawn(async move {
    if let Err(e) = app_grpc.await {
        tracing::error!(error = %e, "app gRPC server exited");
    }
});
```

- [ ] **Step 4:** Smoke test. `tests/api.rs` already builds a full `AppState`
  from mocks (`MockMetadataStore`, `MockCloud`, `ProcessBackend`,
  `InMemorySecretStore`) — but those helpers live inside that test binary and
  are **not importable**; extract a shared test-support builder or duplicate
  the minimal one in `tests/grpc_app.rs`. Serve on an ephemeral port, call
  `list_sessions`, assert `Code::Unimplemented`. (No `grpcurl` — tonic serves
  no reflection by default.)
- [ ] **Step 5:** `just check` + `just e2e` green. Commit:

```bash
git add crates/engram-coordinator/
git commit -m "feat(coordinator): tonic app-gRPC scaffold with keepalives + graceful shutdown (ADR 0039 §2)"
```

### Task 9: Service-bearer auth on the app surface

**Goal:** Every app-gRPC call authenticates a static bearer token —
machine identity for exactly one caller (ADR §5). **Deliberately a separate
credential** from the host-agents' `ENGRAM_AUTH_TOKENS` (`api/auth.rs` — do
not touch that file; it guards host ingest): different caller, different
blast radius, independently rotatable.

**Outcome:** Correct token → call proceeds; missing/wrong →
`unauthenticated`; **no tokens configured → all calls rejected (fail closed),
boot unaffected**.

**Files:**
- Create: `crates/engram-coordinator/src/grpc_app/auth.rs`
- Modify: `main.rs`/`config.rs` (`--app-grpc-tokens`, env `APP_GRPC_TOKENS`,
  comma-separated to allow rotation overlap)
- Modify: `Tiltfile` — generate/set a dev token in `coord_env` **and** export
  it for the orchestrator resource (`CONTROL_PLANE_BEARER`) **now**, so
  Phase 3/4 live checks need no later wiring task

**Steps:**

- [ ] **Step 1:** Failing tests first in `grpc_app/auth.rs`: right token →
  Ok; wrong/missing → `unauthenticated`; empty token set → `unauthenticated`
  (NOT accept-everything — note this is the opposite of `api/auth.rs`'s
  `accepts_anything()` dev posture, on purpose: this surface is born strict).
- [ ] **Step 2:** Implement:

```rust
//! Machine auth for the app-gRPC surface (ADR 0039 §5): one trusted
//! caller (the orchestrator), one static bearer, constant-time compare.
//! Fail closed: no configured tokens = reject everything.
pub struct BearerAuth {
    tokens: Vec<String>,   // >1 only during rotation overlap
}

impl BearerAuth {
    pub fn check<T>(&self, req: &tonic::Request<T>) -> Result<(), tonic::Status> {
        let presented = req
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| tonic::Status::unauthenticated("missing bearer token"))?;
        if self.tokens.iter().any(|t| constant_time_eq(t.as_bytes(), presented.as_bytes())) {
            Ok(())
        } else {
            Err(tonic::Status::unauthenticated("invalid bearer token"))
        }
    }
}
```

  (Use the same constant-time comparison `api/auth.rs` uses — read it for the
  helper, copy the call, leave the file alone.) Call `self.auth.check(&req)?`
  at the top of every RPC. This is sync — unlike the pre-revision JWT design,
  a plain check works fine per-RPC; don't bother with a tower layer.
- [ ] **Step 3:** Tests green; `just check` green. Commit:

```bash
git add crates/engram-coordinator/ Tiltfile
git commit -m "feat(coordinator): service-bearer auth on app-gRPC — fail-closed machine identity (ADR 0039 §5)"
```

### Task 10: SessionService — extract-and-delegate, the unary six

**Goal:** `ListSessions`, `CreateSession`, `GetSession`, `DeleteSession`,
`SendPrompt`, `Interrupt` over gRPC via transport-agnostic cores both axum and
tonic call. **Revision note:** the cores carry **no principal and no authz**
(the caller is trusted; authz happens in the orchestrator). The axum shims
keep their existing `CurrentUser`/middleware behavior untouched — where a
handler body used the principal (list scoping, `user_id` stamping), the core
takes the *data* as a plain parameter and the axum shim supplies it from its
extractor.

**Sizing note:** land as ~3 PRs: (a) list/get/delete + `into_status` +
converters, (b) `create_session` + the `user_id`-nullable migration,
(c) prompt + interrupt.

**Outcome:** gRPC integration test: create → list → get → delete end-to-end;
`just e2e` green (axum path unchanged by construction).

**Steps:**

- [ ] **Step 1:** `into_status` in `grpc_app/mod.rs` — **exhaustive** over
  `ApiError`'s 12 variants (`src/error.rs`): `NotFound→not_found`,
  `Forbidden→permission_denied`, `Unauthorized→unauthenticated`,
  `BadRequest→invalid_argument`, `Conflict→failed_precondition`,
  `Gone/HostLost→failed_precondition`, `Unavailable→unavailable`,
  `Unsupported→unimplemented`, `PayloadTooLarge/TooManyRequests→resource_exhausted`,
  rest → `internal`. Make `ApiError::slug()` `pub` and attach it as
  `engram-error-slug` Status metadata — the web distinguishes slugs sharing
  an HTTP code (e.g. `snapshot_invalidated` vs `host_lost`, both 410) and the
  orchestrator relays it.
- [ ] **Step 2:** Extraction template — `list_sessions` first. Real signatures
  (verified): handlers use the `CurrentUser` extractor; the query is
  `ListSessionsParams { scope: Option<String> }` (sessions.rs:1201). The core
  returns **all** sessions; the axum shim keeps today's principal-scoped
  filtering on its side:

```rust
// Transport-agnostic core: returns ALL sessions (the gRPC caller is a
// trusted service; filtering/authz is the orchestrator's job, ADR §6).
// The axum shim applies today's principal scoping before returning.
pub(crate) async fn list_sessions_core(
    state: &SharedState,
) -> Result<ListSessionsResponse, ApiError> { /* body, extractors unwrapped, scoping removed */ }
```

- [ ] **Step 3:** `create_session`: it is a metrics wrapper around
  `create_session_inner` (sessions.rs:497-534) — extract the core *inside*
  the wrapper so gRPC creations are counted. The core takes
  `owner: Option<String>` (axum passes the principal's id as today;
  **gRPC passes `None`**) and `harness_secret_id: Option<String>` (axum
  passes the legacy per-user lookup; gRPC passes the request field — actual
  unsealing lands in Task 13 with SecretService; until then gRPC create
  works for no-harness images). Ship a sqlx migration making
  **`sessions.user_id` nullable** (dropped entirely in Task 32).
- [ ] **Step 4:** `get_session` / `delete_session` / prompt / interrupt: their
  axum bodies contain no authz (it's route middleware — verified) so
  extraction is mechanical; prompt's auto-resume-on-idle lives in the core.
- [ ] **Step 5:** `convert.rs` mappers — dumb, total field copies; unit-test by
  round-tripping populated structs.
- [ ] **Step 6:** Integration tests follow the repo's live-test idiom
  (verified: `tests/*_live_pg.rs` are `#[ignore]`d AND env-gated with a
  graceful skip) — gate on an env var with the gRPC addr + bearer. House them
  all in **one test binary, `tests/grpc_smoke.rs`** — this is the
  `just smoke-control-plane` stage gate (see "Stage smoke gates"). Add the
  recipe now:

```make
# Live smoke of the app-gRPC surface. Requires `just dev` running.
# APP_GRPC_TOKENS' dev token is read from the same env Tilt sets.
smoke-control-plane:
    ENGRAM_SMOKE_GRPC=127.0.0.1:50061 cargo test -p engram-coordinator --test grpc_smoke -- --ignored --nocapture

# Umbrella stage gate. Each task that adds a smoke-* recipe appends it
# here (a just recipe can't reference recipes that don't exist yet).
smoke: smoke-control-plane
```

  Tasks 11–13 extend this same binary (stream-events replay/tail check,
  snapshot→evict→resume, secret put/has/delete, bearer-rejected check) rather
  than scattering new ones. The `smoke` umbrella grows in Task 18
  (`smoke-orchestrator`) and Task 21b (`smoke-parity`), and shrinks in
  Task 28 when parity is deleted.
- [ ] **Step 7:** `just check` + `just e2e` green. Commit per PR boundary.

### Task 11: `StreamEvents` server-stream

**Goal:** Replay-then-tail identical to the SSE handler (`api/events.rs:43`).
**Do NOT add auto-resume** — the events handler has never resumed idle
sessions (only prompt/shell/exec do); merely viewing an idle session must not
resurrect its microVM.

**Outcome:** `StreamEvents(id, since unset)` yields the same `idx` sequence as
the SSE feed; reopen with `since=last_idx` → no gap, no dupe.

**Steps:**

- [ ] **Step 1:** Extract the sequencing-sensitive core — existence check,
  **subscribe-before-query** (`state.events.subscribe(id)` at events.rs:67
  *before* the log query; rule at events.rs:15), replay (`REPLAY_LIMIT`
  = 1000):

```rust
pub(crate) async fn events_core(
    state: &SharedState,
    id: &str,
    since: Option<i64>,
) -> Result<(Vec<PersistedEvent>, broadcast::Receiver<IndexedEvent>), ApiError>
```

- [ ] **Step 2:** The tonic method maps both arms to `app::SessionEvent`,
  carrying three wire details (verified in events.rs):
  **`with_rewind_meta` (events.rs:144) applies to BOTH arms**; replay→live
  dedupe drops live events with `idx <= replay high water`
  (events.rs:98-101 — copy the exact rule); broadcast **`Lagged`**
  (events.rs:102-106) → `SessionEvent { kind: "lagged", idx: unset,
  payload_json: {"missed": n} }`.
- [ ] **Step 3:** Confirm RAII teardown (client disconnect → future drop →
  receiver drop) with a `tracing::debug!` in a guard's `Drop`.
- [ ] **Step 4:** Env-gated live test; `just check` green. Commit:

```bash
git add crates/engram-coordinator/
git commit -m "feat(coordinator): StreamEvents sharing the SSE replay+tail core (no auto-resume — matches today)"
```

### Task 12: Remaining SessionService RPCs + ShellRelay

**Goal:** `Exec` (streaming), `GetLog`, `Snapshot`, `Resume`, `EvictLocal`,
`GetCowState`, `ListCheckpoints`, `GetArtifact` (streaming),
`CreateArtifactFromPath`, and `ShellRelayService.Relay`.

**Sizing note:** three PRs — (a) `Exec` + `GetArtifact`, (b) the mechanical
rest, (c) `Relay`.

**Steps:**

- [ ] **Step 1:** `Exec`: **two distinct handlers** exist — `exec`
  (exec.rs:147, unary + rusage) and `exec_stream` (exec.rs:260). Verify
  they're unifiable behind one streaming core; if not, two cores, proto `Exec`
  uses the streaming one.
- [ ] **Step 2:** `GetArtifact`: source is `serve_artifact` (upload.rs:542),
  which *streams* up to 512 MiB. Emit `metadata` first, then 64 KiB `chunk`
  frames.
- [ ] **Step 3:** The mechanical six: cores + converters + unit tests.
- [ ] **Step 4:** `Relay`. Map of the real code (verified): the WS handler
  bridges `engram_core::types::shell::ShellFrame`
  (`{Text, Binary, Ping, Pong, Close}`, shell.rs:29) over
  `HostClient::proxy_shell`'s `ShellTunnel` channels — it never touches
  `ProxyShellMessage` (that lives below `HostClient`). Core:
  `ensure_active` (shells DO auto-resume, shell.rs:42) → `registry.get` →
  `acquire_shell` → `proxy_shell` → pump → `release_shell`. Preserve:
  no-sandbox-after-resume → `Conflict`/`failed_precondition`
  (shell.rs:46-53); **`acquire_shell` failure is non-fatal**
  (warn-and-continue, shell.rs:62-68). First inbound frame must be `open`
  (else `invalid_argument`). **Teardown under tonic is new code**: the WS
  handler releases after its bridge completes (shell.rs:94) and is never
  cancelled mid-bridge; tonic *does* drop the future on disconnect — wrap the
  lease in a guard whose `Drop` spawns the async release (`Drop` can't
  `await`), explicit release on the normal path.
- [ ] **Step 5:** Env-gated live tests (incl. open-Relay → type `echo hi` →
  output frames → drop client → lease-release debug log fires). `just check`
  + `just e2e` green. Commit per PR boundary.

### Task 13: Fleet/Image services + SecretService + harness injection

**Goal:** The remaining services. **No admin gating anywhere** — the caller is
trusted; gating moved to the orchestrator's policy gate (Task 18).

**Steps:**

- [ ] **Step 1:** FleetService cores + converters. `DrainHost` = the soft
  member-facing `hosts::drain`; `AdminDrainHost` = `admin::drain_host`
  (cordon+evacuate) — two RPCs, two handlers, don't conflate. GC RPCs take
  `dry_run`.
- [ ] **Step 2:** ImageService cores + converters; the `AddRegistryAuth`
  serde-enum ⇄ proto-`oneof` mapping is explicit in `convert.rs`.
- [ ] **Step 3:** `SecretService`: new sqlx migration —
  `sealed_secrets(key TEXT PRIMARY KEY, ciphertext BYTEA, created_at)`.
  Reuse the exact KEK sealing path the current `/me/claude-token` handler
  uses (`api/principal.rs:296` — read it; the sealing helper moves or is
  shared, the per-user row in `users` is NOT reused). `PutSecret` seals +
  upserts; never log the value (keep the handler's redaction discipline).
- [ ] **Step 4:** `CreateSession` harness injection: when
  `harness_secret_id` is set and the image's builtin harness wants a token,
  unseal from `sealed_secrets` and inject exactly as the legacy per-user path
  did (find the injection site via the ADR 0031 wiring in
  `create_session_inner`). Legacy axum create keeps the old per-user lookup
  until Task 32. Existing tokens are **not migrated** — users re-save
  (decision 4 in the header).
- [ ] **Step 5:** Tests: secret round-trip (put → has → create-with-injection
  → delete); converter round-trips. `just check` green. Commit per service.

---

# Phase 3 — The orchestrator service

> `orchestrator/` is a **standalone pnpm package** (verified: no root
> pnpm-workspace; `web/` is standalone too). Commit `orchestrator/pnpm-lock.yaml`.
> CI: Task 14 adds `pnpm -C orchestrator install --frozen-lockfile && pnpm -C orchestrator test && pnpm -C orchestrator typecheck`.

### Task 14: Scaffold — Hono on Node, config, health, SIGTERM, Tilt wiring

**Goal:** One Node HTTP server: `/rpc/*` → Connect adapter (empty routes),
everything else → Hono; `/healthz`; SIGTERM close; Tilt resource.

**Outcome:** `curl http://127.0.0.1:8787/healthz` → `200 {"ok":true}` under
`just dev`.

**Steps:**

- [ ] **Step 1:** `mkdir orchestrator && cd orchestrator && pnpm init`, then:

```bash
pnpm add hono @hono/node-server @hono/node-ws \
  @connectrpc/connect @connectrpc/connect-node @bufbuild/protobuf @casl/ability
pnpm add -D typescript tsx vitest @types/node
```

  Scripts: `"dev": "tsx watch src/index.ts"`, `"test": "vitest run"`,
  `"typecheck": "tsc --noEmit"`. Pin `@hono/node-server` ≥ 1.13 (wires
  client-disconnect → `c.req.raw.signal` abort; Task 21 depends on it).
- [ ] **Step 2:** `tsconfig.json` — load-bearing: the committed `src/gen` uses
  `.js`-suffixed relative imports + `@bufbuild/protobuf/codegenv1` subpaths
  (classic `node` resolution fails):

```jsonc
{
  "compilerOptions": {
    "target": "es2022",
    "module": "esnext",
    "moduleResolution": "bundler",
    "strict": true,
    "skipLibCheck": true,
    "types": ["node"]
  },
  "include": ["src"]
}
```

- [ ] **Step 3:** `src/config.ts` — one validated **singleton** (every later
  snippet imports `config`; don't mix factory and singleton styles):

```ts
export interface Config {
  port: number;                 // ORCHESTRATOR_PORT, default 8787
  databaseUrl: string;          // ORCHESTRATOR_DATABASE_URL (required from Task 15)
  controlPlaneGrpcUrl: string;  // CONTROL_PLANE_GRPC_URL, default http://127.0.0.1:50061
  controlPlaneBearer: string;   // CONTROL_PLANE_BEARER (required) — the Task 9 token
  trustedOrigins: string[];     // TRUSTED_ORIGINS, dev: http://localhost:5173
}

export function loadConfig(env: NodeJS.ProcessEnv = process.env): Config {
  /* read + validate; throw naming the missing var */
}
export const config: Config = loadConfig();
```

- [ ] **Step 4:** `src/server.ts` (options verified against connect-node v2):

```ts
import { createServer } from 'node:http';
import { getRequestListener } from '@hono/node-server';
import { connectNodeAdapter } from '@connectrpc/connect-node';
import type { ConnectRouter } from '@connectrpc/connect';
import type { Hono } from 'hono';

// One process, one port: Connect RPCs under /rpc, everything else
// (better-auth, SSE, WS upgrade, artifacts, health) is Hono. ADR §7.
export function buildServer(
  app: Hono,
  routes: (r: ConnectRouter) => void,
  contextValues?: Parameters<typeof connectNodeAdapter>[0]['contextValues'],
) {
  const rpc = connectNodeAdapter({ routes, requestPathPrefix: '/rpc', contextValues });
  const hono = getRequestListener(app.fetch);
  return createServer((req, res) =>
    req.url?.startsWith('/rpc/') ? rpc(req, res) : hono(req, res),
  );
}
```

- [ ] **Step 5:** `src/routes/health.ts` + `src/index.ts` (listen +
  `process.on('SIGTERM', () => server.close(() => process.exit(0)))`).
- [ ] **Step 6:** Vitest: stub app on an ephemeral port; `/healthz` 200;
  `/rpc/x` handled by the adapter, not Hono.
- [ ] **Step 7:** Tilt: mirror the web resource (Tiltfile:489 — including
  `pnpm install --silent &&` in `serve_cmd`, or fresh checkouts break);
  `resource_deps=['postgres']`; env: `CONTROL_PLANE_BEARER` from Task 9's
  Tilt wiring. `just dev` → healthz 200.
- [ ] **Step 8:** Add the orchestrator CI job. Commit:

```bash
git add orchestrator/ Tiltfile .github/workflows/
git commit -m "feat(orchestrator): scaffold — hono on node, /rpc connect seam, tilt + ci (ADR 0039)"
```

### Task 15: Orchestrator database + drizzle (incl. task tables)

**Goal:** Separate `engram_orchestrator` DB (ADR §10), drizzle-owned, with the
**task model schema from day one**.

**Outcome:** Migrations apply; `/healthz` includes a DB ping; fresh `just dev`
brings the schema up unattended.

**Steps:**

- [ ] **Step 1:** `pnpm add drizzle-orm pg && pnpm add -D drizzle-kit @types/pg`.
- [ ] **Step 2:** `drizzle.config.ts` (dialect postgresql, schema
  `src/db/schema.ts`, out `drizzle/`, url from env); `src/db/client.ts`
  (pg Pool + drizzle).
- [ ] **Step 3:** `src/db/schema.ts` — the task model (ADR §3; better-auth
  tables join in Task 16):

```ts
import { pgTable, text, jsonb, timestamp, primaryKey, index } from 'drizzle-orm/pg-core';

export const task = pgTable('task', {
  id: text('id').primaryKey(),                       // nanoid/uuid
  type: text('type').notNull(),                      // 'chat' only for now
  title: text('title'),
  status: text('status').notNull().default('open'),  // open|working|awaiting_review|done|failed
  createdByUserId: text('created_by_user_id'),       // better-auth user id; null = automation (future)
  source: jsonb('source'),                           // type-specific trigger ref
  workflowRunId: text('workflow_run_id'),            // DBOS run — null for chat (ADR §4)
  createdAt: timestamp('created_at').notNull().defaultNow(),
  updatedAt: timestamp('updated_at').notNull().defaultNow(),
});

export const taskSession = pgTable('task_session', {
  taskId: text('task_id').notNull().references(() => task.id, { onDelete: 'cascade' }),
  sessionId: text('session_id').notNull(),           // control-plane session id
  role: text('role'),                                // nullable until multi-session types exist
  createdAt: timestamp('created_at').notNull().defaultNow(),
}, (t) => [
  primaryKey({ columns: [t.taskId, t.sessionId] }),
  index('task_session_session_idx').on(t.sessionId), // the authz join (ADR §6) hits this
]);
```

- [ ] **Step 4:** Database creation, two paths (verified: the compose volume
  persists `just db-down`, so initdb alone is NOT enough): fresh machines get
  a `docker-entrypoint-initdb.d` script (`CREATE DATABASE engram_orchestrator;`);
  existing machines run
  `docker compose -f deploy/docker-compose.dev.yml exec postgres createdb -U engram engram_orchestrator`
  (ignore "already exists") — put it in the Tilt migrate resource's command.
- [ ] **Step 5:** Tilt one-shot
  `local_resource('orchestrator-migrate', cmd='… createdb …; cd orchestrator && pnpm drizzle-kit migrate', resource_deps=['postgres'])`;
  the `orchestrator` resource depends on it. DB ping in `/healthz`. Env-gated
  vitest against the local DB. Commit:

```bash
git add orchestrator/ deploy/ Tiltfile
git commit -m "feat(orchestrator): engram_orchestrator db via drizzle — task model schema (ADR 0039 §3/§10)"
```

### Task 16: better-auth + admin plugin (roles)

**Goal:** better-auth with the drizzle adapter and the **admin plugin** —
which owns the role (`admin`/`user`; we read `user` as "member"), ban, and
set-role APIs. **No JWT plugin, no JWKS** — nothing downstream consumes user
identity anymore (ADR §5).

**Outcome:** Sign-up → session cookie works; an admin-plugin
`setRole`-promoted user reads back `role: 'admin'` via `getSession`.

**Steps:**

- [ ] **Step 1:** `pnpm add better-auth`. `src/auth/better-auth.ts`:

```ts
import { betterAuth } from 'better-auth';
import { drizzleAdapter } from 'better-auth/adapters/drizzle';
import { admin } from 'better-auth/plugins';
import { db } from '../db/client';
import { config } from '../config';

export const auth = betterAuth({
  baseURL: `http://127.0.0.1:${config.port}`,
  // The browser reaches this through the vite proxy with
  // Origin: http://localhost:5173 — without trustedOrigins, better-auth
  // 403s every non-GET auth route (CSRF protection). Verified.
  trustedOrigins: config.trustedOrigins,
  database: drizzleAdapter(db, { provider: 'pg' }),
  // Dev/self-hosted door. NOTE: public sign-up = open registration.
  // Acceptable in dev only; production posture (disable sign-up /
  // allowlist) is decided in Task 23 — do not deploy past Phase 4
  // without it.
  emailAndPassword: { enabled: true },
  plugins: [
    admin(),   // role field ('admin'|'user'), setRole/ban/list APIs → Members UI
  ],
});
```

- [ ] **Step 2:** Generate schema: `pnpm dlx @better-auth/cli@latest generate`
  (needs the DB env vars — the config import is eager); merge into
  `src/db/schema.ts`; `pnpm drizzle-kit generate && pnpm drizzle-kit migrate`.
- [ ] **Step 3:** Mount: `app.on(['GET','POST'], '/api/auth/*', (c) => auth.handler(c.req.raw))`.
- [ ] **Step 4:** Dev admin bootstrap: a `just dev-admin` recipe that
  promotes a dev email:
  `psql $ORCHESTRATOR_DATABASE_URL -c "UPDATE \"user\" SET role='admin' WHERE email='dev@engram.local'"`.
  Nothing else ever makes a user admin — without this, every admin surface in
  Phase 4 is untestable.
- [ ] **Step 5:** Verify by curl (sign-up → cookie → getSession shows role);
  env-gated vitest round-trip. Commit:

```bash
git add orchestrator/ justfile
git commit -m "feat(orchestrator): better-auth + admin plugin — users and roles live here (ADR 0039 §5)"
```

### Task 17: Control-plane transport — service bearer + clients

**Goal:** The upstream gRPC transport with the static bearer and the ADR §9.4
HTTP/2 keepalives. Radically simpler than the pre-revision per-user JWT
design: no context keys, no per-request minting — one header, set once.

**Outcome:** Vitest (in-process Connect fake): bearer header lands on every
call. Live (needs Task 10): `sessions.listSessions({})` returns data.

**Steps:**

- [ ] **Step 1:** `src/control-plane/transport.ts`:

```ts
import { createGrpcTransport } from '@connectrpc/connect-node';
import type { Interceptor } from '@connectrpc/connect';
import { config } from '../config';

// Machine identity (ADR §5): one trusted caller, one static credential.
const bearer: Interceptor = (next) => (req) => {
  req.header.set('authorization', `Bearer ${config.controlPlaneBearer}`);
  return next(req);
};

export const controlPlaneTransport = createGrpcTransport({
  baseUrl: config.controlPlaneGrpcUrl,   // HTTP/2; also carries the Relay bidi
  interceptors: [bearer],
  // ADR §9.4: detect half-open connections so leases don't ghost.
  pingIntervalMs: 20_000,
  pingTimeoutMs: 10_000,
  pingIdleConnection: true,
});
```

- [ ] **Step 2:** `src/control-plane/client.ts` — typed clients for
  `SessionService`, `ShellRelayService`, `SecretService`, `ImageService`,
  `FleetService` via `createClient(Svc, controlPlaneTransport)`.
- [ ] **Step 3:** Tests (fake upstream captures headers). Commit:

```bash
git add orchestrator/
git commit -m "feat(orchestrator): control-plane transport — service bearer + h2 keepalives (ADR 0039 §5)"
```

### Task 18: CASL ability + the policy gate + the generic passthrough

**Goal:** The authorization core (ADR §6) and the forwarder it guards. The
gate **fails closed**: a method with no policy entry is denied even if it's in
the surface.

**Outcome:** Fake-upstream vitest matrix green: member reads own session ✓,
member reads another's session ✗ (`permission_denied`), member calls
`ListHosts` ✗, admin calls anything ✓, unauthenticated → 401, method without
a policy entry → denied. Live (needs Task 10): the curl checks below.

**Files:**
- Create: `orchestrator/src/authz/ability.ts`, `src/authz/policy-map.ts`,
  `src/authz/resolve.ts`, `src/rpc/passthrough.ts`, `src/rpc/surface.ts`
- Modify: `src/index.ts`

**Steps:**

- [ ] **Step 1:** `src/authz/ability.ts` — THE policy, one file, exported for
  the web too (it will be copied/shared via the contract-test convention —
  Task 26 wires the web copy):

```ts
import { AbilityBuilder, createMongoAbility, type MongoAbility } from '@casl/ability';

export type Actions = 'create' | 'read' | 'prompt' | 'shell' | 'delete' | 'manage';
export type Subjects = 'Task' | 'Session' | 'EnabledImage' | 'Fleet' | 'Registry' | 'all';
export type AppAbility = MongoAbility<[Actions, Subjects]>;

export interface AbilityUser { id: string; role: string }   // role: 'admin' | 'user'

// ADR §6: two roles, one ownership relation. Ownership is resolved via
// the task join (authz/resolve.ts) and passed in as the subject's
// `createdByUserId`. If this file grows team/sharing semantics, that is
// the OpenFGA trigger — stop and write the ADR first.
export function abilityFor(user: AbilityUser): AppAbility {
  const { can, build } = new AbilityBuilder<AppAbility>(createMongoAbility);
  can('create', 'Task');
  can('manage', 'Task',    { createdByUserId: user.id });
  can(['read', 'prompt', 'shell', 'delete'], 'Session', { createdByUserId: user.id });
  can('read', 'EnabledImage');   // the create form needs images + enable-job reads
  if (user.role === 'admin') can('manage', 'all');
  return build();
}
```

- [ ] **Step 2:** `src/authz/resolve.ts` — the ownership join:

```ts
// sessionId → the owning task's createdByUserId (or null: unattributed —
// admin-only by construction, since no ownership condition can match).
// One indexed lookup (task_session_session_idx); cache briefly (5s LRU)
// because the SSE/WS/artifact routes hit it per-connection, not per-frame.
export async function sessionOwner(sessionId: string): Promise<{ createdByUserId: string | null } | null>
```

- [ ] **Step 3:** `src/authz/policy-map.ts` — per-method entries; **no entry =
  deny**:

```ts
import type { DescMethod } from '@bufbuild/protobuf';

export interface PolicyEntry {
  action: 'read' | 'prompt' | 'shell' | 'delete' | 'manage';
  subject: 'Session' | 'EnabledImage' | 'Fleet' | 'Registry' | 'all';
  // For session-scoped RPCs: pull the session id out of the request so
  // the gate can resolve ownership. Field name per request type.
  sessionIdField?: string;
}

// SessionService: session-scoped, member-reachable when owner.
// ListSessions/CreateSession are NOT here: the UI goes through
// TaskService (native); raw list/create is admin-only via 'manage all'.
export const POLICY: Record<string, PolicyEntry> = {
  'SessionService.GetSession':       { action: 'read',   subject: 'Session', sessionIdField: 'sessionId' },
  'SessionService.DeleteSession':    { action: 'delete', subject: 'Session', sessionIdField: 'sessionId' },
  'SessionService.SendPrompt':       { action: 'prompt', subject: 'Session', sessionIdField: 'sessionId' },
  'SessionService.Interrupt':        { action: 'prompt', subject: 'Session', sessionIdField: 'sessionId' },
  'SessionService.GetLog':           { action: 'read',   subject: 'Session', sessionIdField: 'sessionId' },
  'SessionService.GetCowState':      { action: 'read',   subject: 'Session', sessionIdField: 'sessionId' },
  'SessionService.ListCheckpoints':  { action: 'read',   subject: 'Session', sessionIdField: 'sessionId' },
  'SessionService.Exec':             { action: 'shell',  subject: 'Session', sessionIdField: 'sessionId' },
  'SessionService.Snapshot':         { action: 'manage', subject: 'all' },
  'SessionService.Resume':           { action: 'read',   subject: 'Session', sessionIdField: 'sessionId' },
  'SessionService.EvictLocal':       { action: 'manage', subject: 'all' },
  'SessionService.ListSessions':     { action: 'manage', subject: 'all' },   // admin raw view
  'SessionService.CreateSession':    { action: 'manage', subject: 'all' },   // UI uses CreateTask
  'ImageService.ListEnabledImages':  { action: 'read',   subject: 'EnabledImage' },
  'ImageService.ListEnableJobs':     { action: 'read',   subject: 'EnabledImage' },
  'ImageService.GetEnableJob':       { action: 'read',   subject: 'EnabledImage' },
  // everything else on Image/Fleet: admin
  'ImageService.EnableImage':        { action: 'manage', subject: 'all' },
  'ImageService.DisableImage':       { action: 'manage', subject: 'all' },
  'ImageService.RefreshImage':       { action: 'manage', subject: 'all' },
  'ImageService.RetryEnableJob':     { action: 'manage', subject: 'all' },
  'ImageService.ListRegistries':     { action: 'manage', subject: 'all' },
  'ImageService.AddRegistry':        { action: 'manage', subject: 'all' },
  'ImageService.DeleteRegistry':     { action: 'manage', subject: 'all' },
  'FleetService.ListHosts':          { action: 'manage', subject: 'all' },
  // ... every FleetService method: { action: 'manage', subject: 'all' }
};

export const policyKey = (svc: string, m: DescMethod) =>
  `${svc.split('.').pop()}.${m.name}`;
```

  (GetArtifact and StreamEvents are session-scoped too but are served by the
  Hono routes, not the passthrough — their handlers call the same
  `sessionOwner` + `ability.can` directly. They get NO policy-map entry and
  are excluded from the surface.)
- [ ] **Step 4:** `src/rpc/passthrough.ts` — the forwarder with the gate
  (Transport call shapes verified against connect v2; `methodKind` is
  `'server_streaming'`, snake_case):

```ts
import type { ConnectRouter, Transport, HandlerContext } from '@connectrpc/connect';
import { ConnectError, Code } from '@connectrpc/connect';
import { subject } from '@casl/ability';
import type { DescService } from '@bufbuild/protobuf';
import { POLICY, policyKey } from '../authz/policy-map';
import { abilityFor } from '../authz/ability';
import { sessionOwner } from '../authz/resolve';
import { auth } from '../auth/better-auth';

export interface PassthroughSpec {
  service: DescService;
  methods?: string[];
}

// One forwarder for every passthrough RPC + THE authz boundary (ADR §6).
// The control plane trusts us completely — there is no re-check below.
// Fail closed: no session → 401; no policy entry → deny.
export function registerPassthrough(
  router: ConnectRouter,
  specs: PassthroughSpec[],
  upstream: Transport,
) {
  for (const { service, methods } of specs) {
    const impl: Record<string, unknown> = {};
    for (const m of service.methods) {
      if (methods && !methods.includes(m.name)) continue;
      if (m.methodKind !== 'unary' && m.methodKind !== 'server_streaming') continue;

      const gate = async (req: unknown, ctx: HandlerContext) => {
        const session = await auth.api.getSession({ headers: headersOf(ctx) });
        if (!session) throw new ConnectError('unauthenticated', Code.Unauthenticated);
        const entry = POLICY[policyKey(service.typeName, m)];
        if (!entry) throw new ConnectError('forbidden', Code.PermissionDenied);
        const ability = abilityFor({ id: session.user.id, role: session.user.role ?? 'user' });
        if (entry.sessionIdField) {
          const sid = (req as Record<string, string>)[entry.sessionIdField];
          const owner = await sessionOwner(sid);
          // Unattributed (no task row / unknown): only 'manage all' passes,
          // because the ownership condition can never match null.
          if (!ability.can(entry.action, subject('Session', { createdByUserId: owner?.createdByUserId ?? null }))) {
            // 404-shape, not 403: don't confirm existence to non-owners.
            throw new ConnectError('not found', Code.NotFound);
          }
        } else if (!ability.can(entry.action, entry.subject)) {
          throw new ConnectError('forbidden', Code.PermissionDenied);
        }
      };

      if (m.methodKind === 'unary') {
        impl[m.localName] = async (req: unknown, ctx: HandlerContext) => {
          await gate(req, ctx);
          const res = await upstream.unary(m, ctx.signal, undefined, undefined, req, ctx.values);
          copyHeaders(res.header, ctx.responseHeader);
          copyHeaders(res.trailer, ctx.responseTrailer);
          return res.message;
        };
      } else {
        impl[m.localName] = async function* (req: unknown, ctx: HandlerContext) {
          await gate(req, ctx);
          const res = await upstream.stream(
            m, ctx.signal, undefined, undefined,
            (async function* () { yield req; })(), ctx.values,
          );
          copyHeaders(res.header, ctx.responseHeader);
          yield* res.message;
        };
      }
    }
    router.service(service, impl as never);
  }
}
```

  Write the small `headersOf(ctx)` / `copyHeaders` helpers (the request
  headers come off the handler context; check the installed types). The
  anti-enumeration choice (NotFound for unowned sessions) mirrors what the
  coordinator middleware did — keep it.
- [ ] **Step 5:** `src/rpc/surface.ts`:

```ts
// What the UI may reach via passthrough. Absent on purpose:
// ShellRelayService (WS route), SecretService (keys must come from the
// session — routes/me.ts), TaskService (native impl, Task 19),
// StreamEvents/GetArtifact (Hono routes own them).
export const SURFACE: PassthroughSpec[] = [
  { service: SessionService, methods: SessionService.methods
      .filter((m) => !['StreamEvents', 'GetArtifact'].includes(m.name)).map((m) => m.name) },
  { service: FleetService },
  { service: ImageService },
];
```

- [ ] **Step 6:** The **passthrough conformance test**
  (`src/rpc/passthrough.conformance.test.ts`) — the generic fidelity proof
  (see "Stage smoke gates" layer 1). One reflection-based filler, every
  method covered automatically:

```ts
import { create, toBinary, type DescMessage, type DescField } from '@bufbuild/protobuf';

// Deterministically populate EVERY field of a message: string → field
// name, numbers → field number, bool → true, bytes → [0xAB], enum →
// first non-zero value, repeated → [one element], map → one entry,
// nested message → recurse (depth-capped at 3), oneof → first case.
// If a field kind isn't handled, THROW — an unpopulatable field is a
// finding, not a skip.
function populate(schema: DescMessage, depth = 0): unknown { /* ~50 lines */ }

for (const { service, methods } of SURFACE) {
  for (const m of service.methods.filter(/* surface filter */)) {
    test(`${service.typeName}/${m.name} passes through faithfully`, async () => {
      const req = populate(m.input);
      const expectedRes = populate(m.output);
      // fake upstream: capture the request, reply with expectedRes
      // (server-streaming: yield it twice), set a response header.
      const captured = await callThroughOrchestratorAsAdmin(m, req);
      expect(toBinary(m.input, captured.upstreamSawRequest)).toEqual(toBinary(m.input, req));
      expect(toBinary(m.output, captured.clientGotResponse)).toEqual(toBinary(m.output, expectedRes));
      expect(captured.upstreamSawHeaders.get('authorization')).toMatch(/^Bearer /);
      expect(captured.clientGotHeaders.get('x-fake-upstream')).toBe('1');
    });
  }
}
```

  Run as an admin user so the policy gate passes — transport fidelity and
  authz are tested orthogonally (authz is Step 7's matrix). A future RPC
  added to the contract is covered with zero new test code.
- [ ] **Step 7:** Tests — the authz Outcome matrix, with an in-process fake
  upstream + a seeded local DB (two users, one admin; two tasks). Wire into
  `index.ts`. Also create `orchestrator/src/smoke.live.test.ts` (env-gated,
  `SMOKE=1`) + the `just smoke-orchestrator` recipe, and append it to the
  `smoke` umbrella — the live half of this task's Outcome, extended by
  Tasks 19/20 with the CreateTask→SSE→delete flow. Live curls when Task 10
  lands:
  member cookie + own session `GetSession` → 200; another's → 404 (NotFound);
  `ListHosts` → 403; no cookie → 401. Commit:

```bash
git add orchestrator/
git commit -m "feat(orchestrator): CASL ability + fail-closed policy gate on the generic passthrough (ADR 0039 §6)"
```

### Task 19: TaskService — the native implementation

**Goal:** The aggregate root's API (ADR §3): `CreateTask` (chat) does upstream
`CreateSession` + the two inserts; `ListTasks`/`GetTask` join task rows with
live control-plane session state; `DeleteTask` deletes the session(s) then the
rows.

**Outcome:** Vitest (fake upstream + local DB): create → list shows the task
with its session ref; member sees only their tasks, admin sees all (incl. an
unattributed session surfaced as a synthetic admin-only row); delete cascades.
Live: a task created via curl appears with real session state.

**Files:**
- Create: `orchestrator/src/rpc/tasks.ts`
- Modify: `src/index.ts` (register on the same ConnectRouter as the
  passthrough — one `/rpc` surface)

**Steps:**

- [ ] **Step 1:** Implement against the generated `TaskService` descriptor:

```ts
// TaskService — orchestrator-NATIVE (ADR §3). Same Connect router as the
// passthrough, so the web sees one uniform generated API; never proxied.
export function registerTasks(router: ConnectRouter) {
  router.service(TaskService, {
    async createTask(req, ctx) {
      const user = await requireUser(ctx);                  // 401 if no session
      if (req.type !== 'chat') throw new ConnectError('only chat tasks exist yet', Code.InvalidArgument);
      if (!abilityFor(user).can('create', 'Task')) throw new ConnectError('forbidden', Code.PermissionDenied);
      // Harness token: keyed by user id in the sealed store (Task 20's
      // routes/me.ts writes it). HasSecret decides whether to pass the ref.
      const { exists } = await secrets.hasSecret({ key: user.id });
      const created = await sessions.createSession({
        imageUri: req.imageUri,
        mode: 'agent',
        prompt: req.prompt,
        harnessSecretId: exists ? user.id : undefined,
      });
      const id = crypto.randomUUID();
      await db.transaction(async (tx) => {
        await tx.insert(task).values({
          id, type: 'chat', title: req.title ?? null, status: 'open',
          createdByUserId: user.id, source: {},
        });
        await tx.insert(taskSession).values({ taskId: id, sessionId: created.sessionId, role: 'primary' });
      });
      return { task: await loadTask(id, user) };
    },
    async listTasks(_req, ctx) { /* rows scoped by ability (member: own; admin: all
      + synthetic rows for unattributed control-plane sessions) joined with
      upstream ListSessions for live state */ },
    async getTask(req, ctx)  { /* ability-checked single row + session join */ },
    async deleteTask(req, ctx) { /* ability 'delete' on the task → upstream
      DeleteSession per task_session row → delete task (cascade) */ },
  });
}
```

  Failure ordering in `createTask`: if the upstream create succeeds but the
  insert fails, delete the session before rethrowing (compensation — chat has
  no DBOS to lean on, so don't leave orphans).
- [ ] **Step 2:** `loadTask`/`listTasks` join shape: one upstream
  `ListSessions({})` per list call (the trusted caller gets everything; match
  rows in memory), `GetSession` per `getTask`. Status for chat tasks derives
  from the session state (map `active/idle/… → working`, terminal → `done`).
- [ ] **Step 3:** The Outcome test matrix; live check. Commit:

```bash
git add orchestrator/
git commit -m "feat(orchestrator): native TaskService — tasks own sessions (ADR 0039 §3)"
```

### Task 20: SSE events route + artifact byte route + `/me/claude-token`

**Goal:** The browser-native HTTP legs, each gated by the same ability checks
as the passthrough: SSE events, artifact bytes (browsers consume artifacts as
`<img src>`, not RPCs — without this route every artifact 404s at the Task 29
proxy flip), and the secret-relay route (`SecretService` is not in the
passthrough surface — the key must be the *session's* user id, never
client-supplied).

**Outcome:** SSE curl streams today's events in the envelope frame format;
reconnect with `Last-Event-ID` replays from the cursor; the artifact route
streams an image with the right content-type; `POST /api/v1/me/claude-token`
flips `HasSecret`. Live Outcomes need Tasks 11/12/13.

**Steps:**

- [ ] **Step 1:** Shared guard helper: resolve the better-auth session → 401;
  resolve `sessionOwner(sessionId)` → `abilityFor(user).can('read', subject('Session', …))`
  → 404 on failure (same anti-enumeration shape as Task 18).
- [ ] **Step 2:** `routes/events.ts`. The wire envelope is **hand-built JSON**
  (protobuf-JSON would emit lowerCamel `payloadJson` + int64-as-string — the
  parser shouldn't have to know):

```ts
events.get('/api/v1/sessions/:id/events', async (c) => {
  const user = await guardSession(c, 'read');          // Step 1 helper; throws 401/404
  return streamSSE(c, async (stream) => {
    // Cursor: max(?since, Last-Event-ID), NaN-guarded — matches the
    // coordinator's "never goes backward" rule (web/src/sse.ts:5-9).
    const nums = [c.req.query('since'), c.req.header('last-event-id')]
      .map(Number).filter((n) => Number.isFinite(n) && n >= 0);
    const since = nums.length ? BigInt(Math.max(...nums)) : undefined;

    const upstream = sessions.streamEvents(
      { sessionId: c.req.param('id'), since },
      { signal: c.req.raw.signal },                     // browser close → RST upstream
    );
    // Keepalive: coordinator emits SSE comments every 15s (axum KeepAlive,
    // api/events.rs:78-81). hono's writeSSE can't emit comments; an empty
    // `event: ping` frame is functionally equivalent (sse.ts listens
    // per-kind and ignores unknown events).
    const ping = setInterval(() => void stream.writeSSE({ data: '', event: 'ping' }), 15_000);
    try {
      for await (const ev of upstream) {
        await stream.writeSSE({
          // lagged frames have no idx — omit id: so reconnect cursors are
          // never disturbed (mirrors api/events.rs:103-105).
          ...(ev.idx !== undefined ? { id: String(ev.idx) } : {}),
          event: ev.kind,
          data: JSON.stringify({
            idx: ev.idx !== undefined ? Number(ev.idx) : null,
            kind: ev.kind,
            payload_json: ev.payloadJson,
          }),
        });
      }
    } finally {
      clearInterval(ping);
    }
  });
});
```

- [ ] **Step 3:** `routes/artifacts.ts`: guard → upstream `getArtifact` →
  first message (`metadata`) sets content-type/length → pipe `chunk` frames;
  abort upstream on client disconnect (same signal pattern).
- [ ] **Step 4:** `routes/me.ts`: `POST /api/v1/me/claude-token` →
  `secrets.putSecret({ key: user.id, value: body.token })` — **opaque relay**:
  never logged, never persisted locally; `GET` → `hasSecret`; `DELETE` →
  `deleteSecret`.
- [ ] **Step 5:** Tests (fake upstream): 3 events → 3 ordered `id:` lines;
  a lagged event → no `id:`; client abort cancels the fake (onAbort flag);
  artifact metadata-then-chunks; token route 401 without cookie. Commit:

```bash
git add orchestrator/
git commit -m "feat(orchestrator): SSE event leg + artifact bytes + claude-token relay (ADR 0039 §8)"
```

### Task 21: Shell WS route

**Goal:** `GET /api/v1/sessions/:id/shell` ⇄ upstream `ShellRelayService.Relay`,
ability-gated (`'shell'`), with the corrections the review round established:
**no unhandled rejections** (a bare abort error in a void'd async IIFE kills
Node on every normal tab close), ping/pong relayed, WS keepalive, backpressure.

**Outcome:** Terminal works through the relay; killing the client releases the
coordinator lease (Task 12's debug log) **and the orchestrator process
survives** (test asserts no unhandledRejection).

**Steps:**

- [ ] **Step 1:** Wire `createNodeWebSocket({ app })` →
  `{ upgradeWebSocket, injectWebSocket }`; inject on the raw server in
  `index.ts`.
- [ ] **Step 2:** The bridge — auth+ability gate **before** upgrade (401/404
  pre-upgrade, same guard as Task 20); `catch` around the pump (swallow
  `Code.Canceled`/`Code.Aborted`, log others, `ws.close(1011)`); answer
  upstream `ping` frames with `pong` (browser JS cannot send WS pongs; the
  host side uses these for liveness — shell.rs:119/142); handle Node `Buffer`
  message data (not browser `ArrayBuffer`):

```ts
app.get('/api/v1/sessions/:id/shell', upgradeWebSocket((c) => {
  const abort = new AbortController();
  const inbound = pushableQueue<RelayShellRequest>();   // bounded async-iterable queue, this file
  return {
    onOpen: (_e, ws) => {
      inbound.push(openFrame(c.req.param('id')));
      void (async () => {
        try {
          for await (const f of shellRelay.relay(inbound, { signal: abort.signal })) {
            switch (f.frame.case) {
              case 'text':   ws.send(f.frame.value); break;
              case 'binary': ws.send(f.frame.value); break;
              case 'ping':   inbound.push(pongFrame(f.frame.value)); break;
              case 'close':  ws.close(f.frame.value.code, f.frame.value.reason); break;
            }
          }
        } catch (e) {
          if (!isAbortLike(e)) {                         // Canceled/Aborted = normal teardown
            log.warn({ err: e }, 'shell relay error');
            ws.close(1011, 'upstream error');
          }
        } finally {
          ws.close();                                    // upstream end → browser close
        }
      })();
    },
    onMessage: (e) => inbound.push(frameFromWsEvent(e)), // string | Buffer | ArrayBuffer
    onClose: () => { abort.abort(); inbound.end(); },    // browser close → upstream abort
  };
}));
```

- [ ] **Step 3:** Helpers + tests: `pushableQueue` (capped) with a binary
  `Buffer` round-trip; output backpressure via `ws.raw.bufferedAmount`
  high-water pause (a `cat bigfile` must not balloon memory).
- [ ] **Step 4:** WS keepalive: `ws.raw.ping()` every 20s, close on missed
  pong (§9.4 half-open). Subprotocol: the client opens
  `new WebSocket(url, 'tty')` (`TerminalPane.tsx:170`) — verify the upgrade
  response echoes `Sec-WebSocket-Protocol: tty` (browsers hard-fail
  otherwise).
- [ ] **Step 5:** Process-survives test (vitest traps `unhandledRejection`;
  open against a fake upstream, close client, assert alive). Live check vs
  Task 12. Commit:

```bash
git add orchestrator/
git commit -m "feat(orchestrator): browser WS shell leg over ShellRelayService bidi"
```

### Task 21b: Cross-stack parity smoke (scaffolding — lives Phases 3→4, dies in Task 28)

**Goal:** The differential old-vs-new check ("Stage smoke gates" layer 2):
while the legacy axum REST and the new gated `/rpc` both serve the same data,
read every overlapping resource through both paths and diff. This is the only
test that catches a Rust core extraction or `convert.rs` copy silently
defaulting a field — conformance (Task 18) can't see below the orchestrator.

**Outcome:** `just smoke-parity` (dev stack up, one session existing) prints a
per-probe ✓/✗ table and exits non-zero on any diff; the SSE probe confirms
identical `idx` sequences + payloads for the first 20 events on both feeds.

**Files:**
- Create: `orchestrator/scripts/parity.ts`
- Modify: `justfile` (`smoke-parity` recipe: `cd orchestrator && pnpm tsx scripts/parity.ts`)

**Steps:**

- [ ] **Step 1:** The probe table — one entry per overlapping read surface:

```ts
interface Probe {
  name: string;
  rest: string;                       // legacy: http://127.0.0.1:8090/api/v1/...
  rpc: () => Promise<unknown>;        // new: via /rpc with an admin cookie
  volatile?: string[];                // per-probe drop-list (ages, timestamps)
}
const PROBES: Probe[] = [
  { name: 'sessions',      rest: '/sessions',        rpc: () => rpcList('SessionService/ListSessions'), volatile: ['age_s', 'updated_at'] },
  { name: 'hosts',         rest: '/hosts',           rpc: () => rpcList('FleetService/ListHosts'),      volatile: ['last_heartbeat'] },
  { name: 'storage',       rest: '/storage/summary', rpc: () => rpcList('FleetService/GetStorageSummary') },
  { name: 'images',        rest: '/enabled-images',  rpc: () => rpcList('ImageService/ListEnabledImages') },
  { name: 'registries',    rest: '/registries',      rpc: () => rpcList('ImageService/ListRegistries') },
  { name: 'cow-state',     rest: `/sessions/${SID}/cow-state`,   rpc: () => …, volatile: ['…'] },
  { name: 'checkpoints',   rest: `/sessions/${SID}/checkpoints`, rpc: () => … },
];
```

- [ ] **Step 2:** Normalization before diffing: fold camelCase↔snake_case keys
  to one convention, sort arrays by `id`/primary key, apply the volatile
  drop-list, stringify-stable, diff. The legacy REST runs unauthenticated
  (dev `AuthMode::None`); the `/rpc` side signs in as the bootstrap admin
  (Task 16 recipe) and reuses the Task 22 sign-in helper once it exists.
- [ ] **Step 3:** The SSE probe: open the coordinator's legacy
  `/api/v1/sessions/:id/events` and the orchestrator's new SSE for the same
  session, collect 20 events each from `since=-1`, compare `(idx, kind,
  payload)` triples (the new feed wraps in the envelope — unwrap before
  comparing).
- [ ] **Step 4:** Run it; fix what it finds (expect converter-level findings —
  that is its job). Append `smoke-parity` to the `smoke` umbrella.
  **Mark with a `// SCAFFOLDING: delete in Task 28` header** — when the
  legacy REST dies there is nothing left to compare against.
- [ ] **Step 5:** Commit:

```bash
git add orchestrator/scripts justfile
git commit -m "test(parity): differential old-vs-new smoke across the migration window"
```

---

# Phase 4 — Web migration

> **Posture (read first):** runnable in dev only — the coordinator keeps
> `AuthMode::None` + SyntheticAdmin on its legacy axum routes until Task 32.
> Mid-phase, RPC data flows as the better-auth user while events/shell still
> ride the coordinator under SyntheticAdmin until Tasks 27/28 — expected.
> **Deviation note:** events keep a hand-typed payload union
> (`web/src/events.ts`), pinned by Task 27's fixture contract test rather
> than codegen.

### Task 22: better-auth client, login page, AuthProvider, e2e entry

**Goal:** The auth front door moves to the orchestrator. `AuthProvider` keeps
its external interface (`principal` with `is_admin` / `has_claude_token` /
`display_name`, `refresh()`) but recomposes internally: **role comes straight
off the better-auth session** (admin plugin — no control-plane call, unlike
the pre-revision GetMe design) and `has_claude_token` from
`GET /api/v1/me/claude-token`.

**Sizing note:** two PRs — (a) login + proxy + e2e entry, (b) the
AuthProvider recomposition.

**Outcome:** `just e2e` passes entering through better-auth (the one
sanctioned entry-step change); admin nav gates off the better-auth role;
sign-out works. **Visual check:** snap `/login` (new page — intended diff:
it didn't exist in baseline) and `/` post-login vs baseline: identical chrome,
no auth-flash/spinner stuck states.

**Files:**
- Modify: `web/vite.config.ts` — `/api/auth` + `/rpc` →
  `http://127.0.0.1:8787` **before** the existing `/api` rule (vite matches in
  insertion order — verified); `/api/v1` keeps targeting the coordinator until
  Task 29.
- Create: `web/src/pages/Login.tsx`, `web/src/lib/auth-client.ts`
- Modify: `web/src/auth/AuthProvider.tsx` (also: `logout()` POSTs the
  coordinator's `/auth/logout` and the 401 path hard-navigates to
  `/api/v1/auth/login` — swap to `authClient.signOut()` + an SPA `/login`
  redirect), `web/src/App.tsx` (route `/login`)
- Modify: `web/e2e/global-setup.ts`, `web/playwright.config.ts`

**Steps:**

- [ ] **Step 1:** `pnpm -C web add better-auth`. `web/src/lib/auth-client.ts`:

```ts
import { createAuthClient } from 'better-auth/react';
import { adminClient } from 'better-auth/client/plugins';
// Same-origin: the vite proxy (dev) / fronting LB (prod) routes /api/auth
// to the orchestrator. Don't set a cross-origin baseURL — cookie scoping.
export const authClient = createAuthClient({ plugins: [adminClient()] });
```

- [ ] **Step 2:** `Login.tsx` (email+password sign-in, dev sign-up);
  unauthenticated users route here. **Production decision recorded here in a
  comment:** public sign-up is dev-only; production disables it or gates on
  an allowlist (replacing the old `NotMemberError` screen's job).
- [ ] **Step 3:** Recompose `AuthProvider`: `authClient.useSession()` for
  authn + role (`session.user.role === 'admin'` → `is_admin`); token presence
  via the Task 20 route; keep the exported `AuthState` shape so `UserChip`,
  `ProfilePanel`, `TokensPanel`, `NewSessionForm`, `Members`, `RequireAdmin`
  don't churn.
- [ ] **Step 4:** e2e entry via Playwright's request context (don't hand-roll
  set-cookie parsing):

```ts
import { request } from '@playwright/test';
// in globalSetup, after the precondition gate:
const ctx = await request.newContext({ baseURL: 'http://localhost:5173' });
await ctx.post('/api/auth/sign-up/email', { data: { email: E2E_EMAIL, password: E2E_PW, name: 'e2e' } })
  .catch(() => {});                                  // idempotent
const res = await ctx.post('/api/auth/sign-in/email', { data: { email: E2E_EMAIL, password: E2E_PW } });
if (!res.ok()) throw new Error('better-auth sign-in failed — orchestrator up on :8787?');
await ctx.storageState({ path: 'e2e/.auth-state.json' });
```

  Reference via `use.storageState`; promote the e2e user with the Task 16
  admin recipe (the journey exercises admin-visible surfaces today). Journey
  spec body untouched.
- [ ] **Step 5:** `just e2e` → PASS. **Visual check** per the Outcome. Commit
  per PR boundary.

### Task 23: The task list + CreateTask flow

**Goal:** The UI's primary noun becomes the task (ADR §3): the list page reads
`TaskService.ListTasks`; the create flow calls `CreateTask(type:'chat')`.
Chat-only, so rows look essentially like today's session rows — by design.

**Outcome:** The list renders tasks (chat tasks ≈ session rows; unattributed
legacy sessions visible to the admin e2e user); create → navigates to
`/sessions/<id>` exactly as today; `just e2e` green **unchanged** (testids
survive the noun change). **Visual check:** snap `/` and the new-session form
vs baseline — intended diffs: none visible for chat-only rows (title chip if
`title` set); anything else (missing status chips, unstyled rows, empty
list with sessions present) is a regression.

**Files:**
- Modify: `web/src/App.tsx` — `TransportProvider` with
  `createConnectTransport({ baseUrl: '/rpc' })` wraps alongside the existing
  `QueryClientProvider` (it lives in `App.tsx`, not `main.tsx` — verified)
- Modify: `web/src/pages/Sessions.tsx`, `web/src/hooks/useSessions.ts` →
  `useTasks` on `listTasks`; `web/src/components/SessionManifest.tsx` rows
  take task objects (keep `data-testid="session-row"`)
- Modify: `web/src/components/NewSessionForm.tsx` —
  `useMutation(createTask)`; image options still via
  `ImageService.ListEnabledImages` (member-level passthrough)
- Modify: `web/src/test-utils.tsx` (TransportProvider backed by
  `createRouterTransport` fakes), `web/src/components/NewSessionForm.test.tsx`
  (it mocks `globalThis.fetch` on REST URLs today — replace fetch-spying with
  `createRouterTransport` service fakes)

**Steps:**

- [ ] **Step 1:** `pnpm -C web add @connectrpc/connect-web`. Wire
  `TransportProvider`.
- [ ] **Step 2:** `useTasks` — **carry each old hook's TanStack options
  verbatim** (the old `useSessions` polls at 1s with
  `placeholderData: (prev) => prev`; dropping them freezes the list and fails
  the Phase 0 net):

```ts
import { useQuery } from '@connectrpc/connect-query';
import { listTasks } from '../gen/engram/app/v1/task-TaskService_connectquery';

export function useTasks() {
  return useQuery(listTasks, {}, {
    refetchInterval: 1_000,                  // carried from useSessions
    placeholderData: (prev) => prev,
  });
}
```

  Generated TS is camelCase (`imageUri`) — fix call sites as the compiler
  surfaces them; that's the hand-mirror debt being paid.
- [ ] **Step 3:** Create flow → `useMutation(createTask)`; invalidate the
  tasks query via `createConnectQueryKey` (manual string keys die with
  `api.ts`).
- [ ] **Step 4:** Fix the test harness (`createRouterTransport` fakes for
  TaskService + ImageService). `pnpm -C web test` green.
- [ ] **Step 5:** `just e2e` green. **Visual check** per the Outcome. Commit:

```bash
git add web/
git commit -m "feat(web): task list + CreateTask flow — tasks are the primary noun (ADR 0039 §3)"
```

### Task 24: Session detail data → connect-query

**Goal:** SessionDetail's data hooks (`useSession`-equivalent via `GetSession`,
`useCheckpoints`, `useCowState`) and the prompt/interrupt mutations move to
the passthrough.

**Outcome:** Detail renders identically; network tab shows
`/rpc/engram.app.v1.SessionService/*`; `pnpm -C web test` + `just e2e` green.
**Visual check:** snap `/sessions/<id>` (transcript + raw tabs) vs baseline —
intended diffs: none.

**Steps:**

- [ ] **Step 1:** Migrate hooks per the Task 23 pattern — carry polling
  options (detail 2s, checkpoints 5s, cow-state 2s) and either keep consumers
  on `data.session` etc. or use `select`; pick one convention and apply it
  everywhere.
- [ ] **Step 2:** Mutations: `sendPrompt` lives in
  `web/src/components/PromptComposer.tsx`, interrupt in `SessionDetail.tsx`
  (there is **no** delete-session call in the web today — don't invent one;
  task deletion arrives with task UI later).
- [ ] **Step 3:** Tests + `just e2e` + **Visual check**. Commit:

```bash
git add web/
git commit -m "feat(web): session detail on connect-query via the gated passthrough"
```

### Task 25: Admin pages + ability-shared UI gating

**Goal:** Fleet/Storage/Settings/Members on the new stack, with UI affordances
driven by **the same ability file the server enforces** (ADR §6).

**Outcome:** Admin user: all four pages render live data. Member user: admin
nav hidden, member-visible pages render, no dead buttons. **Visual check:**
snap `/fleet`, `/storage`, `/settings`, `/members` as admin vs baseline
(intended diffs: none) AND as a member (intended diff: admin nav/sections
absent — capture this as a new member-baseline for future tasks).

**Files:**
- Create: `web/src/lib/ability.ts` — re-export/copy of
  `orchestrator/src/authz/ability.ts` (`pnpm -C web add @casl/ability`;
  packages aren't workspace-linked — copy the file verbatim and add a
  comment + a CI-able `diff` check in the contract test so drift is caught)
- Modify: `web/src/hooks/useHosts.ts`, `useDrainHost.ts`,
  `useStorageSummary.ts`, `useEnabledImages.ts`, `useEnableJobs.ts`,
  `useRegistries.ts` → connect-query (carry options; rebuild the three
  cross-hook invalidation edges — `useDrainHost`→`['hosts']`,
  `useEnableJobs`↔`['enabled-images']`, `useEnableImage`→`['enable-jobs']` —
  with `createConnectQueryKey`)
- Modify: `web/src/pages/Fleet.tsx`, `Storage.tsx`, `Settings.tsx`,
  `Members.tsx`, `web/src/auth/RequireAdmin.tsx`

**Steps:**

- [ ] **Step 1:** Hooks migration per the Task 23 pattern; GC buttons pass
  `dryRun`; Fleet drain uses `DrainHost` (the soft one the page calls today).
- [ ] **Step 2:** Members page → **better-auth admin client**
  (`authClient.admin.listUsers/setRole/banUser`) — replaces the old
  coordinator `/admin/users` surface; roles changed here are the same roles
  the ability reads.
- [ ] **Step 3:** Settings token form → the Task 20 `/api/v1/me/claude-token`
  routes; verify nothing logs the request body.
- [ ] **Step 4:** UI gating: `RequireAdmin` + nav + per-button affordances
  read `abilityFor(user).can(...)` from `web/src/lib/ability.ts`.
- [ ] **Step 5:** Tests; `just e2e`; **Visual check** per Outcome (both
  roles). Commit:

```bash
git add web/
git commit -m "feat(web): admin pages on the gated passthrough; ability-shared UI gating (ADR 0039 §6)"
```

### Task 26: Events → orchestrator SSE envelope

**Goal:** `useSessionEvents` consumes the orchestrator's SSE leg: still
`EventSource` (native reconnect — verified `sse.ts` has no custom retry loop),
new envelope parse; payload union isolated in `web/src/events.ts` and pinned
by a fixture contract test.

**Outcome:** Event count/status/RAW identical; reconnect resumes from
`Last-Event-ID` without gap. (Manual-check caveat: while the orchestrator is
*down* the vite proxy answers 502, which EventSource treats as fatal — native
retry covers network blips and stream drops, which is what production sees.)

**Steps:**

- [ ] **Step 1:** Move the `SessionEvent` union (`web/src/types.ts:250`) and
  `IndexedEvent` (`:384`) to `web/src/events.ts`, unchanged; mechanical
  import updates.
- [ ] **Step 2:** Rework the frame parse in `subscribeSession`
  (`web/src/sse.ts:21-98`), keeping its signature. The wire is Task 20's
  hand-built envelope — `{ idx: number|null, kind, payload_json }`
  (snake_case, numeric idx — deliberately NOT protobuf-JSON). Preserve the
  **`_rewound`/`_recovery_epoch` lifting** the current code does
  (`sse.ts:36-44`) — transcript greying depends on it. `lagged` arrives as
  `kind:"lagged"` with no SSE `id:` — keep the `onLagged` behavior. Point the
  EventSource path at the orchestrator-proxied route.
- [ ] **Step 3:** `web/src/events.contract.test.ts`: checked-in fixture lines
  captured from the live wire (one per major event kind + one rewound + one
  lagged), parsed through `subscribeSession`'s parser, type-asserted against
  the union. Also the ability-file drift check from Task 25 lives here.
- [ ] **Step 4:** `pnpm -C web test` (Transcript fixtures updated where they
  faked the wire) + live reconnect check + `just e2e`. Commit:

```bash
git add web/
git commit -m "feat(web): event feed via orchestrator SSE envelope; union pinned by contract test"
```

### Task 27: Shell → orchestrator WS

**Goal:** `TerminalPane`'s WebSocket rides the orchestrator.

**Outcome:** SHELL tab works end-to-end; e2e shell step green; the `tty`
subprotocol is echoed. **Visual check:** snap the shell tab vs baseline —
intended diffs: none (a blank/unstyled terminal pane = regression).

**Steps:**

- [ ] **Step 1:** Point the `/api/v1/sessions/:id/shell` proxy entry
  (`ws: true`) at the orchestrator; `TerminalPane` itself shouldn't change
  (same path).
- [ ] **Step 2:** Manual: `echo hi`; check `Sec-WebSocket-Protocol: tty` in
  the upgrade response; kill tab → lease release in coordinator logs.
- [ ] **Step 3:** `just e2e` green; **Visual check**. Commit:

```bash
git add web/
git commit -m "feat(web): shell websocket via the orchestrator relay"
```

### Task 28: Delete the hand-mirrored layer + flip the proxy

**Goal:** Remove `web/src/api.ts` and `web/src/types.ts`; flip `/api`
wholesale to the orchestrator; fix the e2e precondition probe (it queries
`/api/v1/enabled-images`, which the orchestrator doesn't serve — left as-is
the gate dies before any test runs).

**Outcome:** `git grep -nE "from '\.\.?/(api|types)'" web/src` → empty;
`pnpm -C web build` clean; `just e2e` green; the coordinator receives no
browser traffic. **Visual check:** full snap sweep (all baseline paths) —
intended diffs: none beyond those already accepted in Tasks 22–27.

**Steps:**

- [ ] **Step 1:** Flip the vite proxy: `/api` + `/rpc` → `:8787` only.
- [ ] **Step 2:** Sweep the long tail (verified dependents beyond hooks):
  `API_BASE` is imported by `sse.ts`, `TerminalPane.tsx`, `ArtifactCard.tsx`
  (artifact URLs resolve against the Task 20 byte route — same path; give
  `API_BASE` a home in `web/src/lib/base.ts`); `logout()`/`redirectToLogin`
  died in Task 22; pure-UI helper types move next to their single consumer or
  into `events.ts`. **Run `just smoke-parity` one final time, then delete
  `orchestrator/scripts/parity.ts` + the recipe** (Task 21b scaffolding — its
  comparison target dies with the legacy REST).
- [ ] **Step 3:** Re-point the e2e precondition: sign in first (the Task 22
  request-context), then probe
  `POST /rpc/engram.app.v1.ImageService/ListEnabledImages` with the captured
  cookie; keep the no-harness-image assertion.
- [ ] **Step 4:** `pnpm -C web build && pnpm -C web test && just e2e` green;
  **Visual check** sweep. Commit:

```bash
git add -A web/
git commit -m "feat(web)!: retire hand-mirrored api.ts/types.ts — generated contract only (ADR 0039 §7)"
```

---

# Phase 5 — Cutover and shedding the coordinator's web identity

> Order: scripts/CLI first (29), then the IAP door (30), **then** auth+identity
> collapse (31) and route removal (32) — never remove a door before its
> replacement exists.

### Task 29: Port scripts, CLI, and CI off the cookie/synthetic auth

**Goal:** Everything non-browser that consumes the web API keeps working when
SyntheticAdmin and the web routes go. **Inventory (verified):** `engram-cli`
is a full REST client (sessions, hosts, drain, registries, admin flush —
`crates/engram-cli/src/main.rs:514-1236`); `deploy/dev/tilt-up-ci.sh:60-65`
polls `GET /api/v1/hosts`; the CI e2e lane seeds GHCR creds via
`engram-cli registry add`; the `integration-*.sh` scripts curl unauthenticated.

**Outcome:** `just integration-session`, `integration-test.sh`, and the CI
lane pass against the app-gRPC surface with the Task 9 bearer; sessions they
create render as *unattributed* (admin-only) in the task UI — expected.

**Steps:**

- [ ] **Step 1:** Port `engram-cli`'s commands to tonic clients
  (`engram-protocol` is already a workspace dependency) using
  `APP_GRPC_TOKENS`/`ENGRAM_APP_TOKEN` env; port the scripts' curls to
  `engram-cli` invocations (one auth mechanism, not two).
- [ ] **Step 2:** **Do not touch `ENGRAM_AUTH_TOKENS`** (the host-ingest
  bearer, `api/auth.rs` — verified: dev/CI run it empty and
  `accepts_anything()` passes hosts through; flipping it non-empty bricks
  host registration unless coordinator + both Tilt host-agent resources +
  all four deploy/dev scripts + CI + prod values change atomically). The app
  surface's token (Task 9) is already separate — this task needs only it.
- [ ] **Step 3:** Run all three integration scripts + the CI lane. Commit:

```bash
git add crates/engram-cli deploy/ .github/
git commit -m "feat(cli,ci): port scripts and engram-cli to app-gRPC + bearer (ADR 0039 cutover prep)"
```

### Task 30: IAP bridge middleware (before the doors close)

**Goal:** Behind GCP IAP, a verified `X-Goog-IAP-JWT-Assertion` creates a
better-auth session (ADR §5). Sequenced **before** Task 31 so an IAP
production deploy always has a working door.

**Outcome:** Unit tests: valid ES256 IAP JWT (the real JWKS is ES256 at
`https://www.gstatic.com/iap/verify/public_key-jwk`,
`iss=https://cloud.google.com/iap`) → better-auth session for that email;
invalid → 401; inert when `IAP_AUDIENCE` unset. Plus a local smoke: sign with
a test ES256 key, point `IAP_JWKS_URL` at a local fixture server, drive a
request end-to-end.

**Steps:**

- [ ] **Step 1 (placement — the subtle part):** the bridge must cover **every
  entry path**, and `server.ts` routes `/rpc/*` around Hono — Hono-only
  middleware never runs for RPCs. Hoist the bridge to the raw-server level (a
  wrapper around both handlers in `buildServer`): one place, both stacks.
- [ ] **Step 2 (spike, timeboxed):** better-auth has no public "create a
  session for an arbitrary verified user" one-liner — pin the mechanism
  (server-side `auth.api` / internal adapter session-create) against the
  installed version before writing the middleware; record it in the file's
  doc comment. Semantics: create the session once on first verified request
  (JIT user create, default role `user`), set the cookie; subsequent requests
  ride the cookie.
- [ ] **Step 3:** Implement with `jose` (`createRemoteJWKSet` + `jwtVerify`,
  check `iss` + audience). Tests + the ES256 fixture smoke. Commit:

```bash
git add orchestrator/
git commit -m "feat(orchestrator): IAP trusted-SSO bridge into better-auth sessions (ADR 0039 §5)"
```

### Task 31: Coordinator sheds human auth AND identity

**Goal:** Remove the human cookie/OIDC/synthetic machinery and the identity
data (ADR §5/§6/§10). **File map (verified):** what goes is `CookieSession`
(`crates/engram-auth/src/cookie.rs`), `SyntheticAdmin` (`synthetic.rs`), the
OIDC endpoints `principal::login/callback` (api/mod.rs:209-210) +
`principal::logout` (`:96`), the `WebSessionStore` plumbing, the
`AuthMode::Oidc`/`None` arms in `build_chain` (config.rs:122-158),
`require_admin`/`require_session_owner` (principal.rs:160/:177), and —
**migrations** — the `users` table and `sessions.user_id` (nullable since
Task 10). **Do NOT touch** `api/auth.rs` (host-ingest bearer) or
`api/session_auth.rs` (ADR 0023/0026 per-session broker tokens for in-guest
forge/upload) — both are load-bearing for non-web traffic.

**Outcome:** The coordinator's auth surface is exactly: app-gRPC bearer
(Task 9), host-ingest bearer, broker tokens. `just dev` + `just e2e` +
`just integration-session` green with the orchestrator as the only human door.

**Steps:**

- [ ] **Step 1:** Inventory first:
  `git grep -n "SyntheticAdmin\|CookieSession\|AuthMode::\|require_admin\|require_session_owner\|user_id" crates/ deploy/`
  — every dependent must be already ported (Tasks 24–29) or part of this
  change. The legacy per-user Claude-token lookup in `create_session_inner`
  dies here too (the `harness_secret_id` path from Task 13 is the only one
  left).
- [ ] **Step 2:** Remove code + ship the drop migrations; fix compilation;
  update tests.
- [ ] **Step 3:** Fresh `just dev`; full gate sweep (Outcome). Commit:

```bash
git add crates/ deploy/ Tiltfile
git commit -m "feat(coordinator)!: shed human auth and identity — users table and owner-scoping leave Rust (ADR 0039 §5/§6)"
```

### Task 32: Remove the coordinator's legacy web routes

**Goal:** Delete the web-facing `/api/v1` axum routes. **Retain (verified —
the naive list is incomplete):** all **eight** internal ingest routes incl.
`/hosts/forge` and `/hosts/upload` (api/mod.rs:183-184); the broker-token
forge seam `/sessions/:id/git-credential` + `/sessions/:id/pull-request`
(api/mod.rs:216-221 — in-guest, NOT web routes despite the path shape);
`/healthz` + `/readyz`. Decide explicitly for the operator endpoints with no
RPC (`/admin/chunk-gc/candidates`, `/admin/reap-materialize-dir`): keep
behind the host bearer or delete with a note.

**Outcome:** Route table = host-ingest + broker-seam + health; handler bodies
survive as the `*_core` functions serving gRPC; `just check` + `just e2e` +
`just integration-session` + the CI lane green.

**Steps:**

- [ ] **Step 1:** Route-by-route deletion in `api/mod.rs:33-241` per the
  retain list; delete now-unreferenced axum shims (cores stay).
- [ ] **Step 2:** Clippy dead-code flags: remove, don't `allow`.
- [ ] **Step 3:** Full gates (Outcome). Commit:

```bash
git add crates/
git commit -m "feat(coordinator)!: shed the web-server identity — app surface is gRPC-only (ADR 0039 §1)"
```

---

## Final validation gate (after Task 32)

- [ ] `just check` — Rust gates green.
- [ ] `just smoke` — the umbrella: `smoke-control-plane` +
  `smoke-orchestrator` (parity is gone by design — deleted in Task 28).
- [ ] `pnpm -C web build && pnpm -C web test`; `pnpm -C orchestrator test && pnpm -C orchestrator typecheck`.
- [ ] `just e2e` — **the Phase 0 net, with its two sanctioned setup edits
  (better-auth entry, Task 22; precondition probe via `/rpc`, Task 28) and
  otherwise byte-for-byte the same assertions, green across three tiers.**
  This is the ADR's definition of done.
- [ ] **Visual sweep:** `just snap` over every baseline path as admin AND as a
  member; read all PNGs against `baseline/` (+ the member-baseline from
  Task 25). Accepted diffs: login page exists; Members page is the
  better-auth admin UI; everything else pixel-equivalent in structure.
- [ ] `just integration-session` + `deploy/dev/integration-test.sh` + the CI
  e2e lane — green post-Task-29 auth.
- [ ] **AuthZ negative checks (the boundary moved — test it like it's new):**
  unauthenticated `/rpc` → 401; member `GetSession` on another's session →
  404 (not 403 — anti-enumeration); member `ListHosts` → 403; a method
  missing from the policy map → denied; raw `grpcurl`-style call to the
  control plane without the bearer → `unauthenticated`; with the bearer →
  succeeds (documents that the control plane trusts the credential, per
  ADR §6's caveat).
- [ ] IAP: the Task 30 ES256 fixture smoke passes (staging verification is a
  deploy-checklist item).
- [ ] `buf breaking --against '.git#branch=main'` clean locally;
  `buf generate && git diff --exit-code -- web/src/gen orchestrator/src/gen`
  clean.
- [ ] Walk the revised ADR §2.3 tables + §3: every RPC exists and is reachable
  (passthrough or native); the task→session join is the only place ownership
  lives (15 min, by hand, dev stack).
