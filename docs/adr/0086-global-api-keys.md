# 0086 — Global API keys: admin-minted programmatic credentials riding the existing session seams

Status: Accepted (2026-07-09)

Builds on: ADR 0051 (the TypeScript orchestration tier — better-auth cookie
sessions, CASL authorization, the two-plane auth split this ADR extends), ADR
0057 (native admin-gated Connect services on the orchestrator router — the
`OrgSecretService` shape `ApiKeyService` mirrors).

## Problem

1. **engrams has no programmatic credential.** Every inbound request is
   authenticated by a better-auth browser-session cookie resolved via
   `auth.api.getSession({headers})`. CI jobs, scripts, and external tools
   cannot call the orchestrator API at all — the only bearer credentials in
   the system are machine-to-machine (orchestrator→coordinator
   `CONTROL_PLANE_BEARER`, host-agent `ENGRAM_AUTH_TOKENS`), neither of which
   is an app-tier identity.
2. **Any credential must ride the existing RBAC unchanged.** Authorization is
   `user.role` (`'admin'|'user'`) → CASL ability (`abilityFor`) → the
   passthrough policy map / native-service `requireAdmin` / Hono guards, with
   session ownership keyed on `task.createdByUserId`. A parallel identity or
   permission model would fork every seam.
3. **Keys must be operator-shaped**: named, optionally expiring, revocable,
   and admin-only to mint — a key is deployment infrastructure (like an org
   secret), not a user convenience.

## Decision

**Admin-minted global API keys, stored hashed by the `@better-auth/api-key`
plugin, each owned by a dedicated un-log-in-able service-account user row
carrying the key's assigned role. A request bearing a key (`x-api-key` or
`Authorization: Bearer engk_…`) resolves to a plugin-mocked session for that
service user at the existing `getSession` seams — so CASL, the policy map,
and `createdByUserId` ownership work with zero changes.**

The invariant: *there is exactly one identity shape (`{user: {id, role}}`)
and one resolution point (`getSession`); an API key is a second way to reach
it, never a second authorization path.*

### A. Plugin, not hand-rolled crypto (`@better-auth/api-key`)

better-auth 1.6.16 ships the apiKey plugin as the separate lockstep package
`@better-auth/api-key@1.6.16` (pinned; must be upgraded in lockstep with
`better-auth`). It provides SHA-256 hashing at rest, a masked `start`
preview, `engk_` prefixing (`defaultPrefix`), expiration enforced at verify
time (expired rows are auto-deleted), and — the load-bearing piece —
`enableSessionForAPIKeys: true`: a plugin hook that turns a request carrying
a valid key into a mock session for the key's owning user at
`auth.api.getSession()`, including server-side invocations.

Config: `customAPIKeyGetter` reads `x-api-key` first, else
`Authorization: Bearer engk_…` (the prefix gate prevents collision with any
other bearer usage) via the shared dependency-free helper
`extractApiKey()` (`orchestrator/src/auth/api-key-header.ts`). Per-key rate
limiting is disabled (`rateLimit: { enabled: false }` — the stock default of
~10 req/day would break programmatic callers). `keyExpiration:
{ minExpiresIn: 1, maxExpiresIn: 3650 }` (days).

### B. Identity: one service-account user per key

`CreateApiKey` JIT-creates a `user` row — `email =
apikey+<uuid>@service.local`, `name = <key name>`, `role = <assigned role>`,
`emailVerified: true`, **no `account` row** — and points the key's
`referenceId` at it (FK, `onDelete: cascade`). Service accounts are
structurally un-log-in-able: email/password sign-in requires an `account`
row, and IAP/OIDC can never attest a `@service.local` email.

Why per-key (not per-role shared, not metadata-carried role):

- The mock session yields exactly `{id, role}` — `abilityFor`, the policy
  map, `resolveSessionOwner`, and `task-create.ts`'s `createdByUserId`
  stamping are untouched.
- Per-key attribution and ownership isolation: a user-role key cannot read
  or delete another key's sessions.
- Revocation is a single cascade: delete the service user → the key row
  dies. `task.createdByUserId` has no FK (dangling id = "automation",
  already tolerated); `port_exposure.ownerUserId` cascades (desired).

Because the plugin auto-deletes expired key rows (orphaning the service
user), `ListApiKeys` runs an orphan sweep: delete `@service.local` users
with no surviving `apikey` row. The Members UI filters `@service.local`
accounts out of rows and counts.

### C. Management: native admin-gated `ApiKeyService`; plugin endpoints killed

The plugin's own HTTP endpoints (`/api/auth/api-key/*`) have **no role
gate** — any logged-in user could mint themselves a key. A global
better-auth `hooks.before` 404s every HTTP request to `/api-key/*`
(`ctx.request` is set for HTTP only — the same discriminator the plugin
itself uses to block `body.userId` from HTTP callers — so server-side
`auth.api.createApiKey` still works).

Management flows exclusively through the native Connect `ApiKeyService`
(`orchestrator/src/rpc/api-key.ts`, mirroring `org-secret.ts`: injectable
deps, `requireAdmin` on every RPC, registered before the passthrough):

- `CreateApiKey(name, role, expires_at?)` → meta + **plaintext key exactly
  once**. Validates `role ∈ {admin, user}`; creates the service user; calls
  `auth.api.createApiKey({body: {name, userId, expiresIn?}})` (the
  sanctioned server-side arbitrary-userId path); deletes the service user on
  plugin failure.
