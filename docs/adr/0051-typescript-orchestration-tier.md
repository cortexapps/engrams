# ADR 0051: A TypeScript orchestration tier — splitting the application layer from the control plane

Status: 2026-06-16 — **Accepted**, implemented + prod-deployed. The stacked PR
chain landed on `main`: the app-contract protos + buf pipeline (#288); the
coordinator's additive app-gRPC surface + the `harness_env` create-time secret
channel (#295); the orchestrator tier image + Helm (#297); the web cutover to
the orchestrator (#296); gRPC reflection + end-to-end error surfacing (#311);
and the destructive flip (#298) — the coordinator drops its web-facing REST,
human-auth, and `users` identity model to become a gRPC-only control plane
(migration `0067`). Prod runs the three-tier stack behind GCP IAP (web →
orchestrator → coordinator app-gRPC); human authentication and authorization
live entirely in the orchestrator. (Original status: 2026-06-05 — Proposed.)
Renumbered: 2026-06-14 — was ADR 0039 in the original proposal (PR #122), which
collided with the existing `0039-retire-rolling-memfile-all-sparse-checkpoints`.
Renumbered to **0051** (next free after 0050) for the implementation stack; all
in-code `ADR 0039` references move with it.
Revised: 2026-06-10 — major revision after design review. Changes from the
original draft: (1) the runtime is **Hono on Node**, not Bun/Elysia; (2) the
orchestrator forwards the control-plane contract through a **generic Connect
passthrough** rather than hand-written endpoints; (3) the browser event leg
**stays SSE**; (4) **authorization moves up into the orchestrator** — the
control plane becomes an AWS-style raw resource API with machine-to-machine
auth only, and sheds its `users` table entirely (this reverses the original
§5/§6 and supersedes ADR 0031's in-coordinator ownership model); (5) a **task
model** becomes the application-layer aggregate root — sessions are resources
tasks consume. The implementation lands as a stacked PR chain: the app-contract
protos + buf pipeline (this PR), the orchestrator tier, the coordinator's
gRPC-only control plane, the schema migrations, the web cutover, and the deploy
wiring. Builds on the existing gRPC fabric (ADR 0013) and the `ProxyShell` bidi
shell tunnel (ADR 0014).

## Context

Today the system is two tiers: a Rust **coordinator** (`engram-coordinator`,
axum) and a React/Vite **web** app. The coordinator is doing two unrelated
jobs in one binary:

1. **A control plane for a Firecracker microVM fleet** — sandbox lifecycle,
   chunked manifests / COW (ADR 0007/0016), snapshot blobs, the gRPC host
   fabric (ADR 0013), the `ProxyShell` shell tunnel (ADR 0014), KEK/KMS
   envelope crypto, the OCI client, the harness hub, the scheduler, idle
   eviction (ADR 0034), and the per-session event log. This is systems
   programming and Rust is the right language for it.
2. **A web application backend** — the `/api/v1` REST surface the web consumes
   (~30 of the coordinator's 69+ routes), SSE for the session event stream,
   the shell WebSocket, OIDC/cookie session auth (ADR 0031), and the
   hand-written `web/src/types.ts` that mirrors the Rust wire shapes by hand.

The forcing function is twofold. **Near-term**, the web-facing concerns above —
the app API, session auth, the drift-prone hand-mirrored types (measured: half
of all web commits touch `types.ts`) — want to evolve faster than a Rust
systems binary comfortably allows. **Forward-looking**, a new class of work is
coming that does not belong in Rust at all: an **orchestration layer** for
tasks and workflows. The motivating examples — *a Linear ticket spins up a
session, opens a PR, moves the ticket through stages and posts to Slack*; *a
Slack mention creates a session and comments results back into the thread* —
are stateful, event-driven, integration-heavy glue whose business rules change
weekly. That is TypeScript's home turf and Rust's worst ergonomics. We
introduce the tier **now** for the application-layer split (API + auth +
transport + the task model), and shape it deliberately so those **future**
workflows land in TS where they belong.

## Decision

### 1. Three tiers, with a clean cut

```
   Linear / GitHub / Slack webhooks ──┐ (future)
                                       ▼
   React UI  ◄──────────────►  TS Orchestration Tier  ◄──────────►  Rust Control Plane
                               (Hono / Node)                        (engram-coordinator)
                               • the public app API                 • microVMs / snapshots
                               • TASKS (the app aggregate root)     • the SESSION EVENT LOG
                               • human authn (better-auth)          • gRPC host fabric / ttyd
                               • authZ (CASL + roles)               • KEK/KMS, COW, scheduler
                               • users + roles (better-auth admin)  • per-session sealed secrets
                               • user secrets (user_session_secrets)• (no per-user secret store)
                               • durable state (its own Postgres)   • NO user identity at all
                               • inbound webhooks        (future)   • auth: service bearer only
                               • durable workflows — DBOS (future)  • exposes: commands + streams
```

The **control plane** becomes a pure internal resource API — the AWS-service
shape: it provides raw primitives (sessions, snapshots, images, hosts) to a
single trusted caller and knows nothing about end users, roles, tickets, or
ownership. The **orchestration tier** owns every human- and integration-facing
concern, including *who may do what*. The **UI** becomes one of several clients
of the orchestration tier (webhooks and scheduled workflows are the future
others).

The governing principle for the cut: *the orchestrator both commands the
control plane (create session, send prompt) and reacts to it (consume session
events → external side effects), and it alone knows what any of it means to a
human.* The control plane knows nothing about Linear, Slack, cookies, OIDC —
or users.

### 2. Ownership boundary and the control-plane contract

There are **two contracts**, and only one is new:

- the **orchestrator-facing app contract** — a new tonic gRPC surface the
  orchestrator consumes, defined in §2.3 (derived from today's `/api/v1`
  routes, minus everything identity-shaped);
- the **host-facing internal contract** — the *existing* `host_service.proto`
  plus the host→coordinator ingest routes. This stays internal, unchanged, and
  the orchestrator never touches it.

#### 2.1 What the control plane owns (the substrate)

- **Session lifecycle & state machine** — `pending → created → active → idle →
  evicting/…`; create / get / list / delete. Sessions carry **no owner**: the
  `sessions.user_id` column and all owner-scoping leave the control plane
  (dropped at cutover). Attribution lives in the orchestrator's task model
  (§3).
- **Harness management** — *which* agent runs in a session is selected from the
  session's image; prompt routing, interrupt, the in-process harness hub.
  Harness-token injection stays here mechanically, but is driven by the
  **`harness_env` map passed at `CreateSession`** (already-resolved env values
  such as `CLAUDE_CODE_OAUTH_TOKEN`) — the control plane no longer resolves
  "the calling user's token" because it has no notion of a calling user. It
  folds those values into the session's KEK-sealed `session_secrets` and
  replays them on resume.
- **Sandbox / microVM orchestration** — scheduling, the gRPC host fabric
  (ADR 0013), sandbox create/destroy, exec, the `ProxyShell` tunnel.
- **Durability** — snapshot / resume / idle eviction (ADR 0034), chunked
  manifests + COW, blob storage, checkpoints.
- **The session event log** — the authoritative append-only log (monotonic
  `idx`, recovery epochs) and the PG LISTEN/NOTIFY fan-out. The orchestrator
  *consumes* it; it does not own it.
- **Image enablement** — OCI pulls, enable jobs, manifest caching (ADR 0036).
- **Registries & per-session secrets** — KEK-sealed registry creds, and the
  session's own **`session_secrets`**: the env values handed in via
  `harness_env` at create time, KEK-sealed per session and replayed on resume.
  KEK/KMS never leaves Rust. (The original draft put a *user-keyed sealed-secret
  vault* here. In the implementation the durable per-user secret store — the
  Claude token and the like — moved entirely **up to the orchestrator**'s
  KEK-sealed `user_session_secrets` table, which resolves them into `harness_env`
  at create time; the control plane keeps no per-user secret state, only the
  per-session sealed copy it needs to boot and resume.)
- **Fleet & storage** — hosts, capacity, drain/cordon, storage rollups, GC.
- **Artifacts** — session file artifacts (ADR 0026).

Gone from this list relative to the original draft: ~~identity for authZ
(users + roles)~~, ~~owner-scoping + admin gating~~. The control plane
authenticates exactly one kind of caller — a trusted service — and authorizes
nothing per-user.

#### 2.2 What the orchestrator owns

- **The public app API** — the Connect surface the UI consumes. Two kinds of
  endpoint: a **generic passthrough** over the control-plane contract (the
  default — same generated service definitions on both hops, one forwarding
  handler, an allowlist; no per-endpoint code), and **native services** for
  orchestrator-owned data (tasks, §3) or aggregation.
- **Tasks** — the application-layer aggregate root (§3): what work is being
  done, of what type, for whom, using which sessions.
- **Human authentication** — better-auth (login, session, cookie) and the IAP
  bridge (§5).
- **Users and roles** — better-auth's user store *is* the user store; its
  admin plugin owns the `admin`/`member` role, ban/role-set admin APIs, and
  the Members UI. No mirror anywhere else.
- **Authorization** — all of it (§6): a CASL ability built from the
  better-auth role plus task ownership, enforced in the passthrough gate and
  the native services, and shared with the React UI for affordance gating.
- **Its own durable state** — better-auth tables + `task`/`task_session` now;
  the `connections` store + DBOS workflow state when workflows land.
- **(future, §4/§5)** webhook ingestion, outbound integrations, workflows.

#### 2.3 The contract: control-plane gRPC exposed to the orchestrator

Proposed service groupings; each maps to a current `/api/v1` route. Names are
indicative, not final. Note what is *absent*: any user, role, or `/me` concept.

**`SessionService`** (the core resource surface):

| RPC | Replaces (today) | Notes |
|-----|------------------|-------|
| `ListSessions()` | `GET /sessions` | returns **all** sessions — the caller is trusted; the orchestrator filters by task ownership |
| `CreateSession(image_uri, mode, overrides?, prompt?, harness_secret_id?)` | `POST /sessions` | harness selected via the image; token injection driven by the explicit sealed-secret ref, not a principal |
| `GetSession(id)` / `DeleteSession(id)` | `GET`/`DELETE /sessions/:id` | |
| `SendPrompt(id, text)` | `POST /sessions/:id/prompt` | routes to the harness; auto-resumes idle |
| `Interrupt(id)` | `POST /sessions/:id/interrupt` | stops the in-flight run (ADR 0030) |
| `Exec(id, req) → stream` | `POST /sessions/:id/exec[/stream]` | one streaming RPC; unary is the degenerate collected case |
| `StreamEvents(id, since) → stream SessionEvent` | SSE `GET /sessions/:id/events` | server-streaming, typed envelope; retires `sse.ts`'s hand-parse |
| `GetLog(id, kind, limit)` | `GET /sessions/:id/log` | conversation transcript |
| `Snapshot(id)` / `Resume(id)` / `EvictLocal(id)` | `POST snapshot`/`resume`, `DELETE local` | lifecycle |
| `GetCowState(id)` / `ListCheckpoints(id)` | `GET cow-state`/`checkpoints` | durability views |
| `GetArtifact(id, artifact_id) → stream` | artifacts (ADR 0026) | streaming — artifacts run to 512 MiB |
| `CreateArtifactFromPath(id, path)` | | |

**`ShellRelayService`** (its own service — bidi is not browser-callable and
the name must not collide with the host-facing `ProxyShell`):

| RPC | Replaces | Notes |
|-----|----------|-------|
| `Relay(stream) → stream` | WS `GET /sessions/:id/shell` | gRPC bidi; frame kinds mirror `ProxyShellMessage` (§9) |

**`FleetService`** (operator surface — admin-gated *in the orchestrator*):

| RPC | Replaces |
|-----|----------|
| `ListHosts` / `GetHost` / `GetHostCowState` | `GET /hosts*` |
| `DrainHost` (soft) / `AdminDrainHost` (cordon+evacuate) | `POST /hosts/:id/drain`, `/admin/hosts/:id/drain` |
| `CordonHost` / `UncordonHost` | `/admin/hosts/:id/*` |
| `GetStorageSummary` | `GET /storage/summary` |
| `FlushSession` / `EvacuateSession` | `/admin/sessions/:id/*` |
| `ChunkGc` / `BundleGc` / `SnapshotBlobGc` (dry_run flag) | `/admin/*-gc/*` |

**`ImageService`**:

| RPC | Replaces |
|-----|----------|
| `ListEnabledImages` / `EnableImage` / `DisableImage` / `RefreshImage` | `/enabled-images*` |
| `ListEnableJobs` / `GetEnableJob` / `RetryEnableJob` | `/enable-jobs*` |
| `ListRegistries` / `AddRegistry` / `DeleteRegistry` | `/registries*` |

**Secrets — no control-plane secret service.** The original draft put a sealed
`SecretService` (`PutSecret`/`HasSecret`/`DeleteSecret`, opaque keys) on the
control plane and made the orchestrator an opaque relay. The implementation
moved the **durable per-user store up to the orchestrator**: its own KEK-sealed
`user_session_secrets` table (keyed by `(user_id, env_var_name)`), which the
Settings UX reads/writes directly (`has_claude_token` is now an orchestrator
query). The control plane gains no secret RPC; instead `CreateSession` carries
a **`harness_env` map** of already-resolved env values, which the coordinator
seals per session into `session_secrets` and replays on resume. The Claude
token never round-trips through a coordinator secret store — the orchestrator
resolves it at create time and the control plane only ever holds the
per-session sealed copy it needs to boot and resume.

What is **not** in this contract: users, roles, `/me`, `/auth/*` — all fully
orchestrator-owned (§5/§6). There is no `UserService`.
`/healthz`/`/readyz` stay plain HTTP on the control plane for liveness probes.

### 3. Tasks: the application-layer aggregate root

The unit of work users and automations care about is not a session — it is a
**task**: a human chat, a Linear ticket being fixed, a dependabot event, an
incident investigation. Sessions are infrastructure resources a task
*consumes*; a task references its sessions, never the other way around. This
keeps each layer honest: the control plane knows sessions exist; the task
knows why.

- **`task`** (orchestrator Postgres): `id`, `type` (`chat` | `linear_issue` |
  `dependabot` | `incident` | …), `title` (human-readable — session naming
  falls out for free), `status` (app-level: open / working / awaiting_review /
  done / failed), `created_by_user_id` (nullable for pure automation),
  `source` (jsonb, type-specific trigger ref: issue id, Slack thread, alert),
  `workflow_run_id` (DBOS run — **null for chat**, §4), timestamps.
- **`task_session`**: `(task_id, session_id, role?)` — the pointer table.
  1 task : N sessions falls out naturally (an incident fans out parallel
  investigation sessions; a ticket task spins a second session after review
  comments). `role` is the only concession to that future and it is nullable.
- **The human chat path is a degenerate task, not a degenerate workflow:**
  `type='chat'`, one session, no `workflow_run_id`, both rows written
  synchronously in the create handler. The §4 boundary (no durable execution
  for interactive chat) survives intact.
- **Each task type maps 1:1 onto a workflow** when workflows land (§4):
  workflow function per type; `workflow_run_id` is the foreign key into
  DBOS's tables — which live in the *same* Postgres, so the task insert and
  the workflow start can be transactional/exactly-once.
- **TaskService is orchestrator-native** — pure orchestrator data, defined in
  the same proto package for uniform generated clients, implemented directly
  on the Connect router, never touching the control plane.
  `CreateTask(type: 'chat', image_uri, prompt)` is what the UI's "new
  session" becomes; its handler does the upstream `CreateSession` plus the
  two inserts.
- **The UI's primary surface becomes the task list** (the sessions list was
  already a workspace switcher in spirit): rows discriminated by `type`,
  detail views sharing common chrome with a type-specific panel — `chat` is
  exactly today's session detail; `linear_issue` adds ticket/PR/status;
  `incident` shows parallel sessions and a findings rollup.
- **Sessions with no `task_session` row** (created out-of-band, e.g. an
  operator using `engram-cli` against the control plane directly) are
  *unattributed*: visible to admins only.

Deliberately **not** modeled yet: per-type status machines, a task activity
table, task-level permissions beyond owner/admin, type-specific UI beyond
chat. The jsonb `source` column is schemaless so new types land without
migrations.

### 4. Workflows are future work; DBOS is the proposed engine

We are **not** building a workflow engine now. This section records the
*intended* home for future workflows — Linear-/webhook-triggered and
cron-triggered automations — so the tier is shaped to host them.

When workflows land, the **proposal** is **DBOS** (durable execution as a
library, backed by the orchestrator's Postgres) over a standalone engine
(Temporal) or a bare queue (pg-boss). DBOS requires Node — one of the reasons
the tier runs on Node (§8). Two properties will be **non-negotiable**, because
session events can redeliver (reconnect, replay, the `lagged` event):

- **Idempotency** on every outbound effect, keyed on `(session_id, event_idx)`.
- **Durable consumption cursor.** The control plane's per-session event log
  already carries a monotonic `idx` with replay, so a consumer resumes from
  "processed session X through idx N" with at-least-once delivery and **no
  message broker**.

The task model (§3) is the workflows' data home: a workflow run is a column on
the task, not a parallel bookkeeping system.

**Not for the human chat path.** The interactive chat experience needs none of
this. There the orchestrator writes two rows and relays; the **control plane
is already the durability boundary** — the session, its state machine, and the
authoritative event log survive any orchestrator restart, and the UI
reconnects with a `since` cursor. DBOS earns its place only for
**orchestrator-initiated** flows that fan out to external systems and must
survive crashes with idempotent retries (Appendix B/C).

### 5. Authentication: the orchestrator is the only authority; the control plane speaks machine-to-machine only

Human identity lives **entirely** in the orchestrator:

- **better-auth** owns login, the session cookie, and the user store. Its
  **admin plugin** owns roles (`admin`/`member`), the role-set/ban admin APIs,
  and backs the Members UI. There is no second user table anywhere.
- *Self-hosted / no proxy:* better-auth runs OIDC / email+password and owns
  the session.
- *Behind GCP IAP:* a thin bridge verifies the `X-Goog-IAP-JWT-Assertion`
  against Google's JWKS and programmatically creates a better-auth session
  from the verified email. IAP is a trusted-SSO bridge that feeds a session,
  not a better-auth provider.

**Orchestrator → control plane is machine identity, not user identity.** The
original draft had the orchestrator mint per-user forward-auth JWTs that the
control plane verified against JWKS and resolved to roles. With authorization
moved up (§6) there is no per-user anything to assert, so all of that
machinery — the JWT plugin's downstream tokens, the JWKS fetch, the
`ForwardAuthVerifier` wiring, JIT user upsert — **is deleted from the
design**. In its place: the coordinator's existing **`ServiceBearer`**
mechanism (already how host-agents authenticate), i.e. a static bearer token
on the gRPC metadata checked by a trivial interceptor, rotated via env, over a
private network. One caller, one credential. If the control plane ever has
multiple callers with different privileges, that is the trigger to upgrade to
scoped tokens or mTLS/SPIFFE — IAM-style per-caller scoping earns its keep at
more than one principal, not before.

The coordinator's human auth machinery — `CookieSession`, the `/auth/*` OIDC
endpoints, `SyntheticAdmin`, the `users` table, and the forward-auth human
path — all leave the coordinator at cutover.

### 6. Authorization moves up: CASL in the orchestrator

This reverses the original draft (and the enforcement half of ADR 0031). The
control plane authorizes nothing per-user; `require_admin` /
`require_session_owner` are deleted with the routes that carried them. The
orchestrator is the **sole authorization boundary**:

- **Policy engine: CASL** (`@casl/ability`) — TS-first, in-process,
  isomorphic. The policy is one small file: roles from better-auth, ownership
  via the task join (§3):

  ```ts
  can('manage', 'Task',    { createdByUserId: user.id });
  can(['read','prompt','shell','delete'], 'Session',
      { task: { createdByUserId: user.id } });   // resolved via task_session
  can('read', 'EnabledImage');
  if (user.role === 'admin') can('manage', 'all');
  ```

  The same ability file runs in the React app for affordance gating (hide
  admin nav, disable buttons) — UI and server can never disagree about the
  rules, only the server enforces them.
- **Enforcement points:** the generic passthrough gains a per-method policy
  gate (a static `method → {action, subject, resource-extractor}` map checked
  before forwarding — session-scoped RPCs resolve `session_id → task` with
  one indexed join); native services (TaskService) check the same ability
  inline. Fleet/Image/admin RPCs require `manage all` except the three
  member-level image reads the create flow needs.
- **Scale trigger:** if sessions/tasks grow sharing or team semantics
  ("anyone on team X can attach"), that is the moment to revisit a
  relationship-based engine (OpenFGA/SpiceDB). The seam — one
  `ability.can(...)` call site in the gate — keeps that swap contained. Until
  then, two roles and one ownership relation do not justify a policy service.

**Honest caveat — this trades away defense-in-depth, knowingly.** The
original design's argument was that a buggy orchestrator could not widen
access because Rust re-checked ownership. That property is gone: an
orchestrator authz bug is now a full-access bug, and the service-bearer
credential is full-privilege at the control plane. Accepted because it is the
standard shape for an internal-platform control plane (AWS does not know your
app's users either), and mitigated by: the control plane being
network-private with a single rotating credential; the policy module being
small, single-file, and heavily tested; and the passthrough gate failing
closed (no policy entry → deny).

### 7. Transport: Buf + Connect/gRPC end-to-end (retires hand-mirrored types)

Protobuf is the single source of truth for both seams, replacing the
hand-written `types.ts`. `.proto` files in `engram-protocol`; `buf generate`
emits both sides of both hops; `buf` breaking-change detection runs in CI.

- **Control plane ↔ orchestrator:** the control plane exposes a **tonic** gRPC
  service generated from the protos — the *same* stack (tonic 0.12 /
  prost 0.13) the host fabric already runs. The orchestrator consumes it with
  **connect-es** (gRPC to tonic over HTTP/2).
- **UI ↔ orchestrator:** **connect-es + connect-query** gives typed clients
  with TanStack Query bindings. The passthrough services and the native
  TaskService share one proto package, so the UI sees one uniform generated
  API.
- **The passthrough is generic** (§2.2): because both hops share service
  descriptors, the forwarding handler is a loop over `service.methods` — a
  handler for a method *is* a client call of that method. Adding an RPC to
  the contract requires zero orchestrator code; the §6 policy map is the only
  per-method artifact.

What stays plain HTTP on the orchestrator (and why Hono remains the host
process): **better-auth login** (cookie/redirect flows), the **SSE event
leg** (§8), the **artifact byte route** (browsers consume artifacts as
`<img src>`, not RPCs), and **inbound webhooks** when workflows land.

### 8. Runtime and streaming shapes

**Runtime: Node ≥ 22, Hono.** (Supersedes the original Bun/Elysia choice.)
Three reasons: DBOS requires Node; Node's HTTP/2 client is the mature path
for the gRPC bidi shell leg; and with Eden rejected in favor of Connect,
Elysia had no remaining differentiator. Hono is fetch-native, runs the same
code on Node, and has first-class better-auth support.

Streaming shapes:

- **Session event feed:** orchestrator ↔ control plane is a **server-streaming
  RPC**. The browser leg **stays SSE** (revising the original draft's
  Connect-stream choice; SSE was its recorded alternative): `EventSource`
  gives reconnect + `Last-Event-ID` for free, and the `data:` payload is a
  typed envelope (`{idx, kind, payload_json}`) derived from the generated
  `SessionEvent` message. Full event-payload typing is deferred with the
  transcript-shaping decision; the payload union is the one hand-maintained
  type file that survives, pinned by a fixture contract test.
- **Shell (bidirectional):** the browser↔orchestrator leg **stays a
  WebSocket** — browsers cannot do bidi over fetch. The
  orchestrator↔control-plane leg is **gRPC bidi** (`ShellRelayService.Relay`),
  frame kinds mirroring `ProxyShellMessage`
  (`open/text/binary/ping/pong/close`), and the control plane relays to the
  existing host tunnel. Bidi requires an HTTP/2 client on the orchestrator —
  solid on Node.

#### Transport map

| Surface | Transport |
|---|---|
| UI ↔ orchestrator (app API: passthrough + TaskService) | Connect (unary + server-streaming) |
| UI ↔ orchestrator (event feed) | **SSE** (typed envelope) |
| UI ↔ orchestrator (shell) | **WebSocket** (browser bidi limit) |
| UI ↔ orchestrator (artifacts) | plain HTTP bytes |
| Orchestrator ↔ control plane (everything) | gRPC / tonic + **service bearer** |
| Orchestrator ↔ control plane (event feed) | gRPC server-streaming |
| Orchestrator ↔ control plane (shell) | gRPC **bidi** (`ShellRelayService`) |
| Control plane ↔ host-agent (shell) | gRPC bidi (existing, ADR 0014) |
| Inbound webhooks (Linear/Slack/GitHub) | plain HTTP + HMAC *(future, §4)* |
| Human login | HTTP/cookie (better-auth) + IAP-bridge middleware |

### 9. Streaming lifecycle: mapping, reconnect, teardown

Both streams traverse two hops. The model that keeps this correct is
**stateless passthrough**: the orchestrator holds *no* `session_id → stream`
registry. Each browser stream is a lifetime-bound pipe to exactly one upstream
stream, and the **cursor — not the orchestrator — carries resumability**.

#### 9.1 Mapping model

- One browser stream → one upstream stream, opened **on demand**, torn down on
  disconnect. The "mapping" is the request handler's async scope, not a global
  map — nothing to leak, GC, or reconcile.
- gRPC multiplexes many logical streams over one HTTP/2 connection — there is
  no connection-count reason to pool, and pooling would break the teardown
  guarantees below.
- **Lease/subscription lifetime == upstream stream lifetime.** A shell's
  `AcquireShell` lease and an event stream's PG-bus subscription live inside
  the proxied call's scope and die with it.
- Two tabs on one session are two independent pipes; the control plane's PG
  fan-out feeds both.

#### 9.2 Establishing a stream

- **Events:** browser opens the SSE route (better-auth cookie) → orchestrator
  resolves the session, **checks the CASL ability against the task join**
  (§6), opens the upstream `StreamEvents` with the service bearer, and pipes
  with `id:` = event idx. The control plane validates the bearer, subscribes
  to the PG bus, replays then tails — no per-user logic.
- **Shell:** browser WS(`"tty"`) → same ability check → upstream `Relay` bidi
  (first frame `ShellOpen{session_id}`) and frame pumping. The control plane's
  `AcquireShell` pins the sandbox against idle-eviction for the stream's
  lifetime. (Appendix A/B.)

#### 9.3 Reconnect (close tab, return, reopen)

- **Events are replayable.** `EventSource` auto-reconnects with
  `Last-Event-ID`; the orchestrator opens a fresh upstream at that cursor; the
  control plane replays from its log then tails. The orchestrator remembers
  nothing. An idle session does **not** auto-resume on the events path (the
  original draft claimed it did; the code never has — only prompt/shell/exec
  resume).
- **Shells are not replayable.** A terminal is a live PTY; reopening is a
  fresh `Relay` + `AcquireShell`. Scrollback survives only if persisted inside
  the guest (tmux/ttyd).

#### 9.4 Cancellation & teardown

Teardown is a cascade of one primitive per hop:

```
Browser ──[A]──▶ Orchestrator ──[B]──▶ Control plane ──[C]──▶ host-agent ──▶ ttyd
 HTTP/WS close    AbortSignal           tonic future drop      ProxyShell end    PTY dies
                  → gRPC RST_STREAM      → RAII release
```

- **[A]→[B] is all the orchestrator does.** Events: forward the handler's
  abort signal into the upstream call — a one-liner. Shell: an
  `AbortController` bridges `ws.onclose → abort()` while the upstream ending
  closes the browser WS — teardown is bidirectional.
- **[B]→[C] + cleanup is control-plane behavior.** The reset drops the tonic
  handler future: a RAII guard releases the lease / unsubscribes from the
  bus. *Rust caveat:* `Drop` cannot `await` — async release spawns a detached
  task or runs explicitly on the relay loop's exit.
- **Silent death (half-open).** Keepalives at each hop — WS ping/pong to the
  browser, HTTP/2 keepalive PINGs on the orchestrator↔control-plane channel —
  or a ghost shell holds its lease until a coarse timeout.
- **Orchestrator crash = mass cancellation.** A dead orchestrator drops every
  HTTP/2 connection, resetting all in-flight streams → the control plane
  releases every lease/subscription. This holds **only because** lease
  lifetime == stream lifetime == one browser connection — never pool.

#### 9.5 The one long-lived stream the orchestrator owns (future)

A workflow (§4) consuming a session opens its **own** `StreamEvents` with a
**durable** cursor, independent of any browser. A session may have `0..N`
ephemeral browser pipes plus `0..1` durable workflow consumer — independent
subscribers to the control plane's broadcast. Durability is isolated to the
consumer that needs it; the human path stays stateless.

### 10. Databases: a separate store per tier, env-configured

Each tier owns its **own** database; the tiers **never share tables**. The
only cross-tier data flow is the gRPC contract (§2.3).

- **Control plane — shrinks.** Keeps its `engram` Postgres (`DATABASE_URL`,
  sqlx migrations in `deploy/migrations`). At cutover it **drops the `users`
  table and `sessions.user_id`** — the last identity-shaped data leaves Rust.
- **Orchestrator — new, separate DB** (`ORCHESTRATOR_DATABASE_URL`): the
  better-auth tables (users, sessions, accounts + the admin plugin's role
  field), **`task` + `task_session`** (§3), and later the `connections` store
  + DBOS workflow state. Migrations owned by **Drizzle** — its own pipeline,
  no shared migration step across tiers.
  - *Local:* a second database (`engram_orchestrator`) on the same Postgres
    instance (`localhost:5435`); one `CREATE DATABASE` in the local
    bootstrap.
  - *Production:* a wholly separate managed instance. Nothing in code assumes
    co-location.

Rejected shortcut: adding orchestrator tables to the coordinator's `engram`
database — it would couple the migration pipelines and rot the boundary into
shared-table coupling. Separate logical DBs even when co-located.

## Rationale

- **Split by rate-of-change and language fit.** The control plane is
  slow-moving systems code; the orchestration tier is fast-moving glue.
  Coupling them taxes every web/auth change with Rust's ergonomics — and
  would tax future webhook/workflow code harder — while risking the fleet
  brain.
- **The AWS shape for the control plane.** Raw resource API, single trusted
  caller, no end-user concepts. Every concept a product iteration touches
  (users, roles, ownership, tasks, naming) lives in the fast-moving tier;
  the control plane's contract only changes when the *infrastructure*
  changes.
- **Tasks over sessions as the aggregate root.** "What work is being done and
  why" is application data with 1:N session fan-out in its future; modeling
  it up front costs two tables and prevents retrofitting attribution onto an
  infrastructure resource.
- **AuthZ beside the data it conditions on.** Ownership lives in the task
  tables; putting the CASL check anywhere else means a cross-service lookup
  per authorization. In-process CASL + better-auth roles covers two roles and
  one ownership relation; policy services (Cerbos/OpenFGA) are deferred until
  relationship semantics (teams, sharing) exist.
- **Machine auth over identity forwarding.** With authz moved up, per-user
  JWTs to the control plane would carry information no one consumes. A
  service bearer (already implemented for host-agents) is the entire
  mechanism.
- **Protobuf over ts-rs/Eden.** One IDL across three codegen targets, with
  language-neutral breaking-change detection — the same tonic/prost stack the
  host fabric already uses.
- **Generic passthrough over hand-written endpoints.** Shared descriptors
  make a forwarding handler a client call; ~30 hand-written relays would be
  boilerplate that exists only to exist. Bespoke handlers appear exactly
  where orchestrator-owned data does (TaskService, create-with-attribution).
- **Hono on Node over Bun/Elysia.** DBOS needs Node; Node h2 de-risks the
  bidi leg; with Eden rejected, Elysia's differentiator was gone; Hono is
  where connect-es and better-auth are first-class.
- **SSE on the browser event leg.** Reconnect-with-cursor for free beats
  re-implementing `EventSource` by hand on the most correctness-sensitive
  stream; the envelope keeps it typed.
- **DBOS (when workflows land) over Temporal/queues.** Durable execution as a
  Postgres-backed library fits "growing platform, not yet Temporal-scale"
  with zero new infrastructure. Recorded as the proposed direction, not
  adopted now.

## Implications

- **New service:** the orchestration tier (Hono/Node, own Postgres; better-auth
  + tasks now, `connections` + workflow state later). New deploy unit, new
  failure mode (orchestrator down = web down), cross-hop debugging. Rolling
  deploys must drain live streams gracefully (basic SIGTERM-close now; full
  drain deferred).
- **The orchestrator is the security boundary.** Its CASL policy module and
  session-resolution are security-critical; the service-bearer credential is
  full-privilege at the control plane. Keep the control plane network-private;
  keep the policy file small and tested; fail closed in the passthrough gate.
- **Control plane changes:** expose the tonic app contract (§2.3) +
  `ShellRelayService`, with `CreateSession` carrying the `harness_env` secret
  channel (sealed per session into `session_secrets`); authenticate via
  `ServiceBearer` only; **remove** `CookieSession`, `/auth/*` OIDC,
  `SyntheticAdmin`, `require_admin`/`require_session_owner`, the `users`
  table, `sessions.user_id`, and the per-user secret store (now the
  orchestrator's `user_session_secrets`). The host-facing surface is untouched.
- **Web changes:** replace `api.ts`/`types.ts`/hand-parsed `sse.ts` with
  generated connect-es + connect-query clients; the primary list becomes
  tasks; login/Members move to better-auth(+admin plugin) UIs; the ability
  file gates affordances.
- **Non-browser clients (`engram-cli`, CI scripts)** talk to the control
  plane directly with a service bearer — they are operator tools with
  full-privilege semantics by definition; sessions they create are
  unattributed (§3) and admin-visible only in the UI.
- **Contract pipeline:** `buf generate` + breaking-change CI; protos are the
  source of truth.
- **Streaming proxy (§9) is concrete build work:** abort-signal forwarding,
  RAII release on the control-plane side, WS + HTTP/2 keepalives, and the
  SSE envelope. Never pool upstream streams.
- **Latency:** negligible for unary RPCs; the event stream and shell traverse
  one extra relay hop; the orchestrator gains the authz join (indexed, one
  hop) on session-scoped calls.
- **Deferred (own ADRs/plans later):** workflows + DBOS adoption (§4);
  webhook ingestion + integrations + the `connections` store; per-ticket
  on-behalf-of attribution; typed event payloads / server-side transcript
  shaping; team/sharing semantics (the OpenFGA trigger, §6); full
  rolling-deploy drain; migration sequencing of the `users`-table drop.

## Preparation: pin current session behavior before the cut

Do this **first**, before any orchestrator code. The migration rewrites what
sits under the UI — transport, the auth front door, the number of hops a
stream crosses — while intending to leave *user-observable session behavior
unchanged*. That intent is only worth something if it is a runnable assertion.
So we lock today's behavior into an end-to-end characterization net now,
against the **current two-tier stack**, and re-run it unchanged after the cut:
green against three tiers means the seam preserved the contract.

**Shape.** A local Playwright suite (`web/e2e/`) driving the real stack
(`just dev`) — *not* mocked. It creates a session through the UI on a
**no-harness / shell-only demo image**, so it needs no Claude token and runs no
live agent (the deterministic path `deploy/dev/integration-session.sh` already
bakes + enables that image idempotently). The suite is black-box at the
browser, which is exactly why it survives the migration: it asserts what the
user sees, not how the bytes arrive.

**The invariant it pins** (one linear journey): land on `/` with no login →
`+ new session` → `start →` → the URL becomes `/sessions/<id>` → the event
count **climbs above zero** → status advances toward `active` → the RAW tab
renders event rows → the SHELL tab's WebSocket connects → the new row appears
in the session list. Every one of these must hold identically whether the feed
underneath is today's `sse.ts` or the orchestrator-proxied envelope, and
whether it crosses one relay hop or two. (The list becoming a *task* list
post-migration is a sanctioned rendering change; the row-appears assertion is
on the testid, not the noun.)

**Mechanics.** A global setup gates on three preconditions, each failing fast
with its own fix-it line: web up on `:5173`, the control plane's health check
green, and a no-harness image enabled (else "run `just integration-session`
once"). Traces, screenshots, and video are retained on failure. A handful of
`data-testid`s keep the selectors stable against copy drift.

**Scope, honestly.** A no-harness session emits lifecycle events but no agent
turns, so this validates the *plumbing* — create, navigate, list/detail
render, stream flow, lifecycle, shell connect — not transcript *message*
rendering, which stays on the `Transcript.test.tsx` unit tests. One step
changes across the cut: the suite currently rides `SyntheticAdmin` (no login),
which §5 removes — post-migration its entry step re-points at a better-auth
dev session. The behavioral assertions are untouched.

## Appendix: request flows

### A. Human chat (web UI) — near-term

```
Browser ─cookie─▶ Orchestrator: better-auth session → user alice
                     │ CASL: can('prompt','Session',{task.createdByUserId: alice})
                     │   (session → task via task_session join)
                     ▼ gRPC + service bearer
                  Control plane: bearer interceptor (machine auth, no principal)
                                 → SendPrompt routed to the harness
```

The orchestrator is a *stateless relay* on the data path here: two rows at
create (`task`, `task_session`), then `SendPrompt` out, `StreamEvents` back.
Durability lives in the control plane — **no DBOS** (§4).

### B. Automation (Linear-triggered) — future (§4)

Three roles: **External** (Linear + Slack, via bot tokens in the `connections`
store), **Orchestrator** (the task + DBOS workflow + durable event cursor),
**Control plane**. The workflow both *commands* the control plane and *reacts*
to its event stream; every external effect is idempotent on `(issue,
event_idx)`.

```
 External (Linear / Slack)        Orchestrator (task · DBOS wf · cursor)    Control plane
        │                                  │                                  │
 (1) issue.created  ───webhook────────────▶│ verify HMAC (Linear secret)      │
        │                                  │ (2) INSERT task{type:linear_issue,
        │                                  │     source:{issueId}} + start wf │
        │                                  │     [idem = linear_event_id]     │
        │                                  │ (3) CreateSession(image, prompt) │
        │                                  │     + service bearer ───────────▶│ create; schedule sandbox
        │                                  │◀──────────── session_id ──────────┤
        │                                  │     INSERT task_session          │
 (4) issue → "In Progress" ◀───────────────┤ (bot token); task.status=working │
 (5) Slack "▶ starting <issue>" ◀──────────┤                                  │
        │                                  │ (6) StreamEvents(id, since=∅) ───▶│
        │                                  │◀──────────── run_started ─────────┤
        │                                  │◀───── agent_message / tool_call_* ┤  cursor := idx
        │                                  │◀──────── pull_request_opened{url} ┤
        │                                  │ (7) react [idem (issue, idx)]:    │
 (7) issue → "In Review" + PR link ◀───────┤     task.status=awaiting_review  │
 (7) Slack "✅ PR opened <url>" ◀───────────┤                                  │
        │                                  │◀──────── run_completed / idle ────┤
        │                                  │ (8) park awaiting review;         │
        │                                  │     cursor durably = last idx     │
```

A cron trigger is the same picture with step (1) replaced by a scheduled tick.

### C. Why DBOS for B (and not A): crash & redelivery

Session events are at-least-once (reconnect, replay, `lagged`) and the
orchestrator can crash mid-saga. DBOS makes the *effects* exactly-once:

```
crash after (7) "In Review" transition but before the Slack post
   └▶ on restart, DBOS resumes the workflow at the next step:
        • re-run Slack post → idem key (issue, event_idx) ⇒ no-op if already sent
        • re-open StreamEvents from durable cursor (since = last_idx) ⇒ no gap, no dupes
   net: exactly-once side effects despite at-least-once events + crashes
```

Path A has no such multi-step external saga — the control plane is the
durability boundary, and the task rows are written synchronously — so it needs
none of this.
