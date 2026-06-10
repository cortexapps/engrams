# ADR 0039 Implementation Plan — TypeScript Orchestration Tier

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development
> (recommended) or superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split the web-application layer out of the Rust coordinator into a new
TypeScript orchestration tier per [ADR 0039](adr/0039-typescript-orchestration-tier.md),
leaving user-observable session behavior unchanged (verified by a Playwright
characterization net written first).

**Architecture:** Three tiers. The Rust coordinator grows a tonic gRPC "app
contract" (ADR 0039 §2.3) and eventually sheds cookies/OIDC. A new `orchestrator/`
service (Hono on Node) owns human auth (better-auth), mints short-lived
forward-auth JWTs downstream, and forwards RPCs to the control plane through a
**generic Connect passthrough** — no hand-written per-endpoint handlers. The
browser keeps **SSE** for the event feed and **WebSocket** for the shell, both
now terminated at the orchestrator.

**Tech Stack:** Rust (tonic 0.12 / prost 0.13 — already used for the host
fabric), Buf (codegen + breaking-change CI), TypeScript on Node ≥ 22 (Hono,
`@hono/node-server` ≥ 1.13, `@hono/node-ws`, `@connectrpc/connect` v2 +
`@connectrpc/connect-node`, `@bufbuild/protobuf` v2, better-auth, drizzle-orm,
Postgres), React (`@connectrpc/connect-query` v2), Playwright.

**This plan was adversarially reviewed task-by-task before finalization;
corrections are inlined.** Where a step says "verified: X", a reviewer checked X
against the codebase or the installed library — don't re-litigate, but do trust
the compiler over the plan if they disagree.

---

## Amendments to ADR 0039 (decisions made in review, binding for this plan)

These supersede the corresponding ADR text:

1. **Runtime: Hono on Node, not Bun/Elysia** (amends §7, §8). DBOS — the
   intended fast-follow workflow engine (§3) — requires Node; Node's mature
   HTTP/2 client also de-risks the gRPC bidi shell leg. Hono is fetch-native
   and has first-class better-auth support.
2. **Generic passthrough, not hand-written RPCs** (amends §2.2/§2.3 reading).
   The UI-facing Connect surface and the control-plane tonic surface are the
   *same* generated service definitions. The orchestrator forwards them with
   one generic handler whose interceptor swaps cookie → forward-auth JWT.
   Bespoke orchestrator endpoints exist only where there is real aggregation —
   near-term that is exactly three: the SSE event leg, the WS shell leg, and
   the artifact byte route (browsers consume artifacts via `<img src>`, not
   RPCs).
3. **Browser event leg stays SSE** (amends §8/§9.3 primary choice; it was the
   ADR's recorded alternative). The `data:` payload is a small hand-built JSON
   envelope (`{idx, kind, payload_json}`) derived from the generated
   `SessionEvent` message; `EventSource` gives reconnect + `Last-Event-ID` for
   free.
4. **Event payload typing is deferred** (consistent with the ADR's deferred
   list). The proto envelope is `SessionEvent { idx, kind, payload_json }`,
   where `payload_json` is **normatively** the same JSON object today's SSE
   `data:` field carries — i.e. the event payload *with `_recovery_epoch` /
   `_rewound` folded in* (`api/events.rs:144` `with_rewind_meta`), for both
   replayed and live events. The existing discriminated union for payload
   shapes moves to a trimmed `web/src/events.ts` — the one hand-maintained
   type file that survives this plan — and is pinned by a fixture contract
   test (Task 24). Fully typed event protos ride with the transcript-shaping
   decision later.
5. **Shell double relay is kept for now**: browser WS → orchestrator →
   control-plane gRPC bidi → host-agent. The orchestrator's shell code is
   isolated in one module (`routes/shell.ts`) so a later swap to a
   direct/ticket model touches nothing else. The orchestrator-facing bidi RPC
   lives in its own `ShellRelayService` (not inside `SessionService`, and not
   named `ProxyShell` — avoids both the host-contract name collision and
   shipping a browser-uncallable method in the UI surface).
6. **`GetMe` joins the contract.** The ADR excludes `/me` from the gRPC
   contract, but the web's `AuthProvider` needs role / `has_claude_token` /
   admin-ness, which only the control plane knows (roles are authoritative
   there, §6). A self-scoped `UserService.GetMe` (no id parameter; answers for
   the asserted principal) replaces `/me`'s data half; better-auth replaces its
   authentication half.
7. **Module layout** is fixed (below); the `webhooks/`, `integrations/`, and
   `workflows/` directories are *reserved names* for the Slack/Linear/DBOS
   fast-follows and must not be created or stubbed in this plan (YAGNI).

## Out of scope (fast-follows, not in this plan)

DBOS and workflows (§3); Slack/Linear webhooks, the `connections` store, and
integrations (§4); per-ticket on-behalf-of attribution; typed event payloads /
server-side transcript shaping; **full rolling-deploy stream draining** (an ADR
"Implications" item — deliberately deferred: the event cursor + EventSource
reconnect make SSE restarts nearly free and shells are non-replayable anyway;
Task 15 ships basic SIGTERM-close only, and the residual drain work needs its
own plan).

## Orchestrator file layout (target state at end of Phase 3)

```
orchestrator/
  package.json  tsconfig.json  drizzle.config.ts  vitest.config.ts
  src/
    index.ts                # entry: config, server, listen, SIGTERM close
    server.ts               # one Node http server: /rpc → Connect adapter, else → Hono
    config.ts               # env: ORCHESTRATOR_DATABASE_URL, CONTROL_PLANE_GRPC_URL, ports
    auth/
      better-auth.ts        # better-auth instance: drizzle adapter + JWT plugin (serves JWKS)
      token.ts              # kAuthToken context key + per-request token supplier
      iap.ts                # IAP trusted-SSO bridge (Task 29)
    control-plane/
      transport.ts          # gRPC transport to tonic + bearer interceptor + h2 keepalives
      client.ts             # typed clients for orchestrator-initiated calls (SSE/WS/artifact legs)
    rpc/
      passthrough.ts        # the generic forwarder
      surface.ts            # the allowlist: which services the UI may reach
    routes/
      events.ts             # SSE browser leg ⇄ upstream StreamEvents
      artifacts.ts          # HTTP byte route ⇄ upstream GetArtifact stream
      shell.ts              # WS browser leg ⇄ upstream ShellRelayService bidi
      health.ts
    db/
      schema.ts  client.ts  # drizzle (better-auth tables)
    gen/                    # buf output — never hand-edited
```

## Sequencing

```
Phase 0 (e2e net, vs TODAY's stack)
   └─► Phase 1 (protos + codegen) ─► Phase 2 (coordinator tonic server)
                                  └► Phase 3 (orchestrator)
Phase 2 + 3 ─► Phase 4 (web migration; e2e re-pointed at better-auth)
Phase 4 ─► Phase 5 (scripts/CLI auth port → IAP bridge → coordinator auth
                    collapse → legacy route removal)
```

- Phase 0 must merge first — it is the safety net the ADR's Preparation section
  demands. **Gate protocol:** `just e2e` is run manually before merging any
  Phase ≥ 1 task (it needs a live stack, so it is deliberately not in CI); the
  person merging runs it. In-flight UI redesign branches must carry the Phase 0
  `data-testid`s forward — the testids are the contract, the components aren't.
- Phases 2 and 3 can proceed in parallel after Phase 1. Phase 3 tasks each have
  an **in-process fake-upstream test** (needs only Phase 1) and a **live
  Outcome** (needs a specific Phase 2 task: Task 19 ← Task 10, Task 20 ← Task 11,
  Task 21 ← Task 13, artifacts ← Task 12). When running phases in parallel,
  stop at the fake test and defer the live Outcome.
- **Deploy posture:** Tasks 22–30 are a single non-deployable span for any
  OIDC/IAP production environment. The intermediate states are runnable **in
  dev only**, because the coordinator keeps `AuthMode::None` + SyntheticAdmin
  until Task 30: between Tasks 23 and 27, sessions created via `/rpc` are owned
  by the better-auth JIT-upserted user while events/shell still ride the
  coordinator under SyntheticAdmin — coherent only because the synthetic
  principal is an admin who sees every session. Don't "fix" this mid-phase.

Dev ports (existing + new): web `:5173`, coordinator HTTP `127.0.0.1:8090`,
Postgres `localhost:5435`, **coordinator app-gRPC `127.0.0.1:50061` (new)**,
**orchestrator HTTP `127.0.0.1:8787` (new)**.

---

# Phase 0 — Characterization net (against the current two-tier stack)

Implements the ADR's **Preparation** section. Runs against `just dev` today
(SyntheticAdmin, no login); Task 22 later re-points only the entry step, and
Task 27 the precondition probe.

### Task 1: Playwright scaffold + precondition gate

**Goal:** `pnpm e2e` exists in `web/`, fails fast with a fix-it line per missing
precondition (web up, control plane healthy, a **no-harness demo image**
enabled).

**Outcome:** With the stack down, `cd web && pnpm e2e` exits non-zero printing
the `run \`just dev\` first` fix-it. With the stack up and preconditions met it
reaches Playwright's `Error: No tests found` (exit 1 — expected until Task 2;
verified that globalSetup runs *before* the no-tests check, so the gate is
exercised either way).

**Files:**
- Modify: `web/package.json` (devDependency `@playwright/test`, script `"e2e": "playwright test"`)
- Create: `web/playwright.config.ts`
- Create: `web/e2e/global-setup.ts`
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
  // must fit inside this, or a slow boot dies as a generic test-timeout
  // instead of the targeted assertion message.
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

- [ ] **Step 3:** Create `web/e2e/global-setup.ts`. Each precondition fails fast
  with its own fix-it line. Note the third check: not "any image" — a
  **no-harness** image (`harness_name === null`), because the journey must not
  require a Claude token (ADR Preparation):

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
  // Shape: ListEnabledImagesResponse (web/src/types.ts:500) — { images: EnabledImageSummary[] }
  const body = (await res.json()) as { images?: { image_uri: string; harness_name: string | null }[] };
  if (!body.images?.some((i) => i.harness_name === null)) {
    throw new Error('no NO-HARNESS image enabled — run `just integration-session` once (bakes+enables the demo image)');
  }
}
```

- [ ] **Step 4:** Fix `deploy/dev/integration-session.sh` for macOS/arm64 dev
  boxes: it hardcodes `cargo build --target x86_64-unknown-linux-musl -p engram-agentd`
  (lines 52-53), which bakes an unbootable image on the VZ/arm64 backend. Port
  the arch detection from `deploy/dev/bake-demo.sh:45-57` (which already does
  this right). Verify by running `just integration-session` on this machine and
  confirming the baked image boots.
- [ ] **Step 5:** Run `cd web && pnpm e2e` with the stack **down** → exit 1 with
  the fix-it line. Then with the stack up (`just dev`, plus
  `just integration-session` once if the gate demands it) → globalSetup passes,
  then `Error: No tests found`, exit 1 — both expected.
- [ ] **Step 6:** Commit:

```bash
git add web/package.json web/pnpm-lock.yaml web/playwright.config.ts web/e2e/global-setup.ts deploy/dev/integration-session.sh
git commit -m "test(e2e): playwright scaffold + stack precondition gate (ADR 0039 prep)"
```

### Task 2: The journey spec + `data-testid`s + `just e2e`

**Goal:** Pin the ADR's single linear journey: land with no login → `+ new
session` → select the demo image → `start` → URL `/sessions/<id>` → event count
climbs above zero → status advances toward `active` → RAW tab renders event
rows → SHELL tab's WebSocket connects → new row appears in the session list.

**Outcome:** `just e2e` passes against `just dev` on the no-harness demo image.
Selectors use `data-testid`, immune to copy drift.

**Files** (testids live where the elements actually render — verified):
- Modify: `web/src/pages/Sessions.tsx` — `data-testid="new-session"` on the
  new-session button.
- Modify: `web/src/components/NewSessionForm.tsx` — `data-testid="image-select"`
  on the image `<select>` (`NewSessionForm.tsx:168-179` — it is a native
  select; Playwright cannot click `<option>`s, use `selectOption`), and
  `data-testid="start-session"` on the submit button. **Caution:** when the
  selected image has a Claude harness and no token is saved, the submit button
  is replaced by a Link (`NewSessionForm.tsx:260-269`) — selecting the demo
  image first avoids this entirely.
- Modify: `web/src/components/SessionManifest.tsx` — `data-testid="session-row"`
  on each `SessionRow`.
- Modify: `web/src/components/TabRow.tsx` — `data-testid={`tab-${t.id}`}` on the
  tab buttons (SessionDetail's tabs render through this shared component).
- Modify: `web/src/pages/SessionDetail.tsx` — wrap the `{events.length}` in the
  header prose (`SessionDetail.tsx:81`) in its own
  `<span data-testid="event-count">{events.length}</span>` so `textContent` is
  a bare number (tagging the surrounding prose makes `Number(textContent)`
  return `NaN` and the poll can never pass); `data-testid="session-status"` on
  the status chip; `data-testid="event-row"` on each raw event row.
- Create: `web/e2e/session-journey.spec.ts`
- Modify: `justfile` — `e2e` recipe.

**Steps:**

- [ ] **Step 1:** Add the testids above. Inert attributes; `pnpm -C web test`
  still green.
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

  // SHELL tab's WebSocket connects. Predicate-filter: vite's HMR websocket
  // would otherwise be captured. 'websocket' fires on creation, so also wait
  // for a received frame (ttyd handshakes promptly) to pin "connected".
  const wsPromise = page.waitForEvent('websocket', {
    predicate: (ws) => ws.url().includes('/shell'),
    timeout: 30_000,
  });
  await page.getByTestId('tab-shell').click();
  const ws = await wsPromise;
  await ws.waitForEvent('framereceived', { timeout: 15_000 });

  // The new row appears in the session list.
  await page.goto('/');
  await expect(page.getByTestId('session-row').filter({ hasText: id.slice(0, 8) })).toBeVisible();
});
```

  The demo-image option filter and the status regex are the two places reality
  may differ — adjust to what the UI renders (read `NewSessionForm.tsx` /
  `SessionDetail.tsx`), keeping the *assertions* intact.
