/**
 * better-auth configuration — ADR 0039 §5 / Task 16.
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
 *     database via the lazy getDb() singleton. better-auth needs the
 *     resolved drizzle instance, so we call getDb() at module evaluation
 *     time; that is safe because the adapter only captures a reference here
 *     and opens no connection until the first auth request.
 *   - secret: BETTER_AUTH_SECRET env var. better-auth falls back to a hard-
 *     coded dev literal when the var is absent (it logs a warning in dev and
 *     throws in production). We pass it explicitly from config so the value
 *     is consistent with what we inject via the Tiltfile.
 *   - trustedOrigins: wired from TRUSTED_ORIGINS (config.trustedOrigins) so
 *     the vite dev server at http://localhost:5173 passes the built-in CSRF
 *     check. Without this, better-auth 403s every non-GET auth route.
 */

import { betterAuth } from "better-auth";
import { drizzleAdapter } from "better-auth/adapters/drizzle";
import { admin } from "better-auth/plugins/admin";
import { getDb } from "../db/client.ts";
import { config } from "../config.ts";

export const auth = betterAuth({
  baseURL: `http://127.0.0.1:${config.port}`,
  // The browser reaches this through the vite proxy with
  // Origin: http://localhost:5173 — without trustedOrigins, better-auth
  // 403s every non-GET auth route (CSRF protection).
  trustedOrigins: config.trustedOrigins,
  database: drizzleAdapter(getDb(), { provider: "pg" }),
  // Dev/self-hosted door. NOTE: public sign-up = open registration.
  // Acceptable in dev only; production posture (disable sign-up /
  // allowlist) is decided in Task 22 — do not deploy past Phase 4
  // without it.
  emailAndPassword: { enabled: true },
  // BETTER_AUTH_SECRET: better-auth uses this for cookie signing, encryption,
  // and hashing. Falls back to "better-auth-secret-123456789" in dev (with a
  // console warning). In production it throws if unset. We wire it from config
  // so Tilt can inject a known dev literal without requiring a manual .env step.
  secret: config.betterAuthSecret,
  plugins: [
    admin(), // role field ('admin'|'user'), setRole/ban/list APIs → Members UI
  ],
});
