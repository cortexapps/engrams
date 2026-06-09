# ADR 0039: A TypeScript orchestration tier — splitting the application layer from the control plane

Status: 2026-06-05 — **Proposed.** No code yet. This ADR records the target
architecture and the seams; it will be split into an implementation plan with
its own commit chain. Near-term scope is the application-layer split (app API +
auth + transport, §§1–2, 5–10); **workflows and a durable-execution engine are
deferred** (§3). Supersedes the relevant transport/auth assumptions of ADR 0031
(which keeps OIDC/cookie auth *inside* the coordinator) and builds on the
existing gRPC fabric (ADR 0013) and the `ProxyShell` bidi shell tunnel
(ADR 0014).

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
the app API, session auth, the drift-prone hand-mirrored types — want to evolve
faster than a Rust systems binary comfortably allows. **Forward-looking**, a new
class of work is coming that does not belong in Rust at all: an **orchestration
layer** for workflows. The motivating example — *a Linear ticket (via webhook)
spins up a session; as the session opens a PR, moves the ticket through stages
and posts to Slack* — is stateful, event-driven, integration-heavy glue whose
business rules change weekly, and cron-triggered automations are the same shape.
That is TypeScript's home turf and Rust's worst ergonomics. We introduce the
tier **now** for the application-layer split (API + auth + transport), and shape
it deliberately so those **future** workflows land in TS where they belong,
rather than bolted onto the slowest-moving, most safety-critical part of the
system.

We also already feel the smaller pain: `web/src/types.ts` is hand-mirrored
from Rust structs with no codegen, so the wire contract drifts silently.

## Decision

### 1. Three tiers, with a clean cut

```
   Linear / GitHub / Slack webhooks ──┐ (future)
                                       ▼
   React UI  ◄──────────────►  TS Orchestration Tier  ◄──────────►  Rust Control Plane
                               (Bun / Elysia)                       (engram-coordinator)
                               • the public app API                 • microVMs / snapshots
                               • human auth (better-auth)            • the SESSION EVENT LOG
                               • durable state (its own Postgres)    • gRPC host fabric / ttyd
                               • inbound webhooks        (future)    • KEK/KMS, COW, scheduler
                               • outbound integrations   (future)    • users + roles + authZ
                               • durable workflows — DBOS (future)   • exposes: commands + streams
```

The **control plane** becomes a pure internal control API: it keeps everything
hard and real-time and sheds its "web server" identity. The **orchestration
tier** owns every human- and integration-facing concern. The **UI** becomes
one of several clients of the orchestration tier (webhooks and scheduled
workflows are the future others).

The governing principle for the cut: *the orchestrator both commands the
control plane (create session, send prompt) and reacts to it (consume session
events → external side effects).* It sits on both sides of the control plane;
the control plane knows nothing about Linear, Slack, cookies, or OIDC.

### 2. Ownership boundary and the control-plane contract

There are **two contracts**, and only one is new:

- the **orchestrator-facing app contract** — a new tonic gRPC surface the
  orchestrator consumes, defined in §2.3 below (derived from today's `/api/v1`
  HTTP routes);
- the **host-facing internal contract** — the *existing* `host_service.proto`
  (coordinator ↔ host-agent) plus the host→coordinator ingest routes
  (`register`, `heartbeat`, `harness-events`, `idle-eviction-candidates`,
  `live-manifest`, `resolve-registry`). This stays internal, unchanged, and the
  orchestrator never touches it.

#### 2.1 What the control plane owns (the substrate)

- **Session lifecycle & state machine** — `pending → created → active → idle →
  evicting/…`; create / get / list / delete; server-stamped ownership
  (`sessions.user_id`).
- **Harness management** — *which* agent runs in a session is selected from the
  session's image (the enabled-image's declared builtin, e.g. Claude); prompt
  routing, interrupt, and the in-process harness hub that bridges to the
  host-agent's harness (`BindHarnessSession` / `SendHarnessPrompt` /
  `InterruptHarness`). The auto-injection of the user's Claude token (ADR 0031)
  stays here.
- **Sandbox / microVM orchestration** — scheduling onto hosts, the gRPC host
  fabric (ADR 0013), sandbox create/destroy, exec, the `ProxyShell` tunnel.
- **Durability** — snapshot / resume / idle eviction (ADR 0034), chunked
  manifests + COW (ADR 0007/0016), blob storage, checkpoints.
- **The session event log** — the authoritative append-only log (monotonic
  `idx`, recovery epochs) and the PG LISTEN/NOTIFY fan-out. The orchestrator
  *consumes* it; it does not own it.