- [ ] **Step 3:** Run against a live stack: `just dev`, then `cd web && pnpm e2e`
  → PASS.
- [ ] **Step 4:** Add to `justfile` (near `integration-session`, justfile:~204):

```make
# ADR 0039 characterization net. Requires `just dev` running and the
# no-harness demo image enabled (`just integration-session` once).
# Run this manually before merging any task of the ADR 0039 plan.
e2e:
    cd web && pnpm e2e
```

- [ ] **Step 5:** `just e2e` → PASS. Commit:

```bash
git add web/src justfile web/e2e/session-journey.spec.ts
git commit -m "test(e2e): pin the session journey — the ADR 0039 characterization net"
```

---

# Phase 1 — The contract (protos + codegen)

### Task 3: Buf tooling — lint, breaking-change CI, `just gen-proto`

**Goal:** `buf` owns the proto tree; CI fails on breaking changes; one command
regenerates all TS bindings.

**Outcome:** `buf lint` and a local `buf breaking --against '.git#branch=main'`
run clean; the CI job is green on the PR that adds it.

**Files:**
- Create: `crates/engram-protocol/proto/buf.yaml`
- Create: `buf.gen.yaml` (repo root)
- Modify: `justfile` (`gen-proto`), the existing CI workflow (one new job)

**Steps:**

- [ ] **Step 1:** Install buf (`brew install bufbuild/buf/buf`, or via the Nix
  devshell if `flake.nix` provides it).
- [ ] **Step 2:** Create `crates/engram-protocol/proto/buf.yaml`. Verified:
  `host_service.proto` fails **PACKAGE_DIRECTORY_MATCH**,
  **RPC_REQUEST_RESPONSE_UNIQUE**, **RPC_REQUEST_STANDARD_NAME**, and
  **RPC_RESPONSE_STANDARD_NAME** under STANDARD — scope the exceptions to that
  legacy file only, so the new app files stay fully strict:

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
- [ ] **Step 4:** Create repo-root `buf.gen.yaml`. Two plugins: `es` (messages +
  service descriptors) and `query-es` (per-method connect-query exports, web
  only). `paths` values are cwd-relative and include the input-directory prefix
  (verified empirically):

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

- [ ] **Step 5:** `justfile`:

```make
# Regenerate TS protobuf bindings (web + orchestrator). Rust regenerates
# via build.rs.
gen-proto:
    buf generate
```

- [ ] **Step 6:** CI job in the existing workflow. Two corrections vs the naive
  setup (verified): buf-action runs `buf format` by default and the legacy
  proto fails it — disable; and `.git#branch=main` doesn't resolve on PR
  runners (origin refs) — use the action's default PR-base behavior:

```yaml
  buf:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: bufbuild/buf-action@v1
        with:
          input: crates/engram-protocol/proto
          format: false        # host_service.proto predates buf format
      # Generated-code drift gate (web/src/gen + orchestrator/src/gen are committed):
      - run: buf generate && git diff --exit-code -- web/src/gen orchestrator/src/gen
```

- [ ] **Step 7:** Exclude `src/gen` from web's lint/test globs if eslint or
  vitest pick it up (check `web/eslint.config.*` and `vite.config.ts` test
  include).
- [ ] **Step 8:** Commit:

```bash
git add crates/engram-protocol/proto/buf.yaml buf.gen.yaml justfile .github/workflows/
git commit -m "build(proto): buf lint + breaking-change CI + gen-proto recipe (ADR 0039 §7)"
```

### Task 4: `engram/app/v1/session.proto` — SessionService + ShellRelayService

**Goal:** The orchestrator-facing session contract (ADR §2.3 first table) as
protobuf, lint-clean under STANDARD (files live at `proto/engram/app/v1/` to
satisfy PACKAGE_DIRECTORY_MATCH; every RPC gets a unique wrapped
`XxxRequest`/`XxxResponse`).

**Outcome:** `buf lint` clean; every RPC from the ADR's SessionService table
exists. Message fields use snake_case names mirroring `web/src/types.ts`
field-for-field. (Note: this does **not** make generated TS or proto-JSON match
today's wire format — generated TS is camelCase and that churn is paid in
Task 23. The snake_case is simply proto convention + a 1:1 transcription aid.)

**Files:**
- Create: `crates/engram-protocol/proto/engram/app/v1/session.proto`

**Steps:**

- [ ] **Step 1:** Create the file — service blocks complete, load-bearing
  messages complete, the rest transcribed in Step 2:

```proto
syntax = "proto3";

package engram.app.v1;

// The orchestrator-facing app contract (ADR 0039 §2.3). One RPC per
// current /api/v1 session route. Forwarded verbatim to the UI by the
// orchestrator's generic passthrough.
service SessionService {
  rpc ListSessions(ListSessionsRequest) returns (ListSessionsResponse);
  rpc CreateSession(CreateSessionRequest) returns (CreateSessionResponse);
  rpc GetSession(GetSessionRequest) returns (GetSessionResponse);
  rpc DeleteSession(DeleteSessionRequest) returns (DeleteSessionResponse);
  rpc SendPrompt(SendPromptRequest) returns (SendPromptResponse);
  rpc Interrupt(InterruptRequest) returns (InterruptResponse);
  // Server-streaming replacement for SSE GET /sessions/:id/events.
  rpc StreamEvents(StreamEventsRequest) returns (stream SessionEvent);
  // Replaces POST /sessions/:id/exec[/stream]. One streaming RPC; the
  // unary HTTP route's semantics are the degenerate collected case.
  rpc Exec(ExecRequest) returns (stream ExecOutput);
  rpc GetLog(GetLogRequest) returns (GetLogResponse);
  rpc Snapshot(SnapshotRequest) returns (SnapshotResponse);
  rpc Resume(ResumeRequest) returns (ResumeResponse);
  rpc EvictLocal(EvictLocalRequest) returns (EvictLocalResponse);
  rpc GetCowState(GetCowStateRequest) returns (GetCowStateResponse);
  rpc ListCheckpoints(ListCheckpointsRequest) returns (ListCheckpointsResponse);
  // STREAMING: artifacts are up to 512 MiB (MAX_ARTIFACT_BYTES) and the
  // browser consumes them as <img src> bytes — a unary response would
  // blow the 4 MiB message cap. First message carries metadata only.
  rpc GetArtifact(GetArtifactRequest) returns (stream GetArtifactResponse);
  rpc CreateArtifactFromPath(CreateArtifactFromPathRequest) returns (CreateArtifactFromPathResponse);
}

// The browser-shell relay (ADR §8), its own service: keeps the bidi
// (browser-uncallable) method out of SessionService's UI surface, and
// avoids colliding with host_service.proto's ProxyShell.
service ShellRelayService {
  rpc Relay(stream RelayShellRequest) returns (stream RelayShellResponse);
}

message ListSessionsRequest {
  // "mine" (default) or "all" (admin-gated in the control plane).
  string scope = 1;
}

// The typed event ENVELOPE (plan amendment 4). payload_json is
// NORMATIVELY the JSON object today's SSE `data:` carries: the event
// payload with _recovery_epoch/_rewound folded in via with_rewind_meta
// (api/events.rs:144) — on BOTH the replay and live arms.
message SessionEvent {
  // optional: a broadcast-lag notification has no idx (today's SSE emits
  // `event: lagged` with NO id: line so it never disturbs Last-Event-ID;
  // the proxy must preserve that — see kind below).
  optional int64 idx = 1;
  // The event's "type" discriminant. Special value "lagged": the
  // subscriber missed events; idx is unset; payload_json = {"missed": n}.
  string kind = 2;
  string payload_json = 3;
}

message StreamEventsRequest {
  string session_id = 1;
  // Resume cursor: replay strictly-after this idx, then tail.
  // UNSET = from the start (callers translate the HTTP -1 sentinel to
  // unset; proto3 default 0 would silently skip idx 0).
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

// Frame kinds mirror the WS frames / host ProxyShellMessage 1:1.
// Request and response carry the same set; defined separately to satisfy
// RPC_REQUEST_RESPONSE_UNIQUE and to mark directionality.
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

- [ ] **Step 2:** Transcribe the remaining messages **field-for-field** from the
  authoritative sources below. Rules: same snake_case name per field;
  `T | null` / optional → `optional`; arrays → `repeated`;
  `Record<string,string>` → `map<string, string>`; string-literal unions (e.g.
  `SessionState`) → `string` (NOT proto enums); internally-tagged serde unions
  → `oneof` (see `AddRegistryAuth` note in Task 5). Wrap top-level entities in
  the response message (`GetSessionResponse { Session session = 1; }`).

  | Proto message | Transcribe from |
  |---|---|
  | `Session`, `ListSessionsResponse` | `web/src/types.ts:70` (`Session`), `:84` (`SessionListItem` — fold its extras into `Session` as `optional`), `:92` |
  | `CreateSessionRequest` | `CreateSessionInput`, `web/src/api.ts:205` (note `secrets` → `map<string,string>`) |
  | `CreateSessionResponse` | `web/src/types.ts:232` |
  | `SendPrompt*` / `Interrupt*` | serde structs at `crates/engram-coordinator/src/api/prompt.rs:21/26` and `api/interrupt.rs:26` |
  | `ExecRequest`, `ExecOutput` | **`engram_core::types::ExecEvent`** (imported at `api/exec.rs:18`) is the authoritative streaming wire type — `web/src/types.ts:242`'s `ExecRusage` is explicitly non-exhaustive; the unary shapes at `api/exec.rs:27/39` inform `ExecRequest` |
  | `GetLog*` | the `/sessions/:id/log` handler in `api/sessions_inspect.rs` |
  | `Snapshot/Resume/EvictLocal` | `api/snapshot.rs` handlers |
  | `GetCowStateResponse` | wraps `web/src/types.ts:217` (+ `CowStateView` `:159`, `DurabilityRow` `:194`) |
  | `ListCheckpointsResponse` | wraps `web/src/types.ts:370` + `CheckpointSummary` `:358` |
  | `CreateArtifactFromPath*` | the artifact-from-path handler (ADR 0026; `git grep -n "artifact" crates/engram-coordinator/src/api/` to locate) |
  | `GetSessionRequest`/`DeleteSession*` | `{ string session_id = 1; }`; delete response mirrors `api/sessions.rs:1277` |

- [ ] **Step 3:** `buf lint` → clean (the new file must pass full STANDARD — no
  new `ignore_only` entries).
- [ ] **Step 4:** Commit:

```bash
git add crates/engram-protocol/proto/engram/app/v1/session.proto
git commit -m "feat(proto): app/v1 SessionService + ShellRelayService (ADR 0039 §2.3)"
```

### Task 5: `fleet.proto`, `image.proto`, `user.proto`

**Goal:** The remaining services from ADR §2.3 + the `GetMe` amendment, same
rules as Task 4.

**Outcome:** `buf lint` clean; all RPCs exist incl. `GetMe`.

**Files:**
- Create: `crates/engram-protocol/proto/engram/app/v1/{fleet,image,user}.proto`

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
  // ADR-0018 cordon+evacuate — the admin POST /admin/hosts/:id/drain.
  // Two distinct semantics today; kept distinct here.
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

  Each GC request carries `bool dry_run = 1` (today: two routes; here: one
  flag). Transcribe `HostView` from `web/src/types.ts:130`,
  `ListHostsResponse` `:149`, `StorageSummaryResponse` `:206`,
  `HostCowStateResponse` from `api/hosts.rs:67`; admin/GC bodies from
  `api/admin.rs`, `api/hosts.rs`, `api/storage.rs`.
  **Intentional drops (record, don't port):** `GET /admin/chunk-gc/candidates`
  and `/admin/reap-materialize-dir` get no RPC — no web caller today; they are
  operator endpoints that Task 31 must keep or consciously delete (noted
  there).
- [ ] **Step 2:** `image.proto` — RPCs: `ListEnabledImages`, `EnableImage`,
  `DisableImage`, `RefreshImage`, `ListEnableJobs`, `GetEnableJob`,
  `RetryEnableJob`, `ListRegistries`, `AddRegistry`, `DeleteRegistry`, each
  with wrapped Request/Response. Transcribe from `web/src/types.ts`
  (`EnabledImageSummary :455`, `EnableJob :483`, `ListEnableJobsResponse :496`,
  `ListEnabledImagesResponse :500`, `AddRegistryRequest :411`,
  `AddRegistryResponse :416`, `RegistryCredentialSummary :428`,
  `ListRegistriesResponse :437`) and `api/enabled_images.rs` /
  `api/registries.rs`. **`AddRegistryAuth` (`types.ts:406`) is an
  internally-tagged serde union** — map it to
  `oneof auth { StaticAuth static = 1; GcpWorkloadIdentityAuth gcp_workload_identity = 2; AnonymousAuth anonymous = 3; }`
  (exact variant names from the Rust enum in `api/registries.rs`); the Task 14
  convert layer maps the serde enum explicitly.
- [ ] **Step 3:** `user.proto`:

```proto
syntax = "proto3";
package engram.app.v1;

