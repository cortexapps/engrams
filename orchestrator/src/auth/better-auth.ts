/**
 * better-auth configuration — ADR 0051 §5 / Task 16.
 *
 * Auth stack:
 *   - ONE human sign-in door, chosen by config.authMode (ORCHESTRATOR_AUTH_MODE):
 *       `oauth`    — genericOAuth against the deployment's OIDC provider, behind
 *                    the allowlist gate (sign-in-door.ts / oauth-gate.ts).
 *       `iap`      — the IAP bridge (iap-bridge.ts) mints sessions; nothing in
 *                    this file opens a door.
 *       `password` — emailAndPassword. Sign-up is open registration unless
 *                    config.passwordSignup is false. The dev default.
 *     The doors are exclusive: emailAndPassword is OFF outside `password` mode,
 *     and the OAuth plugin is absent outside `oauth` mode.
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
import { APIError, createAuthMiddleware, getSessionFromCtx } from "better-auth/api";
import { admin } from "better-auth/plugins/admin";
import { bearer } from "better-auth/plugins/bearer";
import { deviceAuthorization } from "better-auth/plugins/device-authorization";
import { apiKey } from "@better-auth/api-key";
import { getDb } from "../db/client.ts";
import { config } from "../config.ts";
import {
  ADMIN_ROLE,
  isBootstrapAdmin,
  promotedRoleOnLogin,
} from "./admin-allowlist.ts";
import { API_KEY_PREFIX, extractApiKey } from "./api-key-header.ts";
import { signInDoor } from "./sign-in-door.ts";

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
//     role 'admin' directly (covers IAP bridge createUser, OAuth callback, and
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

/** The SPA login page. A refused OAuth sign-in lands here with `?error=`. */
const LOGIN_URL = `${config.baseUrl.replace(/\/$/, "")}/login`;

// The ONE sign-in door (config.authMode): the password options, plus the OAuth
// plugin in `oauth` mode. See sign-in-door.ts.
const door = signInDoor(config, LOGIN_URL);

export const auth = betterAuth({
  // Public base URL (ORCHESTRATOR_PUBLIC_URL); dev defaults to loopback. Drives
  // the cookie domain + the OAuth redirect callback, so it MUST be the
  // externally-reachable URL in an `oauth` deploy.
  baseURL: config.baseUrl,
  // The browser reaches this through the vite proxy with
  // Origin: http://localhost:5173 — without trustedOrigins, better-auth
  // 403s every non-GET auth route (CSRF protection).
  //
  // ADR 0118: session-app origins are deliberately NOT here. A guest page is
  // attacker-authored (any org member controls their own app), and because the
  // session cookie is now scoped to the shared parent domain, a request from a
  // guest page to the main host is SAME-SITE — so the visitor's cookie rides
  // along whatever SameSite says, and this CSRF origin check is the only
  // remaining guard on state-changing auth routes. Trusting `*.<preview>` would
  // waive it for exactly the origins that should never have it.
  //
  // Nothing needs it: the preview edge authenticates server-side from the
  // Cookie header (routes/preview-proxy.ts), never through a browser call to
  // the auth API.
  trustedOrigins: config.trustedOrigins,
  // ADR 0118: scope the session cookie to the domain the main host and the
  // preview base domain share, so ONE login covers the main host and every app
  // URL and a sibling fetch carries the cookie with no redirect. Omitted
  // entirely when unset, which leaves the cookie host-only — the behaviour
  // every deployment had before session apps.
  //
  // ORCHESTRATOR_COOKIE_PREFIX renames the auth cookies. It matters only for an
  // engrams running as a session app of another engrams: the outer preview edge
  // strips `better-auth`-named cookies in both directions (it cannot tell the
  // visitor's outer token from an identically-named nested one), so a nested
  // stack that keeps the default name never sees its own session and bounces
  // every login back to the form. See routes/preview-proxy.ts.
  ...(config.sessionCookieDomain || config.cookiePrefix
    ? {
        advanced: {
          ...(config.sessionCookieDomain
            ? {
                crossSubDomainCookies: {
                  enabled: true,
                  domain: config.sessionCookieDomain,
                },
              }
            : {}),
          ...(config.cookiePrefix ? { cookiePrefix: config.cookiePrefix } : {}),
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
  // The password door: on only in `password` mode (sign-in-door.ts). The web
  // Login page reads the same posture from /api/v1/auth-config, so it never
  // renders a form the server rejects.
  emailAndPassword: door.emailAndPassword,
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
    //
    // GET /device (the device flow's verify leg) needs a session. The plugin
    // itself answers it for anyone: an anonymous caller that guesses a user
    // code learns that a login is pending. Only the SPA's /device page calls
    // it, and that page is behind the login wall, so the anonymous case has no
    // use. In `iap` mode the bridge's fail-closed 401 used to be the only
    // thing in front of this endpoint; the check is here so it holds in every
    // mode. The two CLI legs (/device/code, /device/token) are different
    // paths and stay anonymous — that anonymity is the point of RFC 8628.
    before: createAuthMiddleware(async (ctx) => {
      if (!ctx.request) return;
      if (ctx.path.startsWith("/api-key")) {
        throw new APIError("NOT_FOUND");
      }
      if (ctx.path === "/device" && !(await getSessionFromCtx(ctx))) {
        throw new APIError("UNAUTHORIZED");
      }
    }),
  },
  plugins: [
    admin(), // role field ('admin'|'user'), setRole/ban/list APIs → Members UI
    ...door.plugins, // the `oauth` door (genericOAuth), present only in that mode
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
