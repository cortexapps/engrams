# ADR 0031: User authentication — SSO + per-user identity (SCIM-ready)

Status: 2026-06-02 — **Proposed.** Authored before code per the ADR-bookend norm.
Branches off `worktree-adr-0030-session-conversation-redesign` (ADR 0030 is in flight);
ships as one big-bang PR with screenshots.

## Context

engrams has no user identity. The coordinator authenticates every request against a
single deployment-wide bearer allowlist (`ENGRAM_AUTH_TOKENS`, checked in
`crates/engram-coordinator/src/api/auth.rs`); an empty list disables auth entirely so
`just dev` runs frictionlessly. The web app makes bare `fetch` calls with no
`Authorization` header, the top-right `UserChip` renders a `§` placeholder, and
`ProfilePanel` literally says "no identity yet." `sessions.user_id` exists as a nullable
free-form `TEXT` column but is never used for authorization. The Claude Code OAuth token
is pasted **per session** into `NewSessionForm` and rides the `secrets` dict; git commits
made inside a session carry no author identity.

This means: no roles (everyone who holds the bearer token sees everything), users
re-paste their Claude token on every session, the profile menu is inert, and commits
authored by a session are unattributable to the human who launched it.

We want real per-user identity — and it must be **identity-provider agnostic** (Cortex
uses Rippling, but nothing IdP-specific may be hardcoded) and **must not cripple local
dev** (`just dev` keeps working with zero auth configuration).

## Decision

### 1. Pluggable authentication behind a trait

Authentication is a new provider seam, expressed the same way as `MetadataStore` /
`SecretStore` / `CloudBackend` / `MasterKeyProvider`: a trait with one implementation per
mechanism, held as `Arc<dyn …>` and selected by config — **not** a `match` over modes.

A new framework-free crate `engram-auth` houses an `IdentityVerifier` trait and a
`VerifierChain`. Every implementation reduces a request to a `VerifiedEmail`, which is
JIT-upserted into a `Principal { user_id, email, role, active }` consumed uniformly by
handlers and extractors:

| Implementation | Used by | How it verifies |
|----------------|---------|-----------------|
| `OidcAuthenticator` | OSS deployments, no proxy | Authorization-Code + PKCE via the `openidconnect` crate; discovery (`.well-known`) + JWKS; ID-token nonce |
| `ForwardAuthVerifier` | behind an edge proxy (GCP IAP, Cloudflare Access, oauth2-proxy) | verifies a signed JWT forwarded by the proxy against a configured `(header, JWKS URL, issuer, audience, email claim)` |
| `ServiceBearer` | host-agent, CLI, machine callers | the **existing** `ENGRAM_AUTH_TOKENS` allowlist → admin-equivalent service principal |
| `SyntheticAdmin` | dev (no SSO configured) | returns one built-in admin principal for every request |

Per-request resolution order: **cookie session → ServiceBearer → ForwardAuth (if
configured) → SyntheticAdmin (only when no SSO is configured)**. First match wins. Adding
a new edge authenticator is a new `IdentityVerifier` impl, not a branch.

This is what makes engrams IdP-agnostic: the OSS core speaks standard OIDC against any
issuer (Rippling is just `issuer + client_id + client_secret`), and **GCP IAP is a
documented config preset** of the five `ForwardAuth` fields — not a Google code path — so
the same binary runs behind Cloudflare Access or oauth2-proxy unchanged.

### 2. Authentication ≠ provisioning; they reconcile on email

A deployment can authenticate via one system and provision via another (Cortex: login via
Google/IAP, directory via Rippling). The two axes join on the **email address**:

- **Authentication** (`IdentityVerifier`) proves *who is making this request*.
- **Provisioning** populates *who exists, their role, and their active state*.

This PR ships **JIT provisioning**: on first successful login the user row is upserted
from the verified email (role from a config bootstrap-admin allowlist, else `member`).
A future **SCIM 2.0 server** (RFC 7643/7644) is deferred but the schema is designed for
it now — `role_source` (`manual | scim | claim`), an `active` flag, and a `groups` notion.
When SCIM lands it becomes **authoritative with manual override**: a `manual` role_source
row is never clobbered by SCIM group sync. SCIM also brings deprovisioning that
authentication alone cannot — an inactive row is rejected even if the edge proxy still
authenticates the person.

### 3. Human sessions vs. machine tokens

Humans get an **opaque session token in an `HttpOnly; Secure; SameSite=Lax` cookie**,
backed by a `web_sessions` Postgres table (token hashed at rest). DB-backed (not a
browser-held JWT) so deprovision/logout = delete the row, with no JWT-expiry replay
window. The existing deployment bearer token stays for host-agent / CLI / internal calls;
coord auth resolves *either* a valid cookie *or* a service bearer.