service UserService {
  rpc ListUsers(ListUsersRequest) returns (ListUsersResponse);     // admin-gated
  rpc PatchUser(PatchUserRequest) returns (PatchUserResponse);     // admin-gated; role authoritative here (ADR 0031)
  // SELF-scoped: answers for the asserted principal (no id param).
  // Replaces GET /me's data half (role, admin-ness, has_claude_token) —
  // plan amendment 6. better-auth replaces its authentication half.
  rpc GetMe(GetMeRequest) returns (GetMeResponse);
  // The storage half of POST /me/claude-token: the orchestrator relays
  // the raw token OPAQUELY — never logged, never persisted there. KEK
  // sealing + CreateSession auto-injection stay in the control plane.
  rpc SaveClaudeToken(SaveClaudeTokenRequest) returns (SaveClaudeTokenResponse);
}

message GetMeRequest {}

message SaveClaudeTokenRequest {
  string token = 1;
}
```

  Transcribe `AdminUser` from `web/src/types.ts:119`; `GetMeResponse` from the
  web `Principal` (`types.ts:101`) minus the orchestrator-owned bits
  (`can_sign_out` is computed by the orchestrator, not asserted by the control
  plane); `PatchUserRequest` from `api/principal.rs:512` (**note: the user
  handlers live in `api/principal.rs`, not `api/admin.rs`** — `list_users`
  `:501`, `patch_user` `:521`, `save_claude_token` `:296`).
- [ ] **Step 4:** `buf lint` → clean. Commit:

```bash
git add crates/engram-protocol/proto/engram/app/v1/
git commit -m "feat(proto): app/v1 Fleet/Image/User services + GetMe (ADR 0039 §2.3)"
```

### Task 6: Rust bindings for `engram.app.v1`

**Goal:** tonic server + client types compile inside `engram-protocol`.

**Outcome:** `cargo build -p engram-protocol` succeeds;
`engram_protocol::app::session_service_server::SessionService` is nameable.

**Files:**
- Modify: `crates/engram-protocol/build.rs`
- Create: `crates/engram-protocol/src/app.rs`
- Modify: `crates/engram-protocol/src/lib.rs` (`pub mod app;`)

**Steps:**

- [ ] **Step 1:** Extend the proto list in `build.rs`:

```rust
let protos = [
    "proto/host_service.proto",
    "proto/engram/app/v1/session.proto",
    "proto/engram/app/v1/fleet.proto",
    "proto/engram/app/v1/image.proto",
    "proto/engram/app/v1/user.proto",
];
```

- [ ] **Step 2:** `src/app.rs`, mirroring `src/grpc.rs:14`'s include pattern:

```rust
//! Generated bindings for the orchestrator-facing app contract
//! (ADR 0039 §2.3): engram.app.v1.
tonic::include_proto!("engram.app.v1");
```

- [ ] **Step 3:** `pub mod app;` in `src/lib.rs`; `cargo build -p engram-protocol`
  → success.
- [ ] **Step 4:** Commit:

```bash
git add crates/engram-protocol/
git commit -m "feat(proto): rust tonic bindings for engram.app.v1"
```

### Task 7: TS bindings generation

**Goal:** `just gen-proto` emits committed TS bindings into `web/src/gen` and
`orchestrator/src/gen` (paths include the proto dir structure:
`web/src/gen/engram/app/v1/session_pb.ts`).

**Outcome:** Generated files exist and export the service descriptors
(`SessionService` etc.) plus connect-query method exports for web;
`pnpm -C web build` clean (gen is additive).

**Steps:**

- [ ] **Step 1:** `cd web && pnpm add @bufbuild/protobuf@^2 @connectrpc/connect@^2 @connectrpc/connect-query@^2`
  (runtime deps of the generated code).
- [ ] **Step 2:** `just gen-proto` → files under `web/src/gen/engram/app/v1/`
  (both `*_pb.ts` and `*-SessionService_connectquery.ts`) and
  `orchestrator/src/gen/engram/app/v1/` (the orchestrator package doesn't exist
  yet — the directory is inert until Task 15; that's fine, it's committed so
  the CI drift gate covers it).
- [ ] **Step 3:** `pnpm -C web build` → clean. Commit:

```bash
git add web/src/gen orchestrator/src/gen web/package.json web/pnpm-lock.yaml
git commit -m "feat(proto): generated connect-es + connect-query bindings for app/v1"
```

---

# Phase 2 — Control-plane gRPC server (coordinator)

### Task 8: tonic app-server scaffold

**Goal:** The coordinator serves tonic on `APP_GRPC_ADDR` alongside axum, all
services registered as `UNIMPLEMENTED` stubs, with graceful shutdown and HTTP/2
keepalives (ADR §9.4) from day one.

**Outcome:** A Rust smoke test gets `Code::Unimplemented` from `ListSessions`;
`just e2e` still green.

**Files:**
- Create: `crates/engram-coordinator/src/grpc_app/mod.rs`
- Modify: `crates/engram-coordinator/src/main.rs` (clap flag),
  `src/config.rs` (thread the addr through `CoordinatorConfig`),
  `src/lib.rs` (spawn — see Step 3)

**Steps:**

- [ ] **Step 1:** Clap arg in `main.rs` (pattern-match `bind_addr`, main.rs:22):

```rust
/// Address the orchestrator-facing app gRPC server binds to (ADR 0039).
#[arg(long, env = "APP_GRPC_ADDR", default_value = "127.0.0.1:50061")]
app_grpc_addr: std::net::SocketAddr,
```

  Thread it into `CoordinatorConfig` (config.rs) like the other addrs.
- [ ] **Step 2:** `grpc_app/mod.rs`. **Note: the shared state handle is
  `SharedState = Arc<AppState>` (state.rs:373/:543) — `AppState` itself is not
  `Clone`:**

```rust
//! Orchestrator-facing app gRPC surface (ADR 0039 §2.3). Lives beside
//! the axum API during the migration; the axum web routes retire in
//! Phase 5.
use engram_protocol::app;
use tonic::{Request, Response, Status};

pub struct AppSessionService {
    pub state: crate::state::SharedState,
}

#[tonic::async_trait]
impl app::session_service_server::SessionService for AppSessionService {
    async fn list_sessions(
        &self,
        _req: Request<app::ListSessionsRequest>,
    ) -> Result<Response<app::ListSessionsResponse>, Status> {
        Err(Status::unimplemented("ADR 0039 phase 2"))
    }
    // ... a stub per RPC (the compiler enumerates them). For the
    // streaming RPCs define the associated types, e.g.:
    // type StreamEventsStream = std::pin::Pin<Box<dyn tokio_stream::Stream<
    //     Item = Result<app::SessionEvent, Status>> + Send>>;
    // and return Err(unimplemented) before yielding anything.
}
```

  Same for `AppFleetService`, `AppImageService`, `AppUserService`,
  `AppShellRelayService`.
- [ ] **Step 3:** Spawn the server **inside `run_with_registry_and_local`**
  (lib.rs — `AppState` is constructed at lib.rs:132-135; `main.rs` never sees
  it), right after `let state = Arc::new(app)`, sharing the same shutdown
  signal the axum side uses (`with_graceful_shutdown(shutdown_signal())` at
  lib.rs:369):

```rust
let app_grpc = tonic::transport::Server::builder()
    // ADR §9.4: HTTP/2 keepalive PINGs so a dead orchestrator's streams
    // are detected and torn down (releases leases/subscriptions).
    .http2_keepalive_interval(Some(std::time::Duration::from_secs(20)))
    .http2_keepalive_timeout(Some(std::time::Duration::from_secs(10)))
    .add_service(app::session_service_server::SessionServiceServer::new(
        grpc_app::AppSessionService { state: state.clone() },
    ))
    // ... the other four services
    .serve_with_shutdown(cfg.app_grpc_addr, shutdown_signal());
tokio::spawn(async move {
    if let Err(e) = app_grpc.await {
        tracing::error!(error = %e, "app gRPC server exited");
    }
});
```

- [ ] **Step 4:** Smoke test. `crates/engram-coordinator/tests/api.rs` already
  builds a full `AppState` from mocks (`MockMetadataStore`, `MockCloud`,
  `ProcessBackend`, `InMemorySecretStore`, `AppState::new(cfg, services)`) —
  but those helpers live inside that test binary and are **not importable**
  from a new test file. Extract the state-builder into a shared
  `#[cfg(test)]`-free test-support module (e.g. `src/test_support.rs` behind a
  `test-support` feature) or duplicate the minimal builder in
  `tests/grpc_app.rs`. The test: serve on an ephemeral port, tonic client
  calls `list_sessions`, assert `Code::Unimplemented`. (No `grpcurl`: tonic
  doesn't serve reflection by default and we don't add `tonic-reflection` for
  this.)
- [ ] **Step 5:** `just check` and `just e2e` green. Commit:

```bash
git add crates/engram-coordinator/
git commit -m "feat(coordinator): tonic app-gRPC scaffold with keepalives + graceful shutdown (ADR 0039 §2)"
```

### Task 9: gRPC forward-auth — JWT → Principal

**Goal:** Every app-gRPC call authenticates a `Bearer` JWT via the **existing
auth chain** — verification, JIT upsert, `bootstrap_admins` promotion, and the
inactive-user gate all come from `engram-auth`, not a reimplementation.

**Outcome:** With a test JWKS: a signed JWT yields a `Principal` whose role
comes from the `users` table; missing/expired/garbage → `unauthenticated`;
an inactive user → `permission_denied`.

**Files:**
- Create: `crates/engram-coordinator/src/grpc_app/auth.rs`
- Modify: `main.rs`/`config.rs` (flags `--app-auth-jwks-url`,
  `--app-auth-issuer`, `--app-auth-audience`; envs `APP_AUTH_*`)
- Modify: `Tiltfile` — add `APP_AUTH_JWKS_URL=http://127.0.0.1:8787/api/auth/jwks`
  (+ issuer/audience) to `coord_env` **now**, so Phase 3/4 live checks work
  without a later wiring task

**Steps:**

- [ ] **Step 1:** Read the real APIs first — the shapes matter (verified):
  - `ForwardAuthVerifier` implements
    `IdentityVerifier::verify(&self, input: &VerifyInput) -> Result<Option<Verified>, AuthError>`
    (forward.rs:59). It reads the assertion from a **configured header name**
    (`cfg.header`) out of `VerifyInput { headers, cookies }` — it does NOT
    take a token string and does NOT strip `Bearer `.
  - The JIT upsert is `VerifierChain::jit_upsert` (chain.rs:74), reached via
    `chain.resolve()` — **not** in forward.rs and not in `build_chain`
    (config.rs:122 only assembles the chain).
- [ ] **Step 2:** Failing tests first, in `grpc_app/auth.rs`'s test module:
  reuse `forward.rs`'s own test fixtures if present (read its tests); otherwise
  mint **RS256** JWTs (the same alg Task 17 pins on the orchestrator — keep
  the two in lockstep so the suites exercise the real pairing) against a local
  JWKS served from a test listener. Tests: valid → Principal with that email;
  expired → `unauthenticated`; inactive user row → `permission_denied`.
- [ ] **Step 3:** Implement `GrpcAuth` holding a `VerifierChain` built via
  `engram_auth::build_chain` with a ForwardAuth-only `AuthConfig`:

```rust
//! Bearer-JWT → Principal for the app-gRPC surface. The orchestrator is
//! just another trusted forward-auth upstream (ADR 0039 §5); we build a
//! ForwardAuth-only VerifierChain so JIT upsert, bootstrap_admins, and
//! the inactive gate are the same code the axum chain runs.
//
// Why a per-RPC helper and not a tonic interceptor: tonic interceptors
// are synchronous; this path does an async JWKS fetch + a DB upsert.
// Do not "DRY this up" into an interceptor — it cannot work there.
impl GrpcAuth {
    pub async fn principal_from_metadata<T>(
        &self,
        req: &tonic::Request<T>,
    ) -> Result<Principal, tonic::Status> {
        let token = req
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| tonic::Status::unauthenticated("missing bearer token"))?;
        let input = VerifyInput {
            headers: [(self.header_name.clone(), token.to_string())].into(),
            cookies: Default::default(),
        };
        match self.chain.resolve(&input).await {
            Ok(Some(principal)) => Ok(principal),
            Ok(None) => Err(tonic::Status::unauthenticated("invalid assertion")),
            Err(AuthError::Inactive) => Err(tonic::Status::permission_denied("inactive user")),
            Err(_) => Err(tonic::Status::unauthenticated("invalid assertion")),
        }
    }
}
```

  (Adapt names to the actual `VerifierChain` API — Step 1 is the map.)
  **Lazy verification:** the JWKS is fetched per-validation/cached, never at
  boot — the coordinator must start fine when the orchestrator isn't up yet
  (Phases 2∥3). If the config flags are unset, the gRPC surface rejects all
  calls with `unauthenticated` (fail closed), it does not crash.
