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

import { betterAuth } from "better-auth";
import { drizzleAdapter } from "better-auth/adapters/drizzle";
import { admin } from "better-auth/plugins/admin";
import { genericOAuth } from "better-auth/plugins/generic-oauth";
import { getDb } from "../db/client.ts";
import { config } from "../config.ts";

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

export const auth = betterAuth({
  // Public base URL (ORCHESTRATOR_PUBLIC_URL); dev defaults to loopback. Drives
  // the cookie domain + the OIDC redirect callback, so it MUST be the
  // externally-reachable URL in an OIDC prod deploy.
  baseURL: config.baseUrl,
  // The browser reaches this through the vite proxy with
  // Origin: http://localhost:5173 — without trustedOrigins, better-auth
  // 403s every non-GET auth route (CSRF protection).
  trustedOrigins: config.trustedOrigins,
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
  // Dev/self-hosted door. NOTE: public sign-up = open registration.
  // Acceptable in dev only; production posture (disable sign-up /
  // allowlist) is decided in Task 22 — do not deploy past Phase 4
  // without it.
  emailAndPassword: { enabled: true },
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
  plugins: [
    admin(), // role field ('admin'|'user'), setRole/ban/list APIs → Members UI
    ...oidcPlugins, // env-driven OIDC provider (genericOAuth) when configured
  ],
});
