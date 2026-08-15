/**
 * better-auth configuration — ADR 0051 §5 / Task 16.
 *
 * Auth stack:
 *   - emailAndPassword: the single dev-accessible sign-up/sign-in door.
 *     Public sign-up = open registration. This is intentional in dev only;
 *     production posture (disable sign-up / allowlist) is decided in Task 22.
 *     Do NOT deploy past Phase 4 without it.
 *   - admin plugin: owns the `role` field ('admin'|'user'), plus setRole /
 *     ban / list APIs used by the Members UI (Task 25). We read 'user' as
 *     "member". There is no JWT plugin and no JWKS — nothing downstream
 *     consumes user identity anymore (ADR §5).
 *   - api-key plugin (ADR 0086): global programmatic keys, each owned by a
 *     dedicated service-account user carrying the key's role. Keyed requests
 *     resolve to a mock session at getSession; the plugin's own HTTP
 *     endpoints are 404'd (hooks.before) — management is the admin-gated
 *     ApiKeyService only. CLI keys (user-owned, self-minted at
 *     `engrams auth login`) share the table + seams — see rpc/api-key.ts.
 *   - device-authorization + bearer plugins: the `engrams auth login` rail.
 *     Device flow (RFC 8628) hands the CLI a short-lived session token; the
 *     bearer plugin lets that token authenticate the ONE CreateCliKey
 *     exchange that mints the durable user-owned API key.
 *   - drizzle adapter: writes to the same engram_orchestrator postgres
 *     database via the lazy getDb() singleton. We pass a Proxy that defers
 *     the getDb() call until the first property access so this module can
 *     be imported in test mode without ORCHESTRATOR_DATABASE_URL set. Any
 *     actual DB operation (sign-up, get-session, etc.) requires the real URL.
 *   - secret: BETTER_AUTH_SECRET env var. This is now REQUIRED — better-auth
 *     1.6.16 has no silent dev fallback (it uses a publicly known constant with
 *     zero warning, making it exploitable). config.ts enforces the requirement;
 *     the Tiltfile injects a deterministic dev literal for local dev; prod must
 *     rotate via .env or a secrets manager before Phase 4 deployment.
 *   - trustedOrigins: wired from TRUSTED_ORIGINS (config.trustedOrigins) so
 *     the vite dev server at http://localhost:5173 passes the built-in CSRF
 *     check. Without this, better-auth 403s every non-GET auth route.
 */

import { betterAuth, type BetterAuthOptions } from "better-auth";
import { drizzleAdapter } from "better-auth/adapters/drizzle";
import { APIError, createAuthMiddleware } from "better-auth/api";
import { admin } from "better-auth/plugins/admin";
import { bearer } from "better-auth/plugins/bearer";
import { deviceAuthorization } from "better-auth/plugins/device-authorization";
import { genericOAuth } from "better-auth/plugins/generic-oauth";
import { apiKey } from "@better-auth/api-key";
import { getDb } from "../db/client.ts";
import { config } from "../config.ts";
import {
  ADMIN_ROLE,
  isBootstrapAdmin,
  promotedRoleOnLogin,
} from "./admin-allowlist.ts";
import { API_KEY_PREFIX, extractApiKey } from "./api-key-header.ts";

/** The one OAuth client_id the device-authorization flow accepts (the
 *  `engrams` CLI). Exported for tests; the CLI hardcodes the same string. */
export const DEVICE_CLIENT_ID = "engrams-cli";

// Bootstrap-admin allowlist (restores the old `auth.bootstrapAdmins` Helm
// value via ORCHESTRATOR_ADMIN_EMAILS). When empty, both hooks below are
// skipped entirely so there is zero per-request/per-create overhead in the
// common (no bootstrap admins) case.
const adminEmails = config.adminEmails;

// `databaseHooks` promote allowlisted emails to admin. Defined only when the
// allowlist is non-empty:
//   - user.create.before: a NEW user with an allowlisted email is created with
//     role 'admin' directly (covers IAP bridge createUser, OIDC callback, and
//     email/password sign-up — every JIT path runs through this hook).
//   - session.create.after: an EXISTING user signing in is promoted to admin if
//     allowlisted and not already admin. This means adding someone to the
//     allowlist AFTER their first login still grants admin on next sign-in,
//     matching the pre-ADR-0051 coordinator behaviour.
const databaseHooks: BetterAuthOptions["databaseHooks"] | undefined =
  adminEmails.length === 0
    ? undefined
    : {
        user: {
          create: {
            // Hook signature requires a Promise return; `async` satisfies it.
            before(user) {
              if (isBootstrapAdmin(user.email, adminEmails)) {
                return Promise.resolve({ data: { ...user, role: ADMIN_ROLE } });
              }
              // No change → better-auth uses the original data.
              return Promise.resolve(undefined);
            },
          },
        },
        session: {
          create: {
            async after(session) {
              const userId = session.userId;
              if (!userId) return;
              const ctx = await auth.$context;
              const existing = await ctx.internalAdapter.findUserById(userId);
              if (!existing) return;
              const next = promotedRoleOnLogin(
                existing.email,
                (existing as { role?: string | null }).role,
                adminEmails,
              );
              if (next) {
                await ctx.internalAdapter.updateUser(userId, { role: next });
              }
            },
          },
        },
      };