- [ ] **Step 4:** Tests green (`just test-pkg engram-coordinator`); thread
  `GrpcAuth` into the service structs; call it in `list_sessions` (still
  returning unimplemented after auth). `just check` green.
- [ ] **Step 5:** Commit:

```bash
git add crates/engram-coordinator/ Tiltfile
git commit -m "feat(coordinator): forward-auth on app-gRPC via the existing VerifierChain"
```

### Task 10: SessionService core — extract-and-delegate for the unary six

**Goal:** `ListSessions`, `CreateSession`, `GetSession`, `DeleteSession`,
`SendPrompt`, `Interrupt` over gRPC by extracting transport-agnostic cores that
axum and tonic both call. **The critical subtlety: today the owner check is
route-layer middleware (`require_session_owner`, api/mod.rs:42-75), so the
handler bodies for get/delete/prompt/interrupt contain ZERO authz** — naively
extracting "the exact body" ships an authz hole where any member can read,
delete, or prompt any session. The cores must carry the check themselves.

**Outcome:** gRPC integration test: create → list → get → delete works; **a
member-role JWT calling `GetSession` on another user's session gets
`not_found`** (the anti-enumeration semantics below); `just e2e` green.

**Sizing note:** land as ~3 PRs: (a) owner-check core + `into_status` +
converters + list/get/delete, (b) `create_session`, (c) prompt + interrupt.

**Files:**
- Modify: `crates/engram-coordinator/src/api/sessions.rs`
  (`create_session:498`, `get_session:1176`, `list_sessions:1213`,
  `delete_session:1277`), `api/prompt.rs`, `api/interrupt.rs`,
  `api/principal.rs`, `src/error.rs`
- Create: `crates/engram-coordinator/src/grpc_app/convert.rs`

**Steps:**

- [ ] **Step 1:** Build the shared authz core FIRST, replicating
  `require_session_owner` (`api/principal.rs:177`) **exactly**: admin bypasses;
  owner-mismatch and not-found are BOTH `NotFound("session not found")` — 404,
  never 403, so existence can't be probed. Have it return the session row
  (saves a second lookup):

```rust
/// gRPC-path equivalent of the require_session_owner middleware.
/// EVERY session-scoped core starts with this. 404 on mismatch — never
/// 403 (anti-enumeration, same as the middleware).
pub(crate) async fn require_session_owner_core(
    state: &SharedState,
    principal: &Principal,
    session_id: &str,
) -> Result<Session, ApiError> { /* mirror principal.rs:177 */ }
```

- [ ] **Step 2:** `into_status` in `grpc_app/mod.rs` — **exhaustive** over
  `ApiError`'s 12 variants (`src/error.rs`), not a 4-arm sketch:
  `NotFound→not_found`, `Forbidden→permission_denied`,
  `Unauthorized→unauthenticated`, `BadRequest→invalid_argument`,
  `Conflict→failed_precondition` (interrupt/prompt "no live sandbox"),
  `Gone/HostLost→failed_precondition`, `Unavailable→unavailable` (capacity,
  retryable), `Unsupported→unimplemented`,
  `PayloadTooLarge/TooManyRequests→resource_exhausted`, rest → `internal`.
  Additionally: make `ApiError::slug()` `pub` and attach it as
  `engram-error-slug` metadata on the `Status` — the web distinguishes slugs
  that share an HTTP code (e.g. `snapshot_invalidated` vs `host_lost`, both
  410) and the orchestrator/UI needs it to reconstruct today's error envelope.
- [ ] **Step 3:** Extraction template — `list_sessions` first. Real signatures
  (verified): handlers use the `CurrentUser` extractor (not
  `Extension<Principal>`), and the list query is
  `ListSessionsParams { scope: Option<String> }` (sessions.rs:1201):

```rust
pub(crate) async fn list_sessions_core(
    state: &SharedState,
    principal: &Principal,
    scope: Option<&str>,
) -> Result<ListSessionsResponse, ApiError> { /* the body, extractors unwrapped */ }
```

  Axum shim calls the core; tonic method = `principal_from_metadata` →
  `convert` → core → `convert`.
- [ ] **Step 4:** Repeat for the rest. Per-handler notes:
  - `create_session` is a metrics wrapper around `create_session_inner`
    (sessions.rs:497-534) — extract the core *inside* the wrapper so gRPC
    creations are counted too.
  - `get_session`/`delete_session`/prompt/interrupt cores start with
    `require_session_owner_core` (Step 1) — their axum bodies have no check
    to extract.
  - prompt: the auto-resume-on-idle behavior must live in the core, not the
    axum shim.
- [ ] **Step 5:** `convert.rs` mappers — dumb, total field copies; unit-test by
  round-tripping populated structs.
- [ ] **Step 6:** Integration tests follow the repo's existing live-test idiom
  (verified: `tests/*_live_pg.rs` are `#[ignore]`d AND env-gated with a
  graceful eprintln-skip, e.g. `users_live_pg.rs:6-13`) — gate on an env var
  pointing at the dev coordinator's gRPC addr. Include the member-vs-other-
  user's-session `not_found` test here (not deferred to Task 14).
- [ ] **Step 7:** `just check` + `just e2e` green. Commit per PR boundary:

```bash
git add crates/engram-coordinator/
git commit -m "feat(coordinator): SessionService unary RPCs via transport-agnostic cores + owner-check core"
```

### Task 11: `StreamEvents` server-stream

**Goal:** Replay-then-tail with semantics identical to the SSE handler
(`api/events.rs:43`). **Correction to the ADR (§9.3 says events auto-resume
idle sessions — the code does not):** the events handler only does
`get_session`; `ensure_active` is called by prompt/shell/exec, never here.
Do NOT add auto-resume — merely viewing an idle session must not resurrect its
microVM.

**Outcome:** `StreamEvents(id, since unset)` on a live session yields the same
`idx` sequence as the SSE feed; reopen with `since=last_idx` → no gap, no dupe.

**Files:**
- Modify: `crates/engram-coordinator/src/api/events.rs` (extract core),
  `grpc_app/mod.rs`

**Steps:**

- [ ] **Step 1:** Extract the sequencing-sensitive setup into a shared core —
  owner check, **subscribe-before-query** (`state.events.subscribe(id)` at
  events.rs:67 *before* the log query; the rule at events.rs:15), replay query
  (`REPLAY_LIMIT` = 1000):

```rust
pub(crate) async fn events_core(
    state: &SharedState,
    principal: &Principal,
    id: &str,
    since: Option<i64>,
) -> Result<(Vec<PersistedEvent>, broadcast::Receiver<IndexedEvent>), ApiError>
```

- [ ] **Step 2:** tonic method maps both arms to `app::SessionEvent`, carrying
  three wire details the SSE path has (verified in events.rs):
  - **`with_rewind_meta` (events.rs:144) applies to BOTH arms** — replay
    events carry rewind/epoch from DB columns, live ones from the broadcast;
    payload_json is the folded object (plan amendment 4).
  - **Replay→live dedupe:** drop live events with `idx <= replay high water`
    (events.rs:98-101 — copy the exact rule).
  - **Broadcast `Lagged`** (events.rs:102-106) → `SessionEvent { kind: "lagged",
    idx: unset, payload_json: {"missed": n} }` — idx stays unset so proxies
    don't disturb reconnect cursors.
- [ ] **Step 3:** Confirm RAII teardown: client disconnect drops the stream
  future → the broadcast receiver drops with it. Add a `tracing::debug!` in a
  guard's `Drop` and watch it fire in the manual check.
- [ ] **Step 4:** Manual verification against `just dev` (env-gated live test
  per the Task 10 idiom; record the commands in its doc comment).
- [ ] **Step 5:** `just check` green. Commit:

```bash
git add crates/engram-coordinator/
git commit -m "feat(coordinator): StreamEvents sharing the SSE replay+tail core (no auto-resume — matches today)"
```

### Task 12: Remaining SessionService RPCs

**Goal:** `Exec` (streaming), `GetLog`, `Snapshot`, `Resume`, `EvictLocal`,
`GetCowState`, `ListCheckpoints`, `GetArtifact` (streaming), and
`CreateArtifactFromPath` — same pattern as Task 10.

**Sizing note:** two PRs — (a) `Exec` + `GetArtifact` (the streaming/tricky
two), (b) the mechanical rest.

**Outcome:** Every SessionService RPC returns real data over gRPC; `just e2e`
green.

**Files:**
- Modify: `api/exec.rs`, `api/sessions_inspect.rs`, `api/snapshot.rs`,
  `api/upload.rs`, `grpc_app/mod.rs`, `grpc_app/convert.rs`

**Steps:**

- [ ] **Step 1:** `Exec`: there are **two distinct handlers** — `exec`
  (exec.rs:147, unary + rusage) and `exec_stream` (exec.rs:260, streamed
  frames). Verify they're semantically unifiable behind one streaming core
  before merging them; if not, extract two cores and have the proto `Exec` use
  the streaming one (the unary axum route keeps its own core until Task 31
  deletes it).
- [ ] **Step 2:** `GetArtifact`: the source handler is `serve_artifact`
  (upload.rs:542) — it *streams* media up to `MAX_ARTIFACT_BYTES` (512 MiB,
  `engram-harness-proto/src/lib.rs:370`). The gRPC method emits
  `GetArtifactResponse{metadata}` first, then `chunk` frames (64 KiB is fine).
  All session-scoped cores start with `require_session_owner_core`.
- [ ] **Step 3:** The mechanical six, with converters + unit tests as before.
- [ ] **Step 4:** Extend the env-gated live test: snapshot → evict-local →
  resume on a demo session; cow-state and checkpoints non-error.
- [ ] **Step 5:** `just check` + `just e2e` green; commit per PR boundary.

### Task 13: `ShellRelayService.Relay` bidi

**Goal:** The orchestrator-facing bidi relay with the same lifecycle as the WS
handler. **Map of the real code (verified — the plan's reviewers corrected an
earlier sketch):** the WS handler already bridges
`engram_core::types::shell::ShellFrame` (`{Text, Binary, Ping, Pong,
Close(Option<ShellClose>)}`, shell.rs:29) over `HostClient::proxy_shell`'s
`ShellTunnel` mpsc channels — it never touches `ProxyShellMessage` (that
mapping lives below `HostClient` in the protocol layer). So the core is:
`ensure_active` (shells DO auto-resume, shell.rs:42 — unlike events) →
`registry.get` → `acquire_shell` → `proxy_shell` → pump `ShellFrame`s →
`release_shell`. The gRPC method just maps proto frames ⇄
`engram_core` `ShellFrame` (careful: the proto messages and the Rust type
share names — alias imports).

**Outcome:** A Rust test client opens `Relay`, sends `ShellOpen`, sends
`echo hi\n` as text, receives output frames; dropping the client releases the
lease (debug-log check).

**Files:**
- Modify: `crates/engram-coordinator/src/api/shell.rs`, `grpc_app/mod.rs`

**Steps:**

- [ ] **Step 1:** Extract the acquire→pump→release core from the WS handler,
  parameterized over a frame source/sink of `engram_core` `ShellFrame`s.
  Preserve three existing behaviors: auto-resume (`ensure_active`);
  no-sandbox-after-resume → `Conflict`/409 → `failed_precondition`
  (shell.rs:46-53); **`acquire_shell` failure is deliberately non-fatal**
  (warn-and-continue, shell.rs:62-68) — the relay must not fail the stream on
  it.
- [ ] **Step 2:** tonic bidi method: first inbound frame must be `open` (else
  `invalid_argument`); auth + `require_session_owner_core` on
  `ShellOpen.session_id`; then the core.
- [ ] **Step 3:** Teardown under tonic is **new code, not a copy**: the WS
  handler releases explicitly after its bridge future completes (shell.rs:94),
  which is safe because `on_upgrade` tasks aren't cancelled mid-bridge — but
  tonic *does* drop the handler future on client disconnect. Wrap the lease in
  a guard whose `Drop` spawns the async release (`Drop` can't `await`, ADR
  §9.4), with the explicit release on the normal exit path so the spawn is the
  exceptional case only.
- [ ] **Step 4:** Env-gated live test per the repo idiom. `just check` +
  `just e2e` green. Commit:

```bash
git add crates/engram-coordinator/
git commit -m "feat(coordinator): ShellRelayService bidi sharing the WS acquire/pump/release core"
```

### Task 14: Fleet/Image/User services + `GetMe` + `SaveClaudeToken`

**Goal:** The remaining services, extract-and-delegate. **Gating is per-RPC,
not per-service** (verified against api/mod.rs:88-92): `ListEnabledImages`,
`ListEnableJobs`, `GetEnableJob` are **member-level** (the create-session form
depends on the first); everything else in Fleet/Image is admin;
`ListUsers`/`PatchUser` admin; `GetMe`/`SaveClaudeToken` self-scoped.