### 4. Roles

Two roles: `admin` and `member`. Admins see Fleet / Storage / admin-Settings and the
operator endpoints (host drain, registries, enabled-images, user admin); members see
Sessions and their own profile. Enforcement is a coord `AdminOnly` extractor (the real
gate); the web only hides tabs (UX). Now: roles come from a config bootstrap-admin email
allowlist + manual promotion via `PATCH /admin/users/:id`. Later: SCIM groups.

### 5. Dev runs with a synthetic admin

When no SSO is configured (`AuthMode::None`), the verifier chain ends in `SyntheticAdmin`,
which returns one built-in admin principal for every request. `just dev` needs zero auth
setup, there is no login wall, and the dashboard renders the *real* authenticated
experience (profile menu, all tabs, token save) while exercising the actual authed code
paths. The dev committer email defaults to a configurable address.

### 6. Per-user Claude OAuth token — never prompt again

A new KEK-sealed `user_tokens` store (reusing `engram-crypto`'s `CredCipher` +
`MasterKeyProvider`, identical envelope shape to `session_secrets`). The user saves their
Claude Code OAuth token once on their profile. At session create, **only when the resolved
harness is `builtin = "claude"`**, the coordinator auto-injects the user's
`CLAUDE_CODE_OAUTH_TOKEN` into `session_env`. We never prompt per session. If the user has
no token saved and the image uses built-in Claude, the web "Create session" button routes
them to the token-save screen instead of creating. Custom harnesses and images that
declare their own `[secrets]` are untouched.

### 7. Git committer attribution

`ENGRAM_USER_EMAIL` / `ENGRAM_USER_NAME` (from the initiating user's profile) flow through
`session_env` into `render_gitconfig` (`engram-session-bundles`), which writes a `[user]`
block to `/etc/gitconfig`. The gitconfig write is decoupled from the forge-token gate so
attribution applies to every session, not just forge-enabled ones. The push credential
(GitHub App installation token) is independent of commit authorship; GitHub links the
commit to the human iff their email is verified on their GitHub account, degrading
gracefully otherwise.

### 8. Web IA: user settings split out from global

Today's "Settings" (images, registries) is really *admin/global* config. We split it:
**user settings** (profile + "My Claude Code OAuth token") reachable by everyone from the
profile menu, and **admin settings** (images, registries) admin-only. The `UserChip`
becomes a real profile menu (initial/avatar, email, working sign-out); its "Settings" link
targets *user* settings. `NavSpine` hides Fleet / Storage / admin-Settings for members.

## Rationale

- **Direct OIDC over a managed broker (WorkOS) or SAML.** OIDC via discovery is agnostic
  by construction, needs no extra infra (good OSS default), and a broker can still front
  it later with zero code change. SAML's XML/signature footguns and weaker Rust libraries
  aren't justified when Rippling speaks OIDC.
- **Trait seam over mode-switch.** Matches every other pluggable provider in the codebase;
  a new authenticator is an impl, and `ForwardAuth`-as-config keeps IAP from leaking a
  Google-shaped branch into the core.
- **JIT now, SCIM later.** SSO + login-time provisioning is the value most users want
  first; a compliant SCIM server is real work and can land authoritative-with-override on
  the schema we seed now.
- **DB-backed opaque cookie over browser JWT.** Clean, instant revocation — the property
  deprovisioning needs.

## Implications

- New crate `engram-auth` (+ workspace/hakari wiring); new `engram-core` `user` types and
  `UserStore` / `WebSessionStore` traits; Postgres impls; migrations `0046`–`0048`
  (free atop the 0030 base, which adds nothing past `0045`).
- Coordinator gains the verifier chain, a `resolve_principal` layer, `Principal` /
  `AdminOnly` extractors, an `api/mod.rs` split into a human-cookie group and an
  internal-bearer group (host routes must keep the bearer), and `/auth/*`, `/me`,
  `/me/claude-token`, `/admin/users` endpoints. `CreateSessionRequest` drops the
  client-supplied `user_id` (now server-stamped from the principal).
- `EnabledImageSummary` gains a `harness_builtin` field so the web can gate create on the
  built-in-Claude case reliably.
- Risk: forgetting to keep host/internal routes on the bearer path would break the
  control plane; the router split is the mitigation. Cookie `Secure` must be config-gated
  off for http-localhost dev. OIDC state/PKCE/nonce must be validated to prevent CSRF.
- Deferred: SCIM 2.0 server; per-image `[secrets]` rendering for custom harnesses in the
  create form (needs the secrets schema on `EnabledImageSummary`).

This status section is updated between phases with divergences/pitfalls and flipped to
**Accepted** with the commit chain when the work lands.