// Env-driven OIDC ("Sign in with your IdP"), restoring the old coordinator
// `--auth-mode=oidc` parity. Added only when config.oidc is present (issuer +
// client id/secret all set). Federated SSO also yields real display names via
// the `profile` scope's ID-token claims — something stock GCP IAP can't supply.
const oidcPlugins = config.oidc
  ? [
      genericOAuth({
        config: [
          {
            providerId: config.oidc.providerId,
            clientId: config.oidc.clientId,
            clientSecret: config.oidc.clientSecret,
            discoveryUrl: `${config.oidc.issuer}/.well-known/openid-configuration`,
            scopes: config.oidc.scopes,
          },
        ],
      }),
    ]
  : [];

/**
 * ADR 0118: the configured trusted origins, plus the preview wildcard when a
 * preview base domain is set.
 *
 * A session app's page is a legitimate origin for an auth call (it is inside
 * the same login wall), but it is one of an unbounded set of hostnames minted
 * per session, so it cannot be enumerated in TRUSTED_ORIGINS. The wildcard
 * covers exactly one label under the base domain — the same shape the preview
 * handler routes — and nothing else.
 */
function previewTrustedOrigins(): string[] {
  const base = config.previewBaseDomain;
  if (!base) return config.trustedOrigins;
  const scheme = /(localhost|127\.0\.0\.1|lvh\.me|localtest\.me)/.test(base)
    ? "http"
    : "https";
  return [...config.trustedOrigins, `${scheme}://*.${base}`];
}