**Sizing note:** one PR per service.

**Outcome:** Every RPC returns real data; a member JWT gets `permission_denied`
on `ListUsers` but succeeds on `ListEnabledImages` and `GetMe`.

**Files:**
- Modify: `api/hosts.rs`, `api/admin.rs`, `api/storage.rs`,
  `api/enabled_images.rs`, `api/registries.rs`,
  **`api/principal.rs` (the user surface lives here: `list_users:501`,
  `patch_user:521`, `save_claude_token:296`, and the `/me` handler `GetMe`
  derives from)**, `grpc_app/mod.rs`, `grpc_app/auth.rs`, `grpc_app/convert.rs`

**Steps:**

- [ ] **Step 1:** Add `fn require_admin_grpc(p: &Principal) -> Result<(), Status>`
  to `grpc_app/auth.rs` (mirrors `require_admin`, principal.rs:160). Apply it
  per the gating map above — enumerated, not blanket.
- [ ] **Step 2:** FleetService: cores + converters. `DrainHost` = the soft
  member-facing `hosts::drain`; `AdminDrainHost` = `admin::drain_host`
  (cordon+evacuate) — two RPCs, two handlers, don't conflate.
- [ ] **Step 3:** ImageService: cores + converters; the `AddRegistryAuth` serde
  enum ⇄ proto `oneof` mapping is explicit in `convert.rs`.
- [ ] **Step 4:** UserService: `ListUsers`/`PatchUser`; `GetMe` builds the
  response from the already-resolved `Principal` (+ the `has_claude_token`
  lookup the `/me` handler does); `SaveClaudeToken` delegates to the existing
  sealing+storage path — the token is never logged (check the handler's
  redaction discipline and keep it).
- [ ] **Step 5:** Tests: converter round-trips; member-vs-admin matrix
  (`permission_denied` on `ListUsers`, success on `ListEnabledImages`/`GetMe`).
- [ ] **Step 6:** `just check` green; commit per service.

---

# Phase 3 — The orchestrator service

> `orchestrator/` is a **standalone pnpm package** (verified: the repo has no
> root package.json / pnpm-workspace.yaml; `web/` is standalone too). Commit
> `orchestrator/pnpm-lock.yaml`. CI: add a job running
> `pnpm -C orchestrator install --frozen-lockfile && pnpm -C orchestrator test && pnpm -C orchestrator typecheck`
> in Task 15 — don't leave orchestrator tests CI-less until the final gate.

### Task 15: Scaffold — Hono on Node, config, health, SIGTERM, Tilt wiring

**Goal:** One Node HTTP server: `/rpc/*` → Connect adapter (empty routes for
now), everything else → Hono; `/healthz`; basic graceful shutdown; Tilt runs it
under `just dev`.

**Outcome:** `curl http://127.0.0.1:8787/healthz` → `200 {"ok":true}` under
`just dev`; SIGTERM closes the listener cleanly.

**Files:**
- Create: `orchestrator/package.json`, `tsconfig.json`, `vitest.config.ts`,
  `src/index.ts`, `src/server.ts`, `src/config.ts`, `src/routes/health.ts`
- Modify: `Tiltfile`

**Steps:**

- [ ] **Step 1:** `mkdir orchestrator && cd orchestrator && pnpm init`, then:

```bash
pnpm add hono @hono/node-server @hono/node-ws \
  @connectrpc/connect @connectrpc/connect-node @bufbuild/protobuf
pnpm add -D typescript tsx vitest @types/node
```

  Scripts: `"dev": "tsx watch src/index.ts"`, `"test": "vitest run"`,
  `"typecheck": "tsc --noEmit"`. Pin `@hono/node-server` ≥ 1.13 (the version
  that wires client-disconnect → `c.req.raw.signal` abort, which Task 20
  depends on — verified the wiring exists in current versions).
- [ ] **Step 2:** `tsconfig.json` — load-bearing because the committed
  `src/gen` uses `.js`-suffixed relative imports and
  `@bufbuild/protobuf/codegenv1` subpath imports (classic `node` resolution
  fails):

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