- `ListApiKeys` → drizzle `apikey ⋈ user` (the plugin's list API is
  owner-scoped and can't serve an admin cross-key view), then the orphan
  sweep.
- `RevokeApiKey(id)` → deletes the **service user** (cascade kills the key
  row); refuses if the referenced user is not `@service.local` (defense
  against a hand-crafted row pointing at a human); idempotent.

### D. IAP bridge: routing bypass, fail-closed

With `IAP_AUDIENCES` set, `iapBridge()` 401s any request lacking an IAP
assertion or session cookie — before a keyed request could ever reach
`getSession`. The bridge now checks `extractApiKey(headers)` (the same
helper the plugin uses, so detection can never skew) after the
PUBLIC_PATHS/inert checks and **before** `verifyExistingSession`: key
present → `next()`. This grants nothing — a junk key sails past the bridge
and fails closed at every seam (invalid key → throw → wrapper → null →
Unauthenticated/401). The exposure delta is exactly the non-IAP
deployment's baseline: unauthenticated requests reach routing instead of
401ing at the wall.

### E. Centralized session resolution

The plugin **throws** an APIError from `getSession` on an invalid, expired,
or disabled key (instead of returning null), which would surface as
500/`Code.Unknown` at every seam. New `orchestrator/src/auth/session.ts`
exports `getSessionFromHeaders(headers)` — `auth.api.getSession` in
try/catch → null — and every inline default-getSession call site
(passthrough, guard, native services, Hono routes) adopts it. Mechanical
clean-break refactor; injectable test seams unchanged.

## Alternatives rejected

- **Hand-rolled key table + verify helper**: re-implements hashing,
  expiration, and lookup the plugin already ships, and still needs the same
  seam integration. More code, no additional control we need.
- **Shared service user per role** (one `api-user`, one `api-admin`): no
  per-key attribution; every key of a role co-owns every session any of them
  created; revoking one key can't garbage-collect anything.
- **Metadata-carried role + custom ability mapping**: role would no longer
  come from `user.role`, forking `abilityFor` and every seam — a second
  authorization path, exactly what the invariant forbids.
- **Exposing the plugin's own client endpoints**: owner-scoped semantics
  (self-service keys), no admin cross-key view, and no role gate on
  creation. Wrong shape on all three axes.
- **JWT/OIDC client-credentials flow**: heavier issuance/rotation machinery
  and a second token format, for zero gain over an opaque hashed key at this
  scale.

## Phases

1. **P1 (orchestrator)**: ADR (Proposed) · `@better-auth/api-key` dep ·
   `apikey` table + migration 0015 · `extractApiKey` +
   `getSessionFromHeaders` + seam adoption · plugin config + endpoint
   lockdown · IAP bypass · `api_key.proto` + codegen · `ApiKeyService` ·
   `api-key.test.ts` (RBAC matrix, e2e key auth incl. Bearer form,
   invalid/expired → Unauthenticated, revoke cascade + idempotency + human
   guard, lockdown 404, orphan sweep) · `iap-bridge.test.ts` extension.
   Tests pin: authz matrix unchanged (the wrapper refactor must not shift
   error codes).
2. **P2 (web)**: `useApiKeys` hooks · `/settings/api-keys` admin panel
   (create dialog with role select + optional native date input; one-time
   key reveal; masked list; revoke confirm) · router + ORG nav · Members
   `@service.local` filter · vitest coverage. Flips this ADR to Accepted.

## Commit chain

- P1 (orchestrator): #631 `ac083f6a`
- P2 (web): #632 `6fad3988` — flips this ADR to Accepted

### Phase divergences (implementation)

- **P1**: verification against the installed dist confirmed every planned
  plugin behavior, plus one nuance the plan missed: the plugin treats a call
  as a *client* request when `ctx.request` **or `ctx.headers`** is present —
  the server-side `createApiKey({body: {userId}})` call must pass no headers
  or it is rejected. `lastRequest` IS stamped with rate limiting disabled
  (the "Last used" column works). Masked preview length raised to 11
  (`startingCharactersConfig`) — the default 6 was swallowed by the 5-char
  `engk_` prefix.
- **P2**: the create form needs `noValidate` — the date input's `min`
  (picker affordance) otherwise triggers NATIVE constraint validation, which
  silently blocks submit on a past date before react-hook-form runs; zod
  owns validation so the error is styled and testable.

## Rollout

- Migration 0015 applies at orchestrator boot (programmatic migrator); no
  backfill — the table starts empty.
- No coordinator/host changes; the coordinator continues to see only the
  static machine bearer.
- **Prod ingress caveat**: behind GCP IAP at the load balancer, keyed
  requests are blocked before reaching the orchestrator. Exposing a
  programmatic path (LB route exempt from IAP, or IAP service-account
  audiences) is an engrams-internal (deploy repo) decision; this ADR only
  guarantees the orchestrator-side seam is correct when a keyed request
  arrives.
- Key rotation = revoke + create (explicitly out of scope).

## Risks

- **Lockstep version coupling**: `@better-auth/api-key` must match
  `better-auth` minor-for-minor; both are pinned and called out here.
- **`lastRequest` freshness with rate limiting off**: if the plugin only
  stamps usage on the rate-limit path, "Last used" shows "Never" — cosmetic;
  fallback is enabling rate limiting with generous limits.
- **Mock-session hook coverage**: verified against the v1.6.16 source that
  the hook fires on server-side `getSession`; the e2e test is the hard
  proof. Contained fallback: call `auth.api.verifyApiKey` inside
  `getSessionFromHeaders` and synthesize the session shape (one file, no
  seam changes).
- **Catch-all in `getSessionFromHeaders`** converts any better-auth internal
  error into "anonymous" at the seams — matching the guard/bridge behavior
  today, but a genuine outage reads as mass 401s rather than 500s.