export const auth = betterAuth({
  // Public base URL (ORCHESTRATOR_PUBLIC_URL); dev defaults to loopback. Drives
  // the cookie domain + the OIDC redirect callback, so it MUST be the
  // externally-reachable URL in an OIDC prod deploy.
  baseURL: config.baseUrl,
  // The browser reaches this through the vite proxy with
  // Origin: http://localhost:5173 — without trustedOrigins, better-auth
  // 403s every non-GET auth route (CSRF protection). ADR 0118 adds the preview
  // wildcard: a session app's page is a legitimate origin for an auth call, and
  // without it every such call would be CSRF-rejected.
  trustedOrigins: previewTrustedOrigins(),
  // ADR 0118: scope the session cookie to the domain the main host and the
  // preview base domain share, so ONE login covers the main host and every app
  // URL and a sibling fetch carries the cookie with no redirect. Omitted
  // entirely when unset, which leaves the cookie host-only — the behaviour
  // every deployment had before session apps.
  ...(config.sessionCookieDomain
    ? {
        advanced: {
          crossSubDomainCookies: {
            enabled: true,
            domain: config.sessionCookieDomain,
          },
        },
      }
    : {}),
  // Lazy Proxy: defers getDb() until better-auth first accesses the db
  // object (i.e., on the first actual auth request). This lets the module
  // be imported in `bun test` without ORCHESTRATOR_DATABASE_URL set — the
  // non-gated tests never touch the DB path, so the proxy is never resolved.
  // In prod/dev the real URL is always present and the proxy is transparent.
  database: drizzleAdapter(
    new Proxy({} as ReturnType<typeof getDb>, {
      get(_target, prop) {
        return Reflect.get(getDb(), prop);
      },
    }),
    { provider: "pg" },
  ),
  // Email/password is the dev/self-hosted door. It is DISABLED whenever the
  // orchestrator runs behind GCP IAP (IAP_AUDIENCES set): IAP is then the sole
  // identity source and the bridge mints sessions from the verified assertion,
  // so a parallel password door (open registration + a credential to phish or
  // brute-force) is pure attack surface. When IAP is off (dev / self-hosted)
  // this stays enabled, and public sign-up = open registration — acceptable in
  // dev only. The web Login page reads the same posture via /api/v1/auth-config
  // so it doesn't render a password form that the server would reject.
  emailAndPassword: { enabled: config.iapAudiences.length === 0 },
  // Encrypt better-auth's stored OAuth access/refresh/id tokens at rest
  // (the `account` table columns). NOTE: this uses the BETTER_AUTH_SECRET
  // (the framework's own encryption mechanism) — NOT the shared engrams KEK.
  // Our user_session_secrets (the Claude harness token) use the engrams KEK
  // via src/crypto/seal.ts; these OAuth tokens are framework-owned and ride
  // the better-auth secret. Both are now sealed at rest.
  account: { encryptOAuthTokens: true },
  // BETTER_AUTH_SECRET: required — config.ts enforces it (test-mode escape
  // injects a placeholder; prod/dev must set the real var). The Tiltfile
  // injects a deterministic dev literal; prod must rotate before Phase 4.
  secret: config.betterAuthSecret,
  // Bootstrap-admin promotion (ORCHESTRATOR_ADMIN_EMAILS). Undefined → omitted
  // entirely when the allowlist is empty (inert dev/local default).
  ...(databaseHooks ? { databaseHooks } : {}),
  hooks: {
    // ADR 0086: the api-key plugin has NO role gate on its HTTP endpoints —
    // any logged-in user could mint themselves a key via
    // POST /api/auth/api-key/create. Kill ALL HTTP access to the plugin's
    // routes; key management flows only through the admin-gated ApiKeyService
    // (src/rpc/api-key.ts). `ctx.request` is set for HTTP requests only, so
    // server-side auth.api.createApiKey (no request) still works — the same
    // discriminator the plugin itself uses to gate body.userId.
    before: createAuthMiddleware(async (ctx) => {
      if (ctx.request && ctx.path.startsWith("/api-key")) {
        throw new APIError("NOT_FOUND");
      }
    }),
  },
  plugins: [
    admin(), // role field ('admin'|'user'), setRole/ban/list APIs → Members UI
    ...oidcPlugins, // env-driven OIDC provider (genericOAuth) when configured
    // The `engrams auth login` rail (RFC 8628 device flow): the CLI POSTs
    // /api/auth/device/code, the user approves on the SPA's /device page
    // (cookie-authed), and the CLI's /api/auth/device/token poll returns a
    // short-lived session token it immediately exchanges for a durable
    // user-owned key via ApiKeyService.CreateCliKey. The IAP bridge exempts
    // exactly the two CLI-facing endpoints (code/token); approve/deny ride
    // the browser session like any auth route.
    deviceAuthorization({
      expiresIn: "10m", // the login window; RFC-typical, well under the 30m default
      interval: "5s",
      // The SPA page, not the plugin's JSON GET /api/auth/device endpoint —
      // this string is what the CLI shows and opens in the browser.
      verificationUri: config.deviceVerificationUrl,
      // One known client. Anything else is a confused or hostile caller.
      validateClient: (clientId) => clientId === DEVICE_CLIENT_ID,
      // The plugin's options zod (v4 z.custom) rejects an ABSENT schema key
      // ("expected nonoptional"); an empty object means "stock model/field
      // names" — which is exactly what db/schema.ts's deviceCode table mirrors.
      schema: {},
    }),
    // Resolves `Authorization: Bearer <session token>` into a session — the
    // ONE hop of the login exchange where the CLI holds a device-flow session
    // token but no cookie jar and no API key yet. Tokens are HMAC-verified
    // against the better-auth secret (an unsigned token is signed server-side
    // before verification), so a junk bearer stays anonymous. engk_ bearers
    // are consumed by the apiKey plugin instead (prefix-disjoint by design —
    // see extractApiKey).
    bearer(),
    // ADR 0086: global API keys. Each key's referenceId points at a dedicated
    // service-account user (apikey+<uuid>@service.local) whose `role` is the
    // key's authorization level; enableSessionForAPIKeys turns a valid keyed
    // request into a mock session for that user at getSession, so every
    // existing seam (CASL, policy map, ownership) works unchanged. Invalid /
    // expired / disabled keys THROW from getSession — src/auth/session.ts
    // catches to null so the seams fail closed with their normal 401s.
    apiKey({
      defaultPrefix: API_KEY_PREFIX,
      // x-api-key or `Authorization: Bearer engk_…` — shared with the IAP
      // bridge bypass so key detection can never skew between the two.
      customAPIKeyGetter: (ctx) => (ctx.headers ? extractApiKey(ctx.headers) : null),
      enableSessionForAPIKeys: true,
      // Masked preview stored as `start`: default 6 chars would be swallowed
      // by the 5-char prefix — 11 shows engk_ + 6 chars of entropy in the UI.
      startingCharactersConfig: { shouldStore: true, charactersLength: 11 },
      // Per-key rate limiting is ON by default (~10 req/day) — that would
      // break programmatic callers, and the orchestrator has no per-key QoS
      // requirement. (lastRequest is still stamped for the "Last used" UI.)
      rateLimit: { enabled: false },
      // Days. Floor is the plugin's own minimum (1 day); revoke covers
      // "kill it now". 3650 ≈ effectively non-expiring for named CI keys.
      keyExpiration: { minExpiresIn: 1, maxExpiresIn: 3650 },
    }),
  ],
});