- [ ] **Step 3:** `src/config.ts` — one validated singleton (every later task
  imports `config`; don't mix factory and singleton styles):

```ts
export interface Config {
  port: number;                 // ORCHESTRATOR_PORT, default 8787
  databaseUrl: string;          // ORCHESTRATOR_DATABASE_URL (required from Task 16)
  controlPlaneGrpcUrl: string;  // CONTROL_PLANE_GRPC_URL, default http://127.0.0.1:50061
  authIssuer: string;           // APP_AUTH_ISSUER, default http://127.0.0.1:8787
  authAudience: string;         // APP_AUTH_AUDIENCE, default engram-control-plane
}

export function loadConfig(env: NodeJS.ProcessEnv = process.env): Config {
  /* read + validate; throw naming the missing var */
}

export const config: Config = loadConfig();
```

- [ ] **Step 4:** `src/server.ts`:

```ts
import { createServer } from 'node:http';
import { getRequestListener } from '@hono/node-server';
import { connectNodeAdapter } from '@connectrpc/connect-node';
import type { ConnectRouter } from '@connectrpc/connect';
import type { Hono } from 'hono';

// One process, one port: Connect RPCs under /rpc, everything else
// (better-auth, SSE, WS upgrade, health) is Hono. ADR 0039 §7.
// connectNodeAdapter's `requestPathPrefix` and `contextValues` options
// verified against @connectrpc/connect-node v2 types.
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

- [ ] **Step 5:** `src/routes/health.ts` + `src/index.ts` (build, `listen(config.port)`,
  and `process.on('SIGTERM', () => server.close(() => process.exit(0)))` —
  cheap shutdown; full stream-draining is explicitly deferred, see Out of
  scope).
- [ ] **Step 6:** Vitest: build with a stub Hono app on an ephemeral port;
  `/healthz` → 200; `/rpc/anything` → handled by the adapter (404/unimplemented,
  not the Hono app). → PASS.
- [ ] **Step 7:** Tilt: mirror the web resource's pattern (Tiltfile:489 —
  including `pnpm install --silent &&` in `serve_cmd`, or a fresh checkout
  breaks):
  `local_resource('orchestrator', serve_cmd='cd orchestrator && pnpm install --silent && pnpm dev', resource_deps=['postgres'], ...)`.
  `just dev` → healthz 200.
- [ ] **Step 8:** Add the orchestrator CI job (see phase preamble). Commit:

```bash
git add orchestrator/ Tiltfile .github/workflows/
git commit -m "feat(orchestrator): scaffold — hono on node, /rpc connect seam, tilt + ci (ADR 0039)"
```

### Task 16: Orchestrator database + drizzle

**Goal:** Separate `engram_orchestrator` DB on the existing dev Postgres
(ADR §10), drizzle-owned, env-configured; migrations wired into dev.

**Outcome:** `pnpm drizzle-kit migrate` applies; `/healthz` includes a DB ping;
a fresh `just dev` brings the schema up without manual steps.

**Files:**
- Create: `orchestrator/drizzle.config.ts`, `src/db/schema.ts`, `src/db/client.ts`
- Modify: `deploy/docker-compose.dev.yml` (initdb script — none exists today,
  verified), `Tiltfile`, `orchestrator/src/config.ts`

**Steps:**

- [ ] **Step 1:** `pnpm add drizzle-orm pg && pnpm add -D drizzle-kit @types/pg`.
- [ ] **Step 2:** `drizzle.config.ts` (dialect postgresql, schema
  `src/db/schema.ts`, out `drizzle/`, url from `ORCHESTRATOR_DATABASE_URL`);
  empty `schema.ts`; `client.ts` exports `pg.Pool` + drizzle instance.
- [ ] **Step 3:** Database creation, two paths (the compose volume persists, so
  initdb alone is NOT enough — verified `just db-down` keeps the volume):
  - *Fresh machines:* mount an init script into `docker-entrypoint-initdb.d`
    with `CREATE DATABASE engram_orchestrator;`.
  - *Existing dev machines (primary path):*
    `docker compose -f deploy/docker-compose.dev.yml exec postgres createdb -U engram engram_orchestrator`
    (idempotent-ish: ignore "already exists"). Put this in a `just` recipe or
    the Tilt migrate resource's command. (`just db-reset` also works but is
    destructive — say so wherever it's suggested.)
- [ ] **Step 4:** Wire migrations into dev: Tilt one-shot
  `local_resource('orchestrator-migrate', cmd='cd orchestrator && pnpm drizzle-kit migrate', resource_deps=['postgres'])`,
  and make the `orchestrator` resource depend on it.
- [ ] **Step 5:** DB ping in `/healthz` (`select 1` → 500 on failure); vitest
  against the local DB with the env-gated graceful-skip idiom. Commit:

```bash
git add orchestrator/ deploy/ Tiltfile
git commit -m "feat(orchestrator): separate engram_orchestrator db via drizzle (ADR 0039 §10)"
```

### Task 17: better-auth — login, session, JWT plugin, JWKS

**Goal:** better-auth with the drizzle adapter and JWT plugin: email+password
(dev/self-hosted; IAP bridge is Task 29), JWKS served for the control plane.

**Outcome:** Sign-up → session cookie works; `GET /api/auth/jwks` returns keys;
the minted JWT verifies against it **with RS256** (see Step 2).

**Files:**
- Create: `orchestrator/src/auth/better-auth.ts`
- Modify: `src/db/schema.ts` (generated tables), `src/index.ts` (mount),
  `src/config.ts` (`BETTER_AUTH_SECRET`, `TRUSTED_ORIGINS`)

**Steps:**

- [ ] **Step 1:** `pnpm add better-auth`.
- [ ] **Step 2:** `src/auth/better-auth.ts`. Two non-obvious, verified settings:
  better-auth's JWKS defaults to **EdDSA/Ed25519**, but the Rust verifier
  resolves keys via `jsonwebtoken`'s `from_jwk` whose Ed25519 support is
  unproven here — **pin RS256**, the alg Task 9's fixtures use, so the test
  suites exercise the real pairing. And the browser reaches this through the
  vite proxy with `Origin: http://localhost:5173` — without `trustedOrigins`,
  better-auth 403s every non-GET auth route (CSRF protection):

```ts
import { betterAuth } from 'better-auth';
import { drizzleAdapter } from 'better-auth/adapters/drizzle';
import { jwt } from 'better-auth/plugins';
import { db } from '../db/client';
import { config } from '../config';

export const auth = betterAuth({
  baseURL: config.authIssuer,
  trustedOrigins: config.trustedOrigins,        // dev: ['http://localhost:5173']
  database: drizzleAdapter(db, { provider: 'pg' }),
  // Dev/self-hosted door. NOTE: public sign-up + the control plane's
  // JIT-upsert = open registration. Acceptable in dev only; production
  // posture is decided in Task 22 (disable sign-up / allowlist) — do not
  // ship this default past Phase 4 without that decision.
  emailAndPassword: { enabled: true },
  plugins: [
    jwt({
      jwks: { keyPairConfig: { alg: 'RS256' } },  // match the Rust verifier
      jwt: {
        issuer: config.authIssuer,
        audience: config.authAudience,
        expirationTime: '60s',                    // ADR §5: short-lived assertion
        definePayload: ({ user }) => ({ email: user.email }),
      },
    }),
  ],
});
```

- [ ] **Step 3:** Generate schema: `pnpm dlx @better-auth/cli@latest generate`
  (needs `ORCHESTRATOR_DATABASE_URL` + `BETTER_AUTH_SECRET` in env — the
  config import is eager); merge into `src/db/schema.ts`;
  `pnpm drizzle-kit generate && pnpm drizzle-kit migrate`.
- [ ] **Step 4:** Mount: `app.on(['GET','POST'], '/api/auth/*', (c) => auth.handler(c.req.raw))`.
- [ ] **Step 5:** Verify: curl sign-up → cookie; `curl :8787/api/auth/jwks` →
  RS256 key present. Vitest: sign-up → sign-in → `auth.api.getSession` round
  trip (env-gated on the local DB). Commit:

```bash
git add orchestrator/
git commit -m "feat(orchestrator): better-auth with drizzle adapter + RS256 jwt/jwks (ADR 0039 §5)"
```

### Task 18: Per-request token supplier + control-plane transport

**Goal:** Every upstream call carries a fresh ~60s JWT for the request's user
via a `kAuthToken` context value + transport interceptor. Fail **closed and
correctly typed**: no session → `Code.Unauthenticated` (Connect maps it to
HTTP 401; a bare `throw` would surface as `internal`/500 — verified).

**Outcome:** Vitest: bearer attached from context; no-session →
`ConnectError` code `unauthenticated`; missing supplier → code `internal`.

**Files:**
- Create: `orchestrator/src/auth/token.ts`, `src/control-plane/transport.ts`,
  `src/control-plane/client.ts`

**Steps:**

- [ ] **Step 1:** `src/auth/token.ts`:

```ts
import { createContextKey, ConnectError, Code } from '@connectrpc/connect';
import { auth } from './better-auth';

// A supplier, not a token: minted lazily per upstream call so the 60s
// exp is always fresh, closed over the inbound request's headers.
export type TokenSupplier = () => Promise<string>;
export const kAuthToken = createContextKey<TokenSupplier | undefined>(undefined);

export function tokenSupplierFromRequest(headers: Headers): TokenSupplier {
  return async () => {
    try {
      const { token } = await auth.api.getToken({ headers });
      return token;
    } catch {
      // Maps to HTTP 401 on the Connect protocol — the contract the UI
      // relies on. A bare rethrow would surface as internal/500.
      throw new ConnectError('unauthenticated', Code.Unauthenticated);
    }
  };
}
```

- [ ] **Step 2:** `src/control-plane/transport.ts` — bearer interceptor + the
  ADR §9.4 HTTP/2 keepalives (options verified in connect-node v2:
  `pingIntervalMs`, `pingTimeoutMs`, `pingIdleConnection`):

```ts
import { createGrpcTransport } from '@connectrpc/connect-node';
import { ConnectError, Code, type Interceptor } from '@connectrpc/connect';
import { kAuthToken } from '../auth/token';
import { config } from '../config';

const bearer: Interceptor = (next) => async (req) => {
  const supply = req.contextValues.get(kAuthToken);
  if (!supply) {
    // Programming error (route forgot contextValues), not a user error.
    throw new ConnectError('no auth token supplier on call context', Code.Internal);
  }
  req.header.set('authorization', `Bearer ${await supply()}`);
  return next(req);
};

export const controlPlaneTransport = createGrpcTransport({
  baseUrl: config.controlPlaneGrpcUrl,   // HTTP/2; also carries the Relay bidi
  interceptors: [bearer],
  // ADR §9.4: detect half-open connections (slept laptops, dead peers)
  // so leases/subscriptions don't ghost.
  pingIntervalMs: 20_000,
  pingTimeoutMs: 10_000,
  pingIdleConnection: true,
});
```

- [ ] **Step 3:** `src/control-plane/client.ts`:

```ts
import { createClient } from '@connectrpc/connect';
import { SessionService, ShellRelayService } from '../gen/engram/app/v1/session_pb';
import { controlPlaneTransport } from './transport';

export const sessions = createClient(SessionService, controlPlaneTransport);
export const shellRelay = createClient(ShellRelayService, controlPlaneTransport);
```

- [ ] **Step 4:** Tests (in-process Connect server capturing headers): bearer
  lands; the two failure codes as in the Outcome. Commit:

```bash
git add orchestrator/
git commit -m "feat(orchestrator): per-request jwt supplier + bearer interceptor + h2 keepalives"
```

### Task 19: The generic passthrough + the surface allowlist

**Goal:** One forwarder for every passthrough RPC (plan amendment 2),
preserving response headers/trailers.

**Outcome:** Fake-upstream vitest green (no Phase 2 needed). Live (needs
Task 10): `curl -X POST :8787/rpc/engram.app.v1.SessionService/ListSessions -H 'content-type: application/json' -b <cookie> -d '{}'`
returns sessions; without a cookie → HTTP 401 (via Task 18's typed error).

**Files:**
- Create: `orchestrator/src/rpc/passthrough.ts`, `src/rpc/surface.ts`
- Modify: `src/index.ts`

**Steps:**

- [ ] **Step 1:** Failing test: in-process Connect server (fake control plane)
  implements `ListSessions` returning a sentinel + a response header; build the
  orchestrator router with `registerPassthrough` → client call → sentinel comes
  back, fake saw the bearer, **response header survived the hop**.
- [ ] **Step 2:** Implement (Transport call shapes verified against
  @connectrpc/connect v2; `methodKind` is `'server_streaming'`, snake_case):

```ts
import type { ConnectRouter, Transport, HandlerContext } from '@connectrpc/connect';
import type { DescService } from '@bufbuild/protobuf';

export interface PassthroughSpec {
  service: DescService;
  methods?: string[];   // subset by method name; default: all
}

// One forwarder for every passthrough RPC (plan amendment 2). AuthZ is
// NOT here — the control plane re-checks owner/admin on every call
// (ADR §6). This only relabels identity: cookie in, bearer JWT out (the
// interceptor on `upstream` reads kAuthToken from ctx.values).
export function registerPassthrough(
  router: ConnectRouter,
  specs: PassthroughSpec[],
  upstream: Transport,
) {
  for (const { service, methods } of specs) {
    const impl: Record<string, unknown> = {};
    for (const m of service.methods) {
      if (methods && !methods.includes(m.name)) continue;
      if (m.methodKind === 'unary') {
        impl[m.localName] = async (req: unknown, ctx: HandlerContext) => {
          const res = await upstream.unary(m, ctx.signal, undefined, undefined, req, ctx.values);
          copyHeaders(res.header, ctx.responseHeader);
          copyHeaders(res.trailer, ctx.responseTrailer);
          return res.message;
        };
      } else if (m.methodKind === 'server_streaming') {
        impl[m.localName] = async function* (req: unknown, ctx: HandlerContext) {
          const res = await upstream.stream(
            m, ctx.signal, undefined, undefined,
            (async function* () { yield req; })(), ctx.values,
          );
          copyHeaders(res.header, ctx.responseHeader);
          yield* res.message;
        };
      }
      // client/bidi streaming: never passthrough — the shell has its own
      // WS route, and browsers can't send these over Connect anyway.
    }
    router.service(service, impl as never);
  }
}

function copyHeaders(from: Headers, to: Headers) {
  from.forEach((v, k) => to.set(k, v));
}
```

- [ ] **Step 3:** `src/rpc/surface.ts` — clean now that the shell relay is its
  own service:

```ts
import { SessionService } from '../gen/engram/app/v1/session_pb';
import { FleetService } from '../gen/engram/app/v1/fleet_pb';
import { ImageService } from '../gen/engram/app/v1/image_pb';
import { UserService } from '../gen/engram/app/v1/user_pb';
import type { PassthroughSpec } from './passthrough';

// The entire "what does the app API expose" decision, in one file.
// ShellRelayService is deliberately absent (WS route owns that leg).
export const SURFACE: PassthroughSpec[] = [
  { service: SessionService },
  { service: FleetService },
  { service: ImageService },
  { service: UserService },
];
```

- [ ] **Step 4:** Wire in `index.ts`:
  `buildServer(app, (r) => registerPassthrough(r, SURFACE, controlPlaneTransport), (req) => createContextValues().set(kAuthToken, tokenSupplierFromRequest(headersFromNodeReq(req))))`
  — write the small `headersFromNodeReq(req: IncomingMessage): Headers` helper.
- [ ] **Step 5:** Tests green; live Outcome when Task 10 is in. Commit:

```bash
git add orchestrator/
git commit -m "feat(orchestrator): generic connect passthrough + surface allowlist (plan amendment 2)"
```

### Task 20: SSE events route + artifact byte route

**Goal:** The two browser-native HTTP legs: `GET /api/v1/sessions/:id/events`
(SSE over upstream `StreamEvents`) and
`GET /api/v1/sessions/:id/artifacts/:artifact_id` (bytes over upstream
`GetArtifact` — `<img src>`/`<a href>` can't speak Connect; without this route
every artifact 404s at the Task 27 proxy flip).

**Outcome:** The SSE curl streams the same events as today's coordinator SSE
(new envelope frame format — see Step 2); reconnect with `Last-Event-ID`
replays from the cursor; the artifact route streams an image with the right
`content-type`. Live Outcome needs Tasks 11/12.

**Files:**
- Create: `orchestrator/src/routes/events.ts`, `src/routes/artifacts.ts`
- Modify: `src/index.ts`

**Steps:**

- [ ] **Step 1:** Auth guard first, both routes: `await auth.api.getSession({
  headers: c.req.raw.headers })` → 401 before any upstream work.
- [ ] **Step 2:** `routes/events.ts`. The envelope on the wire is
  **hand-built JSON**, not `toJsonString` — protobuf-JSON would emit
  lowerCamelCase (`payloadJson`) and int64-as-string, which the Task 24 parser
  (and human curls) shouldn't have to know about:

```ts
import { Hono } from 'hono';
import { streamSSE } from 'hono/streaming';
import { createContextValues } from '@connectrpc/connect';
import { sessions } from '../control-plane/client';
import { kAuthToken, tokenSupplierFromRequest } from '../auth/token';

export const events = new Hono();

events.get('/api/v1/sessions/:id/events', (c) =>
  streamSSE(c, async (stream) => {
    // Cursor: max(?since, Last-Event-ID), NaN-guarded — matches the
    // coordinator's "never goes backward across a reconnect" rule
    // (web/src/sse.ts:5-9). -1 / absent → unset (replay from start).
    const nums = [c.req.query('since'), c.req.header('last-event-id')]
      .map((v) => Number(v))
      .filter((n) => Number.isFinite(n) && n >= 0);
    const since = nums.length ? BigInt(Math.max(...nums)) : undefined;

    const ctx = createContextValues().set(kAuthToken, tokenSupplierFromRequest(c.req.raw.headers));
    const upstream = sessions.streamEvents(
      { sessionId: c.req.param('id'), since },
      { signal: c.req.raw.signal, contextValues: ctx },  // browser close → RST upstream
    );
    // Keepalive: the coordinator emits SSE comments every 15s (axum
    // KeepAlive, api/events.rs:78-81). hono's writeSSE can't emit
    // comments; an empty `event: ping` frame is functionally equivalent
    // (sse.ts listens per-kind and ignores unknown events).
    const ping = setInterval(() => void stream.writeSSE({ data: '', event: 'ping' }), 15_000);
    try {
      for await (const ev of upstream) {
        await stream.writeSSE({
          // lagged frames have no idx — omit id: so reconnect cursors
          // are never disturbed (mirrors api/events.rs:103-105).
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
  }),
);
```

- [ ] **Step 3:** `routes/artifacts.ts`: open upstream `getArtifact`, read the
  first message (`metadata`) → set `content-type`/`content-length`, then pipe
  `chunk` frames into the response stream. Abort upstream on client disconnect
  (same `signal` pattern).
- [ ] **Step 4:** Tests with a fake upstream: 3 events → 3 `id:` lines in
  order; a `lagged` event → frame with no `id:`; client abort cancels the fake
  (onAbort flag); artifact route: metadata-then-chunks → correct headers+body.
- [ ] **Step 5:** Live checks (Outcome). Commit:

```bash
git add orchestrator/
git commit -m "feat(orchestrator): SSE event leg + artifact byte route (plan amendments 2/3)"
```

### Task 21: Shell WS route

**Goal:** `GET /api/v1/sessions/:id/shell` ⇄ upstream `ShellRelayService.Relay`.
Teardown bidirectional; **no unhandled rejections** (the pump must catch — an
uncaught abort error in a void'd async IIFE kills the Node process on every
normal browser disconnect); ping/pong relayed; WS keepalive per ADR §9.4.

**Outcome:** Terminal works through the relay (`websocat` now, UI in Task 25);
killing the client releases the coordinator lease (Task 13 debug log) **and
the orchestrator process survives** (test asserts it).

**Files:**
- Create: `orchestrator/src/routes/shell.ts`
- Modify: `src/index.ts`, `src/server.ts` (inject `@hono/node-ws` upgrade)

**Steps:**

- [ ] **Step 1:** Wire `createNodeWebSocket({ app })` →
  `{ upgradeWebSocket, injectWebSocket }`; call `injectWebSocket(server)` in
  `index.ts` (have `buildServer` return the raw server pre-`listen`).
- [ ] **Step 2:** The bridge. Key corrections baked in: auth gate **before**
  upgrade; `catch` around the pump (swallow `Code.Canceled`/`Code.Aborted`,
  log others, `ws.close(1011)`); answer upstream `ping` frames with `pong`
  (browser JS cannot send WS pongs — the orchestrator answers on its behalf;
  the host side uses these for liveness, shell.rs:119/142); handle Node
  `Buffer` message data (not browser `ArrayBuffer`); periodic WS ping to the
  browser with a pong-deadline close:

```ts
export const shell = (upgradeWebSocket: UpgradeWebSocket) => {
  const app = new Hono();
  app.get('/api/v1/sessions/:id/shell', async (c, next) => {
    const session = await auth.api.getSession({ headers: c.req.raw.headers });
    if (!session) return c.text('unauthenticated', 401);   // gate BEFORE upgrade
    return next();
  });
  app.get('/api/v1/sessions/:id/shell', upgradeWebSocket((c) => {
    const abort = new AbortController();
    const inbound = pushableQueue<RelayShellRequest>();    // bounded asyncIterable queue, this file
    return {
      onOpen: (_e, ws) => {
        inbound.push(openFrame(c.req.param('id')));
        void (async () => {
          try {
            const ctx = createContextValues().set(kAuthToken, tokenSupplierFromRequest(c.req.raw.headers));
            for await (const f of shellRelay.relay(inbound, { signal: abort.signal, contextValues: ctx })) {
              switch (f.frame.case) {
                case 'text':   ws.send(f.frame.value); break;
                case 'binary': ws.send(f.frame.value); break;
                case 'ping':   inbound.push(pongFrame(f.frame.value)); break;  // answer for the browser
                case 'close':  ws.close(f.frame.value.code, f.frame.value.reason); break;
              }
            }
          } catch (e) {
            if (!isAbortLike(e)) {                          // Canceled/Aborted = normal teardown
              log.warn({ err: e }, 'shell relay error');
              ws.close(1011, 'upstream error');
            }
          } finally {
            ws.close();                                     // upstream end → browser close
          }
        })();
      },
      onMessage: (e) => inbound.push(frameFromWsEvent(e)),  // string | Buffer | ArrayBuffer
      onClose: () => { abort.abort(); inbound.end(); },     // browser close → upstream abort
    };
  }));
  return app;
};
```

- [ ] **Step 3:** Helpers + tests: `pushableQueue` (give it a cap — input is
  typing-rate but don't rely on it) with push/iterate/end ordering tests
  including a binary `Buffer` round-trip; `frameFromWsEvent`. Backpressure on
  the output side: check `ws.raw.bufferedAmount` against a high-water mark and
  pause the pump (a `cat bigfile` must not balloon orchestrator memory).
- [ ] **Step 4:** WS keepalive: `ws.raw.ping()` every 20s, close on missed pong
  (§9.4 half-open). Subprotocol: the web client opens
  `new WebSocket(url, 'tty')` (`TerminalPane.tsx:170`) — verify the upgrade
  response echoes `Sec-WebSocket-Protocol: tty` (browsers hard-fail otherwise).
- [ ] **Step 5:** The process-survives test: open against a fake upstream,
  close the client, assert no unhandledRejection (vitest
  `process.on('unhandledRejection')` trap). Live check vs Task 13 when
  available. Commit:

```bash
git add orchestrator/
git commit -m "feat(orchestrator): browser WS shell leg over ShellRelayService bidi"
```

---

# Phase 4 — Web migration

> **Posture (read first):** every task in this phase is runnable in dev only
> because the coordinator stays `AuthMode::None` + SyntheticAdmin until
> Task 30. Mixed states (RPC data as the better-auth user, events/shell as
> SyntheticAdmin) are expected mid-phase — see "Sequencing". **Deviation note:**
> per plan amendments 3/4, events keep a hand-typed payload union
> (`web/src/events.ts`) — the one surviving hand-mirror, pinned by Task 24's
> fixture contract test rather than codegen.

### Task 22: better-auth client, login page, `/me` replacement, e2e entry

**Goal:** Auth front door moves to the orchestrator. `AuthProvider` keeps its
**external interface** (`principal` with `role` / `is_admin` /
`has_claude_token` / `display_name`, `refresh()`, the admin gate) but is
internally recomposed: better-auth session = authentication; the new
`UserService.GetMe` (via `/rpc` passthrough) = the role/token data `/me`
provided. (`GET /me` itself dies in Task 31; a better-auth session alone
carries no role — roles are authoritative in the control plane, ADR §6.)

**Sizing note:** two PRs — (a) login + proxy + e2e entry, (b) the
AuthProvider/GetMe recomposition.

**Outcome:** `just e2e` passes, entering through better-auth (the one
sanctioned entry-step change from the ADR Preparation section); admin nav still
gates correctly; sign-out works.

**Files:**
- Modify: `web/vite.config.ts` — add `/api/auth` and `/rpc` →
  `http://127.0.0.1:8787` **before** the existing `/api` rule (vite matches in
  insertion order — verified); `/api/v1` keeps targeting the coordinator until
  Task 27.
- Create: `web/src/pages/Login.tsx`, `web/src/lib/auth-client.ts`
- Modify: `web/src/auth/AuthProvider.tsx` (recompose; also: `logout()`
  currently POSTs the coordinator's `/auth/logout` and the 401 path
  hard-navigates to `/api/v1/auth/login` — swap to `authClient.signOut()` and
  an SPA `/login` redirect), `web/src/App.tsx` (route `/login`)
- Modify: `web/e2e/global-setup.ts`, `web/playwright.config.ts`
- Modify: `Tiltfile` — add the e2e/dev user's email to the coordinator's
  **bootstrap-admins env** (config.rs:156-160): JIT-upserted users default to
  `member`, and nothing else ever makes the dev user an admin → Task 26's
  admin pages and today's admin-visible e2e flows would silently break.

**Steps:**

- [ ] **Step 1:** `pnpm -C web add better-auth`. `web/src/lib/auth-client.ts`:

```ts
import { createAuthClient } from 'better-auth/react';
// Same-origin: the vite proxy (dev) / fronting LB (prod) routes /api/auth
// to the orchestrator. Don't set a cross-origin baseURL — cookie scoping.
export const authClient = createAuthClient();
```

- [ ] **Step 2:** `Login.tsx` (email+password via `authClient.signIn.email`,
  plus dev sign-up); unauthenticated users route here.
- [ ] **Step 3:** Recompose `AuthProvider`: better-auth `useSession` for
  authn; `GetMe` (generated connect-query hook through `/rpc`) for
  role/token data; keep the exported `AuthState` shape so `UserChip`,
  `ProfilePanel`, `TokensPanel` (`refresh()` after token save),
  `NewSessionForm`, `Members`, `Sessions`, `RequireAdmin` don't churn.
  **Decision to record in code comment:** public sign-up + JIT-upsert = open
  registration; fine for dev, production needs sign-up disabled or an
  allowlist before this deploys anywhere reachable (the old `NotMemberError`
  screen's job).
- [ ] **Step 4:** e2e entry via Playwright's request context (don't hand-roll
  set-cookie parsing):

```ts
import { request } from '@playwright/test';
// in globalSetup, after the precondition gate:
const ctx = await request.newContext({ baseURL: 'http://localhost:5173' });
await ctx.post('/api/auth/sign-up/email', { data: { email: E2E_EMAIL, password: E2E_PW, name: 'e2e' } })
  .catch(() => {});                                  // idempotent: exists already
const res = await ctx.post('/api/auth/sign-in/email', { data: { email: E2E_EMAIL, password: E2E_PW } });
if (!res.ok()) throw new Error('better-auth sign-in failed — is the orchestrator up on :8787?');
await ctx.storageState({ path: 'e2e/.auth-state.json' });
```

  Reference it via `use.storageState` in `playwright.config.ts`; the journey
  spec body is untouched.
- [ ] **Step 5:** `just e2e` → PASS. Commit per PR boundary:

```bash
git add web/ Tiltfile
git commit -m "feat(web): better-auth front door + GetMe-backed AuthProvider (ADR 0039 §5)"
```

### Task 23: Sessions list/detail → connect-query

**Goal:** Session hooks call the orchestrator's Connect surface. **The
migration is NOT a mechanical one-liner per hook** — three things must be
carried deliberately:
1. **Query options:** today's hooks poll (`useSessions` 1s, detail 2s,
   checkpoints 5s, cow-state 2s) with `placeholderData: (prev) => prev` —
   dropping them freezes the status chip and fails the Phase 0 net. Pass each
   hook's existing TanStack options through as connect-query's options arg,
   verbatim.
2. **Response unwrapping:** current hooks return `r.sessions` etc.; either
   keep consumers on `data.sessions` or use `select` — pick one and apply
   consistently.
3. **Cache keys:** manual string keys die. The three cross-hook invalidation
   edges (verified): `useDrainHost` → optimistic ops on `['hosts']`;
   `useEnableJobs` ↔ `['enabled-images']`; `useEnableImage` →
   `['enable-jobs']`. Rebuild them with `createConnectQueryKey` from the
   method descriptors (Task 26 inherits this note).

**Outcome:** Sessions + SessionDetail render identically; network tab shows
`/rpc/engram.app.v1.SessionService/*`; `pnpm -C web test` + `just e2e` green.

**Files:**
- Modify: `web/src/App.tsx` — `TransportProvider` wraps alongside the existing
  `QueryClientProvider` (it lives here, not `main.tsx` — verified; connect-query
  rides the same QueryClient)
- Modify: `web/src/hooks/useSessions.ts`, `useCheckpoints.ts`, `useCowState.ts`,
  `web/src/hooks/useSessionEvents.ts` consumers as needed
- Modify: `web/src/components/NewSessionForm.tsx` (`createSession` call site),
  `web/src/components/PromptComposer.tsx` (`sendPrompt`), the interrupt call
  site in `SessionDetail.tsx` (there is **no** delete-session call in the web
  today — don't invent one)
- Modify: `web/src/test-utils.tsx` (add a `TransportProvider` backed by
  `createRouterTransport` fakes), `web/src/components/NewSessionForm.test.tsx`
  (it mocks `globalThis.fetch` on REST URLs today — replace fetch-spying with
  `createRouterTransport` service fakes)

**Steps:**

- [ ] **Step 1:** `pnpm -C web add @connectrpc/connect-web` (connect +
  connect-query landed in Task 7). Wire `TransportProvider` with
  `createConnectTransport({ baseUrl: '/rpc' })` in `App.tsx`.
- [ ] **Step 2:** Migrate `useSessions` as the template (generated method
  export from the `query-es` plugin):

```ts
import { useQuery } from '@connectrpc/connect-query';
import { listSessions } from '../gen/engram/app/v1/session-SessionService_connectquery';

export function useSessions(scope: 'mine' | 'all' = 'mine') {
  return useQuery(listSessions, { scope }, {
    refetchInterval: 1_000,                  // carried over from the old hook
    placeholderData: (prev) => prev,
  });
}
```

  Generated TS is camelCase (`imageUri` not `image_uri`) — fix call sites as
  the compiler surfaces them; that's the hand-mirror debt being paid, not
  avoidable churn.
- [ ] **Step 3:** Repeat for detail/checkpoints/cow-state; mutations via
  `useMutation(createSession)` etc. in the component files listed above,
  preserving each one's invalidation edges via `createConnectQueryKey`.
- [ ] **Step 4:** Fix the test harness (`test-utils.tsx`,
  `NewSessionForm.test.tsx`) with `createRouterTransport`. `pnpm -C web test`
  green.
- [ ] **Step 5:** `just e2e` green. Commit:

```bash
git add web/
git commit -m "feat(web): sessions list/detail on connect-query via the orchestrator"
```

### Task 24: Events → orchestrator SSE envelope

**Goal:** `useSessionEvents` consumes the orchestrator's SSE leg: still
`EventSource` (native reconnect — verified `sse.ts` has no custom retry loop),
new envelope parse; payload union isolated in `web/src/events.ts` and pinned by
a fixture contract test.

**Outcome:** Event count/status/RAW behave identically; reconnect resumes from
`Last-Event-ID` without gap. (Caveat for the manual check: while the
orchestrator is *down*, the vite proxy answers 502 — EventSource treats HTTP
errors as fatal and stops retrying; native retry covers network blips and
stream drops, which is what production sees. The ADR's reconnect-with-cursor
wrapper remains the recorded fallback if this bites.)

**Files:**
- Create: `web/src/events.ts` (move the `SessionEvent` union from
  `web/src/types.ts:250` and `IndexedEvent` from `:384`, unchanged)
- Modify: `web/src/sse.ts` (`subscribeSession`, `:21-98`) — same EventSource +
  cursor logic, new frame parse
- Modify: `web/src/hooks/useSessionEvents.ts` + consumers importing the union
- Create: `web/src/events.contract.test.ts`

**Steps:**

- [ ] **Step 1:** Move the union; mechanical import updates.
- [ ] **Step 2:** Rework the frame parse in `subscribeSession`, keeping its
  signature. The wire is Task 20's hand-built envelope —
  `{ idx: number|null, kind: string, payload_json: string }` (snake_case,
  numeric idx; deliberately NOT protobuf-JSON):
  - parse the envelope, then `JSON.parse(payload_json)` typed as the
    `events.ts` union;
  - **preserve the `_rewound`/`_recovery_epoch` lifting** the current code
    does (`sse.ts:36-44`) — the payload object carries them (plan
    amendment 4) and transcript greying depends on it;
  - `lagged` frames arrive as `kind: "lagged"` with `idx: null` and no SSE
    `id:` — keep the existing `onLagged` hook behavior;
  - cursor continues to come from `Last-Event-ID` semantics exactly as today.
- [ ] **Step 3:** The contract test (`events.contract.test.ts`): a small set of
  checked-in fixture lines captured from the live wire (one per major event
  kind + one rewound event + one lagged frame), parsed through
  `subscribeSession`'s parser and type-asserted against the `events.ts` union.
  This is the pin replacing codegen for the surviving hand-mirror (Phase 4
  deviation note).
- [ ] **Step 4:** `pnpm -C web test` (Transcript fixtures updated to the
  envelope where they faked the wire) + live reconnect check + `just e2e`.
  Commit:

```bash
git add web/
git commit -m "feat(web): event feed via orchestrator SSE envelope; union pinned by contract test"
```

### Task 25: Shell → orchestrator WS

**Goal:** `TerminalPane`'s WebSocket rides the orchestrator.

**Outcome:** SHELL tab works end-to-end; e2e shell step green; the `tty`
subprotocol is echoed (browser hard-fails otherwise — Task 21 Step 4 owns the
server side; this task verifies it from the client).

**Steps:**

- [ ] **Step 1:** Point the `/api/v1/sessions/:id/shell` proxy entry (`ws: true`)
  at the orchestrator in `web/vite.config.ts`; `TerminalPane` itself shouldn't
  change (same path).
- [ ] **Step 2:** Manual: open shell, `echo hi`, check the response headers for
  `Sec-WebSocket-Protocol: tty`, kill the tab, confirm lease release in
  coordinator logs.
- [ ] **Step 3:** `just e2e` green. Commit:

```bash
git add web/
git commit -m "feat(web): shell websocket via the orchestrator relay"
```

### Task 26: Admin pages → connect-query

**Goal:** Fleet/Storage/Settings/Members on generated clients through the
passthrough, observing the Task 23 rules (options/unwrap/keys).

**Outcome:** All four pages render with live data **as the bootstrap-admin dev
user** (Task 22's Tilt env made the e2e user admin — without it this Outcome is
untestable); a non-admin user sees member-scoped results (control plane
re-checks, ADR §6).

**Files:**
- Modify: `web/src/hooks/useHosts.ts`, `useDrainHost.ts`, `useStorageSummary.ts`,
  `useEnabledImages.ts`, `useEnableJobs.ts`, `useRegistries.ts`
- Modify: `web/src/pages/Fleet.tsx`, `Storage.tsx`, `Settings.tsx`, `Members.tsx`

**Steps:**

- [ ] **Step 1:** Migrate hooks per the Task 23 template, carrying each one's
  polling/optimistic-update behavior (the `useDrainHost` optimistic ops and
  the two enable-images/jobs invalidation edges use `createConnectQueryKey`).
  GC buttons pass `dryRun` on the single RPC. Fleet drain uses `DrainHost`
  (the soft one — that's what the page calls today).
- [ ] **Step 2:** Members: `ListUsers`/`PatchUser`. Settings token form:
  `SaveClaudeToken` — verify orchestrator logging never dumps RPC payloads
  (the token transits opaquely).
- [ ] **Step 3:** `pnpm -C web test` + page-by-page manual check (admin and
  non-admin) + `just e2e`. Commit:

```bash
git add web/
git commit -m "feat(web): admin pages on generated clients via passthrough"
```

### Task 27: Delete the hand-mirrored layer + flip the proxy

**Goal:** Remove `web/src/api.ts` and `web/src/types.ts`; flip `/api`
wholesale to the orchestrator; fix the e2e precondition probe (it queries
`/api/v1/enabled-images`, which the orchestrator doesn't serve — left as-is,
the gate dies before any test runs).

**Outcome:** No imports of `./api` or `./types` remain (`git grep -nE "from '\.\.?/(api|types)'" web/src` → empty); `pnpm -C web build` clean;
`just e2e` green; the coordinator receives no browser traffic.

**Steps:**

- [ ] **Step 1:** Flip the vite proxy: `/api` + `/rpc` → `:8787` only.
- [ ] **Step 2:** Sweep the long tail (verified list of `api.ts` dependents
  beyond the hooks): `API_BASE` is imported by `sse.ts`, `TerminalPane.tsx`,
  and `ArtifactCard.tsx` (artifact URLs now resolve against the orchestrator's
  Task 20 byte route — same path, no logic change; give `API_BASE` a new home,
  e.g. `web/src/lib/base.ts`); `logout()`/`redirectToLogin` died in Task 22;
  pure-UI helper types from `types.ts` move next to their single consumer or
  into `events.ts`.
- [ ] **Step 3:** Re-point the e2e precondition: sign in first (Task 22's
  request-context), then probe
  `POST /rpc/engram.app.v1.ImageService/ListEnabledImages` with the captured
  cookie; keep the no-harness-image assertion.
- [ ] **Step 4:** `pnpm -C web build && pnpm -C web test && just e2e` — all
  green. Commit:

```bash
git add -A web/
git commit -m "feat(web)!: retire hand-mirrored api.ts/types.ts — generated contract only (ADR 0039 §7)"
```

---

# Phase 5 — Cutover and shedding the coordinator's web identity

> Order matters: scripts/CLI first (28), then the IAP door (29), **then** auth
> collapse (30) and route removal (31) — never remove a door before its
> replacement exists.

### Task 28: Port scripts, CLI, and CI off the cookie/synthetic auth

**Goal:** Everything non-browser that consumes the web API today keeps working
when SyntheticAdmin and the web routes go. **Inventory (verified):**
`engram-cli` is a full REST client of the web surface (sessions, hosts, drain,
registries, admin flush — `crates/engram-cli/src/main.rs:514-1236`);
`deploy/dev/tilt-up-ci.sh:60-65` polls admin-gated `GET /api/v1/hosts`; the CI
e2e lane seeds GHCR creds via `engram-cli registry add`;
`integration-session.sh`/`integration-test.sh`/`integration-bake-demo.sh` curl
the API unauthenticated.

**Outcome:** `just integration-session`, `integration-test.sh`, and the CI lane
all pass with the coordinator's gRPC surface (or an explicit bearer), no
synthetic admin involved.

**Steps:**

- [ ] **Step 1:** Decide the mechanism per consumer and write it down in the
  task PR: the clean target is the app-gRPC surface with a long-lived
  **service-bearer** (`ServiceBearer` already exists for host-agents); a
  retained, bearer-authed REST sliver is the fallback for `curl`-heavy
  scripts.
- [ ] **Step 2:** Port `engram-cli`'s commands to tonic clients
  (`engram-protocol` is already a dependency of the workspace) or to bearer
  REST — whichever Step 1 chose. Port the scripts.
- [ ] **Step 3:** **The bearer blast radius (do atomically):** today
  `ENGRAM_AUTH_TOKENS` is empty in dev/CI and `require_bearer` passes
  everything (`api/auth.rs:37-39,77` `accepts_anything`). The moment tokens
  become non-empty, the *internal host-ingest router* starts enforcing — so
  setting a token means simultaneously: coordinator env, **both** Tilt
  host-agent resources' env, `ENGRAM_TOKEN` in all four `deploy/dev` scripts,
  the CI lane, and prod deploy values. Checklist them in the PR; partial
  rollout bricks host registration.
- [ ] **Step 4:** Run all three integration scripts + the CI lane. Commit:

```bash
git add crates/engram-cli deploy/ .github/
git commit -m "feat(cli,ci): port scripts and engram-cli off synthetic/cookie auth (ADR 0039 prep for cutover)"
```

### Task 29: IAP bridge middleware (before the doors close)

**Goal:** Behind GCP IAP, a verified `X-Goog-IAP-JWT-Assertion` creates a
better-auth session (ADR §5's second path). Sequenced **before** Task 30 so an
IAP production deploy always has a working door.

**Outcome:** Unit tests: valid ES256 IAP JWT (test-key-signed; the real JWKS is
ES256 at `https://www.gstatic.com/iap/verify/public_key-jwk`,
`iss=https://cloud.google.com/iap` — matches the preset in
`engram-auth/src/config.rs:63-68`) → better-auth session for that email;
invalid → 401; middleware inert when `IAP_AUDIENCE` unset. **Plus a local
smoke:** sign with a test ES256 key, point `IAP_JWKS_URL` at a local fixture
server, drive a request end-to-end.

**Files:**
- Create: `orchestrator/src/auth/iap.ts`
- Modify: `src/config.ts` (`IAP_AUDIENCE`, `IAP_JWKS_URL`), `src/index.ts`,
  `src/server.ts`

**Steps:**

- [ ] **Step 1 (placement — the subtle part):** the bridge must cover **every
  entry path**, and `server.ts` routes `/rpc/*` around Hono — Hono-only
  middleware never runs for RPCs, so an IAP browser whose only traffic is
  Connect calls would 401 forever. Hoist the bridge to the raw-server level
  (a wrapper around both handlers in `buildServer`) or make
  `tokenSupplierFromRequest` IAP-aware. Pick the wrapper: one place, both
  stacks.
- [ ] **Step 2 (spike, timeboxed):** better-auth has no public "create a
  session for an arbitrary verified user" one-liner — pin the mechanism
  (server-side `auth.api` calls / internal adapter session-create) against the
  installed version before writing the middleware, and record it in the file's
  doc comment. Semantics: create the better-auth session once on first
  IAP-verified request (JIT user create), set the cookie on the response;
  subsequent requests ride the cookie and skip verification.
- [ ] **Step 3:** Implement with `jose` (`createRemoteJWKSet` + `jwtVerify`,
  check `iss` + audience), mirroring `forward.rs`'s claim handling. Tests +
  the local ES256 smoke. Commit:

```bash
git add orchestrator/
git commit -m "feat(orchestrator): IAP trusted-SSO bridge into better-auth sessions (ADR 0039 §5)"
```

### Task 30: Coordinator auth collapse — forward-auth + service-bearer only

**Goal:** Remove the human cookie/OIDC/synthetic machinery (ADR §5: three modes
collapse to one). **File map (verified — an earlier draft of this task named
the wrong files):** what goes is `CookieSession`
(`crates/engram-auth/src/cookie.rs`), `SyntheticAdmin` (`synthetic.rs`), the
OIDC endpoints `principal::login/callback` (api/mod.rs:209-210) +
`principal::logout` (`:96`), the `WebSessionStore` plumbing, and the
`AuthMode::Oidc`/`None` arms in `build_chain` (config.rs:122-158). **Do NOT
touch** `api/auth.rs` (that's `require_bearer` — the deployment-bearer
middleware for the internal host-ingest router) or `api/session_auth.rs`
(the ADR 0023/0026 per-session broker-token check for in-guest forge/upload) —
both are load-bearing for non-web traffic.

**Outcome:** The coordinator's human-auth surface is exactly: forward-auth
(orchestrator JWKS) on gRPC + remaining web-era routes; `ServiceBearer` for
hosts; broker tokens for in-guest seams. `just dev` + `just e2e` +
`just integration-session` green with the orchestrator as the only human door.

**Steps:**

- [ ] **Step 1:** Inventory: `git grep -n "SyntheticAdmin\|CookieSession\|AuthMode::" crates/ deploy/`
  — every dependent must be already ported (Task 28) or part of this change.
  (Note: host ingest never rode `AuthMode` — it's `require_bearer`, a separate
  mechanism; `AuthMode::None` existed solely to inject SyntheticAdmin into the
  human chain.)
- [ ] **Step 2:** Remove the modes + files; fix compilation; update tests.
- [ ] **Step 3:** Tilt env flips to forward-auth (the JWKS env vars landed in
  Task 9). Fresh `just dev`; full gate sweep (Outcome). Commit:

```bash
git add crates/ Tiltfile deploy/
git commit -m "feat(coordinator)!: collapse human auth to forward-auth — cookies/OIDC/synthetic-admin leave (ADR 0039 §5)"
```

### Task 31: Remove the coordinator's legacy web routes

**Goal:** Delete the web-facing `/api/v1` axum routes. **Retain (verified
inventory — the ADR's list of six ingest routes is incomplete):** all
**eight** internal ingest routes incl. `/hosts/forge` and `/hosts/upload`
(api/mod.rs:183-184); the broker-token forge seam
`/sessions/:id/git-credential` + `/sessions/:id/pull-request` (api/mod.rs:216-221
— in-guest, NOT web routes despite the path shape); `/healthz` + `/readyz`;
anything Task 28 chose to keep as a bearer-authed REST sliver. Decide
explicitly for the operator-only endpoints with no RPC
(`/admin/chunk-gc/candidates`, `/admin/reap-materialize-dir`): keep behind
bearer or delete with a note.

**Outcome:** The route table is host-ingest + broker-seam + health (+ the
documented sliver); the handler *bodies* survive as `*_core` functions serving
gRPC; `just check` + `just e2e` + `just integration-session` + the CI lane
green.

**Steps:**

- [ ] **Step 1:** Route-by-route deletion in `api/mod.rs:33-241` per the retain
  list; delete now-unreferenced axum shims (cores stay).
- [ ] **Step 2:** Clippy's dead-code flags: remove, don't `allow`.
- [ ] **Step 3:** Full gates (Outcome). Commit:

```bash
git add crates/
git commit -m "feat(coordinator)!: shed the web-server identity — app surface is gRPC-only (ADR 0039 §1)"
```

---

## Final validation gate (after Task 31)

- [ ] `just check` — Rust gates green.
- [ ] `pnpm -C web build && pnpm -C web test`; `pnpm -C orchestrator test && pnpm -C orchestrator typecheck`.
- [ ] `just e2e` — **the Phase 0 characterization net, with its two sanctioned
  setup edits (better-auth entry, Task 22; precondition probe via `/rpc`,
  Task 27) and otherwise byte-for-byte the same assertions, green across three
  tiers.** This is the ADR's definition of done.
- [ ] `just integration-session` + `deploy/dev/integration-test.sh` + the CI
  e2e lane — green post-Task-28 auth.
- [ ] Negative checks: unauthenticated `POST /rpc/...ListSessions` → 401; a
  deleted web route on the coordinator → 404; a member JWT vs an admin JWT
  through the passthrough behave per the Task 14 gating map.
- [ ] IAP: the Task 29 local ES256 fixture smoke passes (staging verification
  is a deploy-checklist item, not a repo gate).
- [ ] `buf breaking --against '.git#branch=main'` clean locally;
  `buf generate && git diff --exit-code -- web/src/gen orchestrator/src/gen`
  clean.
- [ ] Walk ADR 0039 §2.3's tables + this plan's amendments: every RPC exists
  and is reachable through the passthrough (15 min, by hand, dev stack).