- **Image enablement** — OCI pulls, enable jobs, manifest caching (ADR 0036).
- **Registries & secrets** — KEK-sealed registry creds, secret resolution.
- **Identity for authZ** — `users` + roles are **authoritative** here;
  owner-scoping + admin gating (ADR 0031); JIT-provision by email (§5/§6).
- **Fleet & storage** — hosts, capacity, drain/cordon, storage rollups, GC.
- **Artifacts** — session file artifacts (ADR 0026).

#### 2.2 What the orchestrator owns

- **The public app API** — the Connect surface the UI consumes (aggregation /
  view-models over the control-plane contract).
- **Human authentication** — better-auth (login, session, cookie), the IAP
  bridge, and minting the downstream forward-auth JWT + serving JWKS (§5).
- **The admin UI's identity views** — drives the control plane's authoritative
  `Users` RPCs; holds no role state of its own.
- **Its own durable state** — better-auth tables now; the `connections` store +
  workflow state when workflows land.
- **(future, §3/§4)** webhook ingestion, outbound integrations, workflows.

Authorization is **not** in this list — it stays in the control plane (§6).

#### 2.3 The contract: control-plane gRPC exposed to the orchestrator

Proposed service groupings; each maps to a current `/api/v1` route. Names are
indicative, not final.

**`SessionService`** (the core app surface — session + harness + streams):

| RPC | Replaces (today) | Notes |
|-----|------------------|-------|
| `ListSessions(scope: mine\|all)` | `GET /sessions` | scope=all is admin-gated |
| `CreateSession(image_uri, mode, overrides?, prompt?)` | `POST /sessions` | **selects the harness via the image**; auto-injects Claude token when builtin=claude |
| `GetSession(id)` / `DeleteSession(id)` | `GET`/`DELETE /sessions/:id` | |
| `SendPrompt(id, text)` | `POST /sessions/:id/prompt` | routes to the harness; auto-resumes idle |
| `Interrupt(id)` | `POST /sessions/:id/interrupt` | stops the in-flight run (ADR 0030) |
| `Exec(id, req)` / `ExecStream(id, req) → stream` | `POST /sessions/:id/exec[/stream]` | server-streaming |
| `StreamEvents(id, since) → stream SessionEvent` | SSE `GET /sessions/:id/events` | **server-streaming, typed**; retires `sse.ts` |
| `GetLog(id, kind, limit)` | `GET /sessions/:id/log` | conversation transcript |
| `Snapshot(id)` / `Resume(id)` / `EvictLocal(id)` | `POST snapshot`/`resume`, `DELETE local` | lifecycle |
| `GetCowState(id)` / `ListCheckpoints(id)` | `GET cow-state`/`checkpoints` | durability views |
| `ProxyShell(stream) → stream` | WS `GET /sessions/:id/shell` | **gRPC bidi**, reuses `ProxyShellMessage` (§8) |
| `GetArtifact(id, artifact_id)` / `CreateArtifactFromPath(id, path)` | artifacts (ADR 0026) | |

**`FleetService`** (operator / admin):

| RPC | Replaces |
|-----|----------|
| `ListHosts` / `GetHost` / `GetHostCowState` | `GET /hosts*` |
| `DrainHost` / `CordonHost` / `UncordonHost` | `POST /hosts/:id/drain`, `/admin/hosts/:id/*` |
| `StorageSummary` | `GET /storage/summary` |
| `FlushSession` / `EvacuateSession` | `/admin/sessions/:id/*` |
| `ChunkGc` / `BundleGc` / `SnapshotBlobGc` (dry-run + sweep) | `/admin/*-gc/*` |

**`ImageService`** (admin):

| RPC | Replaces |
|-----|----------|
| `ListEnabledImages` / `EnableImage` / `DisableImage` / `RefreshImage` | `/enabled-images*` |
| `ListEnableJobs` / `GetEnableJob` / `RetryEnableJob` | `/enable-jobs*` |
| `ListRegistries` / `AddRegistry` / `DeleteRegistry` | `/registries*` |

**`UserService`** (authoritative identity — drives the orchestrator's admin UI):

| RPC | Replaces | Notes |
|-----|----------|-------|
| `ListUsers` | `GET /admin/users` | admin-gated |
| `PatchUser(id, role?, active?)` | `PATCH /admin/users/:id` | role is authoritative here (ADR 0031) |
| `SaveClaudeToken(token)` / `GetUser(id)→{…, has_claude_token}` | `POST /me/claude-token` (storage half) | **sealing under KEK + auto-inject at CreateSession stay in Rust**; the orchestrator owns the UX and relays |
| *(provisioning)* | — | implicit JIT-upsert-by-email on the forward-auth assertion — no RPC |

What is **not** in this contract: `/me`, `/auth/*` — fully orchestrator-owned
(§5). `/me/claude-token` is **split**: the orchestrator owns the profile
endpoint/UX, but the KEK sealing, storage, and session-create injection stay in
the control plane behind `SaveClaudeToken` above. On this path the orchestrator
is an **opaque relay** — it forwards the raw token straight to `SaveClaudeToken`
without deserializing, logging, or persisting it, so the plaintext secret never
becomes custody it holds (it transits the orchestrator the same way it transits
any TLS hop). `/healthz`/`/readyz` stay plain HTTP on the control plane for
liveness probes.

### 3. Workflows are future work; DBOS is the proposed engine

We are **not** building a workflow engine now. The near-term tier ships the
application-layer split (§§5,7,8) only. This section records the *intended* home
for future workflows — Linear-/webhook-triggered and cron-triggered automations
(*ticket → session → PR → Slack* and the like) — so the tier is shaped to host
them, not so we build them yet.

When workflows land, the **proposal** is **DBOS** (durable execution as a
library, backed by the orchestrator's Postgres) over a standalone engine
(Temporal) or a bare queue (pg-boss): durable state, retries, and idempotent
steps with no new infrastructure. Two properties will be **non-negotiable**
then, because session events can redeliver (reconnect, replay, the `lagged`
event):

- **Idempotency** on every outbound effect, keyed on `(session_id, event_idx)`.
- **Durable consumption cursor.** The control plane's per-session event log
  already carries a monotonic `idx` with `?since=`/`Last-Event-ID` replay, so a
  consumer resumes from "processed session X through idx N" with at-least-once
  delivery and **no message broker**.

The event model we already built is the integration backbone — which is why
deferring workflows costs us nothing in the control plane now. Nothing in this
ADR's near-term scope depends on DBOS; it is a deferred decision.

**Not for the human chat path.** The interactive chat experience needs none of
this. There the orchestrator is a *stateless relay* — it forwards `SendPrompt`
and re-streams `StreamEvents` — and the **control plane is already the
durability boundary**: the session, its state machine, and the authoritative
event log survive any orchestrator restart, and the UI reconnects with a `since`
cursor. There is no orchestrator-side multi-step, side-effecting saga to make
durable, so DBOS would add a persistence hop and latency for zero benefit. It
earns its place only for **orchestrator-initiated** flows that fan out to
external systems and must survive crashes with idempotent retries (Appendix B/C).

### 4. Integrations (future): workspace-level installs, not user logins

Integrations ship with workflows (§3), not now. Recorded here so the design is
settled when we get there: the Slack/Linear integrations are **per-workspace
app installs** ("the engram workspace connected Linear once; a shared bot acts
for everyone"), not per-user "sign in with" flows. This means:

- A dedicated **connections store** (the orchestrator's Postgres) holds the
  install/bot tokens — *not* better-auth's account table (which is user-scoped
  and the wrong shape for a shared bot token). The OAuth *flow* to obtain the
  token may reuse better-auth's machinery, but the token lands in `connections`.
- **Inbound webhook verification** (Slack signing secret, Linear HMAC) lives in
  the orchestrator's webhook routes and has nothing to do with better-auth — it
  is signature-checking inbound POST bodies.
- Automation-created sessions are owned by a dedicated **low-privilege service
  identity** (`automation@engram.local`, role `member`) — see §6 — rather than
  attributed per-ticket to a human. (Attributing to the ticket assignee remains
  possible later by asserting their email; the JWT seam in §5 makes it nearly
  free.)

### 5. The orchestrator is the authentication authority + a trusted JWT issuer

Human identity moves out of the control plane entirely. The orchestrator owns
login, the session cookie, and the `WebSessionStore`-equivalent, via
**better-auth**. The control plane's three human auth modes (ADR 0031: oidc /
forward-auth / none) **collapse to one**: it always trusts a signed assertion
from the orchestrator (forward-auth), plus `ServiceBearer` for host-agents.
`CookieSession`, the `/auth/*` OIDC endpoints, and `SyntheticAdmin` all leave
the coordinator.

Identity reaches the control plane exactly the way IAP reaches the coordinator
today — a **short-lived signed JWT verified against JWKS**, which is precisely
what `ForwardAuthVerifier` (`crates/engram-auth/src/forward.rs`) already does.
We point its `jwks_url`/`issuer`/`audience` at the orchestrator instead of GCP
IAP. No new verifier.

- The orchestrator holds the private signing key; the control plane holds only
  the public JWKS. **better-auth's JWT plugin** signs the tokens *and* serves
  `/.well-known/jwks.json`, so one component does session management, downstream
  assertion, and key publication.
- The JWT asserts **identity only** (`sub`/`email`, `exp` ~60s, `iss`/`aud`).
  The control plane resolves *role* from its own `users` table. So the
  orchestrator decides *who*; the control plane decides *what*.

**Two inbound human-auth paths in the orchestrator**, both ending in the same
downstream assertion:

- *Self-hosted / no proxy:* better-auth runs OIDC ("Sign in with …"), owns the
  session. (replaces coordinator `oidc` mode)
- *Behind GCP IAP:* a thin middleware verifies the `X-Goog-IAP-JWT-Assertion`
  JWT against Google's JWKS and **programmatically creates a better-auth
  session** from the verified email, skipping the login UI. (replaces
  coordinator `forward-auth` mode). IAP is *not* a better-auth provider — it is
  a trusted-SSO bridge that feeds a session. This path is a near-direct port of
  `forward.rs` into TS.

### 6. Authorization stays in the control plane

`require_admin` and `require_session_owner` (`api/principal.rs`) are
**untouched**. They run on the principal the control plane resolves from the
orchestrator's assertion. So:

- **Human request (near-term):** browser →(cookie)→ orchestrator resolves
  principal →(JWT `sub: alice@corp`)→ control plane → JIT-upsert by email → role
  from DB → `require_session_owner` scopes to Alice's sessions. Even an
  aggregation bug in the orchestrator cannot let Alice read Bob's session *as
  Bob* — the control plane re-checks ownership.
- **Automation (future, with §3 workflows):** Linear webhook →(verify HMAC)→
  workflow →(JWT `sub: automation@engram.local`)→ control plane. Sessions are
  owned by the low-privilege automation identity; owner-scoping boxes a buggy
  workflow to that user.

**Honest caveat:** because the orchestrator can sign *any* identity (including
an admin's email), its session→principal resolution is security-critical — this
is inherent to forward-auth (IAP has the identical property). Mitigations: never
assert an identity not bound to an authenticated session; pin automation to the
`member` service identity; short JWT TTL caps replay; keep the control plane on
a private network / mTLS (defense-in-depth, and how host-agents reach it).

### 7. Transport: Buf + Connect/gRPC end-to-end (retires hand-mirrored types)

Protobuf becomes the single source of truth for both seams, replacing the
hand-written `types.ts` *and* the ts-rs/Eden codegen we considered. `.proto`
files in `engram-protocol`; `buf generate` emits both sides of both hops; `buf`
breaking-change detection runs in CI.

- **Control plane ↔ orchestrator:** the control plane exposes a **tonic** gRPC
  service generated from the protos, alongside the existing `host_service.proto`
  — this is the *same* stack (tonic 0.12 / prost 0.13) the host fabric already
  runs, so it is an extension of an existing pattern, not a new paradigm. The
  orchestrator consumes it with **connect-es** (Connect clients speak gRPC
  natively to tonic).
- **UI ↔ orchestrator:** **connect-es + connect-query** gives typed clients with
  TanStack Query bindings — a near drop-in for today's `useSessions`/`useSession`
  hooks, minus `types.ts`. (Eden is *not* used; Connect's codegen supersedes it.)

What stays plain HTTP (and is why Elysia remains the orchestrator's host
process): **better-auth login** (cookie/redirect flows) now, and **inbound
webhooks** when workflows land (their HTTP+HMAC contracts, not our RPCs). Browser
Connect RPCs carry the better-auth cookie; an auth interceptor validates it and
mints the downstream JWT.

### 8. Streaming: server-streaming for events, bidi gRPC for the shell

gRPC/Connect call shapes map onto our two streams:

- **Session event feed (was SSE):** a **server-streaming RPC**. connect-es
  supports server-streaming in the browser over HTTP, so the orchestrator
  consumes the control plane's server-stream and re-exposes a server-stream to
  the UI. The PG LISTEN/NOTIFY fan-out stays in the control plane. This *retires
  the hand-parsed `sse.ts`*: events become typed protobuf messages.
- **Shell (bidirectional):** the browser↔orchestrator leg **stays a
  WebSocket** — browsers cannot do bidi over fetch, full stop. The
  orchestrator↔control-plane leg is **gRPC bidi**, reusing the existing
  `ProxyShellMessage` type (`text/binary/ping/pong/close` — already exactly the
  WS frame kinds) and mirroring the control-plane↔host-agent `ProxyShell`
  tunnel. The orchestrator terminates the browser WS and maps frames ↔
  `ProxyShellMessage`; the control plane becomes a near-passthrough gRPC↔gRPC
  relay.

  Bidi requires an HTTP/2 **client** on the orchestrator. We verified Bun's
  HTTP/2 client is now solid, so connect-es bidi on Bun is viable; this removes
  the only reason to special-case the internal shell leg as a WebSocket. (Non-
  shell RPCs are unary + server-streaming and run over Connect on HTTP/1.1, so
  the shell is the only surface that needs HTTP/2-client.)

#### Transport map

| Surface | Transport |
|---|---|
| UI ↔ orchestrator (app API) | Connect (unary + server-streaming) |
| UI ↔ orchestrator (shell) | **WebSocket** (browser bidi limit) |
| Orchestrator ↔ control plane (app API) | gRPC / tonic |
| Orchestrator ↔ control plane (event feed) | gRPC server-streaming |
| Orchestrator ↔ control plane (shell) | gRPC **bidi** (`ProxyShellMessage`) |
| Control plane ↔ host-agent (shell) | gRPC bidi (existing, ADR 0014) |
| Inbound webhooks (Linear/Slack/GitHub) | plain HTTP + HMAC *(future, §3)* |
| Human login | HTTP/cookie (better-auth) + IAP-bridge middleware |

### 9. Streaming lifecycle: mapping, reconnect, teardown

Both streams now traverse two hops. The model that keeps this correct is
**stateless passthrough**: the orchestrator holds *no* `session_id → stream`
registry. Each browser stream is a lifetime-bound pipe to exactly one upstream
stream, and the **cursor — not the orchestrator — carries resumability**.

#### 9.1 Mapping model

- One browser stream → one upstream stream, opened **on demand**, torn down on
  disconnect. The "mapping" is the request handler's async scope, not a global
  map — so there is nothing to leak, GC, or reconcile.
- gRPC multiplexes many logical streams over one HTTP/2 connection, so *N*
  upstream streams is **not** *N* sockets — there is no connection-count reason
  to pool, and pooling would break the teardown guarantees below.
- **Lease/subscription lifetime == upstream stream lifetime.** A shell's
  `AcquireShell` lease and an event stream's PG-bus subscription live inside the
  proxied call's scope and die with it.
- Two tabs on one session are two independent pipes; the control plane's PG
  fan-out feeds both. The orchestrator need not know they are "the same session."

#### 9.2 Establishing a stream (new session / new shell)

- **Events:** browser opens `StreamEvents(sessionId, since=-1)` (better-auth
  cookie) → orchestrator resolves principal, mints the forward-auth JWT, opens
  the **upstream** `StreamEvents` with the JWT, and pipes. The control plane
  validates + owner-scopes, subscribes to the PG bus, replays then tails.
- **Shell:** browser WS(`"tty"`) → orchestrator opens an upstream `ProxyShell`
  bidi (first frame `ProxyShellOpen{sandbox}`) and pumps frames. The control
  plane `AcquireShell` pins the sandbox against idle-eviction for the stream's
  lifetime. (See Appendix A/B for full sequences.)

#### 9.3 Reconnect (close tab, return, reopen)

- **Events are replayable.** On reconnect the client passes `since = last_idx`
  it had applied; the orchestrator opens a fresh upstream with that cursor; the
  control plane replays from its log then tails. The orchestrator remembers
  nothing — the stream is re-derived from the client's cursor + the control
  plane's authoritative log. An idle session **auto-resumes** on this call, as
  today, transparently through the proxy.
- **Shells are not replayable.** A terminal is a live PTY, not a log; reopening
  is a fresh `ProxyShell` + `AcquireShell`. Prior scrollback survives only if
  persisted *inside the guest* (tmux/ttyd) — unchanged by the orchestrator.
- **Reconnect is now the client's job.** Connect server-streaming has no
  `EventSource`-style auto-reconnect, so the connect-query hook must track the
  highest applied `idx` and re-subscribe with `since` on stream drop (a thin
  wrapper). *(Alternative: keep SSE on the browser↔orchestrator leg only, buying
  `EventSource` reconnect at the cost of an untyped browser leg.)*

#### 9.4 Cancellation & teardown

Teardown is a cascade of one primitive per hop:

```
Browser ──[A]──▶ Orchestrator ──[B]──▶ Control plane ──[C]──▶ host-agent ──▶ ttyd
 HTTP/WS close    AbortSignal           tonic future drop      ProxyShell end    PTY dies
                  → gRPC RST_STREAM      → RAII release
```

- **[A]→[B] is all the orchestrator does.** Events: forward the handler's
  `ctx.signal` (fires on browser disconnect) straight into the upstream call's
  `signal` — a one-liner; aborting it sends HTTP/2 `RST_STREAM` to tonic.
  Shell: an `AbortController` bridges `ws.onclose → abort()` (browser-initiated)
  while the upstream stream ending closes the browser WS (upstream-initiated) —
  teardown is bidirectional.
- **[B]→[C] + cleanup is existing control-plane behavior.** The reset drops the
  tonic handler future: a RAII guard releases the lease (`ReleaseShell`) /
  unsubscribes from the bus, and the dropped downstream `ProxyShell` stream ends
  → host-agent closes its WS to ttyd → the PTY dies. *Rust caveat:* `Drop`
  cannot `await`, so async release either spawns a detached task or runs
  explicitly on the relay loop's exit — never a bare async call in `Drop`.
- **Silent death (half-open).** A slept laptop sends no FIN, so configure
  **keepalives** at each hop — WS ping/pong to the browser, HTTP/2 keepalive
  PINGs on the orchestrator↔control-plane channel — or a ghost shell holds its
  lease until a coarse timeout.
- **Orchestrator crash = mass cancellation.** A dead orchestrator drops every
  HTTP/2 connection, resetting all in-flight streams at once → the control plane
  releases every lease/subscription. This crash-safety holds **only because**
  lease lifetime == stream lifetime == one browser connection (§9.1) — pooling
  upstream streams would forfeit it.

#### 9.5 The one long-lived stream the orchestrator owns (future)

A workflow (future, §3) consuming a session opens its **own** `StreamEvents`
with a **durable** cursor, independent of any browser. So a session may have
`0..N` ephemeral browser pipes **plus** `0..1` durable workflow consumer — all
independent subscribers to the control plane's broadcast, none aware of each
other. Durability is isolated to the consumer that needs it; the human path
stays stateless.

### 10. Databases: a separate store per tier, env-configured

Each tier owns its **own** database; the tiers **never share tables**. The only
cross-tier data flow is the gRPC contract (§2.3) — neither service connects to
the other's database. This keeps the boundary honest and lets the two schemas
and migration pipelines evolve independently.

- **Control plane — unchanged.** Keeps its `engram` Postgres: `DATABASE_URL`
  (clap `env="DATABASE_URL"`), migrations in `deploy/migrations` (sqlx). This
  ADR touches none of it.
- **Orchestrator — new, separate DB.** Holds better-auth's tables now (users
  mirror, sessions, accounts, verification), and the `connections` store +
  DBOS workflow state when those land (§3/§4). Near-term it is *just* better-auth.
- **Config drives topology, not code.** The orchestrator reads its connection
  string from its own env var — **`ORCHESTRATOR_DATABASE_URL`** (distinct name so
  it never collides with the coordinator's `DATABASE_URL` in a shared `.env`):
  - *Local:* a second database (e.g. `engram_orchestrator`) on the **same**
    Postgres instance the coordinator already runs (today `localhost:5435`). One
    `CREATE DATABASE` added to the local PG bootstrap — no new container.
  - *Production:* point it at a **wholly separate** managed Postgres instance.
    Nothing in code assumes co-location.
- **Migrations: Drizzle.** The orchestrator's schema is owned by **Drizzle**
  (`drizzle-kit` for generation + apply) — its own pipeline, *not* the
  coordinator's sqlx runner in `deploy/migrations`, and no shared migration step
  across tiers. better-auth's tables are defined through its **Drizzle adapter**,
  so they live in the same Drizzle schema and migration history as the
  `connections` store and the rest of the app schema. DBOS manages its own
  workflow tables separately when it lands (§3).

Rejected shortcut: adding orchestrator tables to the coordinator's `engram`
database. It would couple the two migration pipelines and let the boundary rot
into shared-table coupling — exactly what the tier split exists to prevent.
Separate logical DBs even when co-located.

## Rationale

- **Split by rate-of-change and language fit, not by layer cosmetics.** The
  control plane is slow-moving systems code; the orchestration tier is
  fast-moving glue. Coupling them in one Rust binary taxes every web/auth change
  with Rust's ergonomics — and would tax future webhook/SDK/workflow code even
  harder — while risking the fleet brain.
- **Build the tier now, defer the workflows.** The split pays for itself
  immediately on app API + auth + transport. Workflows are the *reason the tier
  is TypeScript* (so they have a home), but they need not block the split — and
  the event-log cursor means deferring them costs the control plane nothing.
- **Forward-auth was already the seam.** ADR 0031's `ForwardAuthVerifier` was
  built so a trusted upstream can assert identity via a signed JWT. The
  orchestrator *is* that upstream. We reuse the verifier and collapse the
  control plane to one auth mode rather than adding one.
- **AuthZ stays in Rust.** Authentication (who) is delegated; authorization
  (what) is not. The control plane re-checks ownership/role on every call, so a
  bug in fast-moving TS cannot widen access.
- **Protobuf over ts-rs/Eden.** One IDL across three codegen targets, with
  language-neutral breaking-change detection — and it is the *same* tonic/prost
  stack the host fabric already uses. ts-rs would have been one-directional and
  Rust-only; Eden would have been a second, separate seam.
- **DBOS (when workflows land) over Temporal/queues.** Durable execution as a
  Postgres-backed library fits "growing platform, not yet Temporal-scale" with
  zero new infrastructure. Recorded as the proposed direction, not adopted now.
- **gRPC bidi for the internal shell leg.** With Bun HTTP/2 confirmed, reusing
  `ProxyShellMessage` is more consistent and simplifies the control plane to a
  relay; the WS-only fallback is no longer needed.

## Implications

- **New service:** the orchestration tier (Bun/Elysia, its own Postgres via
  `ORCHESTRATOR_DATABASE_URL` — separate DB, never shared tables, §10; better-auth
  now, `connections` + workflow state later). Local dev adds one `CREATE DATABASE`
  on the existing PG instance. New deploy unit, new failure mode (orchestrator
  down = web down even if
  the control plane is healthy), cross-hop debugging. Rolling deploys must
  **drain** live server-streams and shell connections gracefully.
- **Control plane changes:** expose a tonic gRPC service for the app API +
  event server-stream + a `ProxyShell`-style bidi relay for the orchestrator;
  keep `require_admin`/`require_session_owner`; **remove** `CookieSession`, the
  `/auth/*` OIDC endpoints, and `SyntheticAdmin` from the chain (forward-auth +
  service-bearer remain). `users` + roles stay authoritative, JIT-provisioned by
  email from the assertion. It now serves gRPC (tonic) and the legacy axum WS —
  either on separate ports or multiplexed via `tower` (build-time choice).
- **Web changes:** replace the hand-written `api.ts`/`types.ts`/`sse.ts` with
  generated connect-es + connect-query clients; the shell `TerminalPane` keeps
  its WebSocket but points at the orchestrator.
- **Contract pipeline:** `buf generate` + breaking-change CI in the monorepo;
  protos are the source of truth.
- **Streaming proxy (§9) is concrete build work, not free relay:** forward
  `AbortSignal`s downstream→upstream (cancellation), wire RAII lease/subscription
  release on the control-plane side, configure WS + HTTP/2 keepalives for
  half-open detection, and write the client-side reconnect-with-cursor wrapper
  that `EventSource` gave us for free. Never pool upstream streams across browser
  connections (it forfeits crash-safe teardown).
- **Security surface:** the orchestrator's signing key is now a high-value
  secret (it can mint any identity). Its session-resolution logic is
  security-critical. Keep the control plane network-private.
- **Latency:** negligible for unary RPCs; the event stream and shell now traverse
  one extra relay hop, and the orchestrator holds 2× the connection count
  (browser-side + upstream).
- **Deferred (own ADRs/plans later):** workflows + the DBOS adoption (§3);
  webhook ingestion + integrations + the `connections` store (§4); per-ticket
  on-behalf-of attribution; the inventory of which app endpoints are pure
  passthrough vs aggregated/view-modelled in the orchestrator; whether
  server-side transcript shaping (`SessionEvent` → assistant-ui messages) moves
  into the orchestrator or stays in the browser; migration sequencing (strangler
  vs big-bang).

## Preparation: pin current session behavior before the cut

Do this **first**, before any orchestrator code. The migration rewrites what
sits under the UI — transport (SSE → Connect server-streaming, §8), the auth
front door (§5), and the number of hops a stream crosses (one → two, §9) —
while intending to leave *user-observable session behavior unchanged*. That
intent is only worth something if it is a runnable assertion. So we lock today's
behavior into an end-to-end characterization net now, against the **current
two-tier stack**, and re-run it unchanged after the cut: green against three
tiers means the seam preserved the contract.

**Shape.** A local Playwright suite (`web/e2e/`) driving the real stack
(`just dev`) — *not* mocked. It creates a session through the UI on a
**no-harness / shell-only demo image**, so it needs no Claude token and runs no
live agent (the deterministic path `deploy/dev/integration-session.sh` already
bakes + enables that image idempotently). The suite is black-box at the browser,
which is exactly why it survives the migration: it asserts what the user sees,
not how the bytes arrive.

**The invariant it pins** (one linear journey): land on `/` with no login →
`+ new session` → `start →` → the URL becomes `/sessions/<id>` → the event count
**climbs above zero** (the stream is live) → status advances toward `active` →
the RAW tab renders event rows → the SHELL tab's WebSocket connects → the new
row appears in the session list. Every one of these must hold identically
whether the event feed underneath is today's hand-parsed `sse.ts` or tomorrow's
server-streaming RPC, and whether it crosses one relay hop or two (§9).

**Mechanics.** A global setup gates on three preconditions, each failing fast
with its own fix-it line: web up on `:5173`, the control plane's health check
green, and ≥1 enabled image (else "run `just integration-session` once" — we do
**not** auto-bake a multi-minute image build into the test run). Traces,
screenshots, and video are retained on failure: those artifacts are what let an
agent self-diagnose a red run. A handful of `data-testid`s on the session row,
the event count, and the tab buttons keep the selectors stable against copy
drift.

**Scope, honestly.** A no-harness session emits lifecycle events but no agent
turns, so this validates the *plumbing* — create, navigate, list/detail render,
stream flow, lifecycle, shell connect — not transcript *message* rendering,
which stays on the `Transcript.test.tsx` unit tests with fixture events. And one
step does change across the cut: the suite currently rides `SyntheticAdmin` (no
login), which §5 removes — post-migration its entry step re-points at a
better-auth dev session. The behavioral assertions are untouched; only how the
suite gets in the door moves.

## Appendix: request flows

### A. Human chat (web UI) — near-term

```
Browser ─cookie─▶ Orchestrator (better-auth session → principal alice@corp)
                     │ mint JWT { sub: alice@corp, exp:+60s }, sign w/ orch key
                     ▼ Connect/gRPC + JWT
                  Control plane: ForwardAuthVerifier validates vs orch JWKS
                                 → JIT-upsert users row → role from DB
                                 → require_session_owner (session.user_id == alice?)
```

The orchestrator is a *stateless relay* here: `SendPrompt` out, `StreamEvents`
back. Durability lives in the control plane — **no DBOS** (§3).

### B. Automation (Linear-triggered) — future (§3)

Three roles: **External** (Linear + Slack, via bot tokens in the `connections`
store), **Orchestrator** (the DBOS workflow + durable event cursor), **Control
plane**. The workflow both *commands* the control plane and *reacts* to its
event stream; every external effect is idempotent on `(issue, event_idx)`.

```
 External (Linear / Slack)        Orchestrator (DBOS wf · cursor)        Control plane
        │                                  │                                  │
 (1) issue.created  ───webhook────────────▶│ verify HMAC (Linear secret)      │
        │                                  │ (2) start workflow               │
        │                                  │     [idem = linear_event_id]     │
        │                                  │ (3) CreateSession(image,          │
        │                                  │     mode=agent, prompt=body)      │
        │                                  │     + JWT{sub:automation} ───────▶│ create; owner=automation;
        │                                  │◀──────────── session_id ──────────┤ schedule sandbox + harness
 (4) issue → "In Progress" ◀───────────────┤ (bot token)                      │
 (5) Slack "▶ starting <issue>" ◀──────────┤                                  │
        │                                  │ (6) StreamEvents(id, since=-1) ──▶│
        │                                  │◀──────────── run_started ─────────┤
        │                                  │◀───── agent_message / tool_call_* ┤  cursor := idx
        │                                  │◀──────── pull_request_opened{url} ┤
        │                                  │ (7) react [idem (issue, idx)]:    │
 (7) issue → "In Review" + PR link ◀───────┤                                  │
 (7) Slack "✅ PR opened <url>" ◀───────────┤                                  │
        │                                  │◀──────── run_completed / idle ────┤
        │                                  │ (8) park awaiting review;         │
        │                                  │     cursor durably = last idx     │
```

A cron trigger is the same picture with step (1) replaced by a scheduled tick;
everything from (2) on is identical.

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
durability boundary — so it needs none of this.
