/**
 * Orchestrator runtime configuration.
 *
 * All values are read from environment variables at startup. `loadConfig`
 * throws with a clear message listing every missing required variable so
 * the operator knows exactly what to set before the process can boot.
 */

import { parseAdminEmails } from "./auth/admin-allowlist.ts";

export interface Config {
  /** ORCHESTRATOR_PORT — default 8787 */
  port: number;
  /**
   * ORCHESTRATOR_PUBLIC_URL — externally-reachable base URL, used as
   * better-auth's `baseURL` (session-cookie domain + the OIDC redirect
   * callback `${baseUrl}/api/auth/oauth2/callback/<provider>`). Default:
   * http://127.0.0.1:${port} (dev). MUST be set to the public https URL in
   * any OIDC prod deploy, else the IdP redirect lands on an unreachable host.
   */
  baseUrl: string;
  /** ORCHESTRATOR_DATABASE_URL — required (Task 15) */
  databaseUrl: string;
  /** CONTROL_PLANE_GRPC_URL — default http://127.0.0.1:50061 */
  controlPlaneGrpcUrl: string;
  /** CONTROL_PLANE_HTTP_URL — coordinator REST base URL. Used by admin REST
   * proxy routes (pause/resume session) that have no gRPC equivalent yet.
   * Default: http://127.0.0.1:8090 (coordinator HTTP port in dev). */
  controlPlaneHttpUrl: string;
  /** CONTROL_PLANE_BEARER — required: the coordinator app-gRPC bearer token */
  controlPlaneBearer: string;
  /** TRUSTED_ORIGINS — comma-separated list; dev default: http://localhost:5173 */
  trustedOrigins: string[];
  /**
   * BETTER_AUTH_SECRET — signing/encryption key for better-auth cookies and
   * tokens (Task 16). Required: better-auth 1.6.16 has NO silent fallback —
   * it silently uses a publicly known constant with zero warning when the var
   * is absent, and its NODE_ENV production guard is meaningless if the deploy
   * forgets to set NODE_ENV. We make it required here so the server refuses
   * to start without it. The Tiltfile injects a deterministic dev literal;
   * production must rotate this via .env or a secrets manager before Phase 4.
   */
  betterAuthSecret: string;
  /**
   * ENGRAM_KEK_MASTER_KEY — base64-encoded 32-byte master key (KEK) for
   * KEK-envelope sealing of user_session_secrets at rest (ADR 0051 Drip A).
   * This is the SAME key the Rust coordinator's `engram-crypto` uses — the
   * orchestrator shares the engrams encryption key so a value sealed by either
   * is format-identical. In prod it is sourced from GCP Secret Manager; in dev
   * `just bootstrap` writes it to `.env` and the Tiltfile injects it into both
   * the coordinator and the orchestrator. Required: the server refuses to start
   * without it (test-mode escape injects a fixed valid 32-byte placeholder, but
   * the crypto factory takes an injected key in tests so the placeholder is
   * never actually used for real sealing).
   */
  kekMasterKey: string;
  /**
   * IAP_AUDIENCE — GCP IAP audience string (e.g. /projects/PROJECT_NUM/apps/APP_ID).
   * When unset the IAP bridge is fully inert (zero overhead, no header reads).
   * Set this in production when the orchestrator sits behind GCP IAP.
   * This is the production door story: IAP bridge + disabled public sign-up
   * (see Task 22 comment chain) replace password auth in prod.
   */
  iapAudience: string | undefined;
  /**
   * IAP_JWKS_URL — override the GCP IAP JWKS endpoint URL.
   * Default: https://www.gstatic.com/iap/verify/public_key-jwk (ES256 keys,
   * iss=https://cloud.google.com/iap). Override in tests to point at a local
   * fixture server. Not needed in prod.
   */
  iapJwksUrl: string;
  /**
   * Env-driven OIDC ("Sign in with your IdP"), restoring the old coordinator
   * `--auth-mode=oidc` parity. Present only when ORCHESTRATOR_OIDC_ISSUER +
   * CLIENT_ID + CLIENT_SECRET are all set; otherwise `undefined` (OIDC off,
   * email+password / IAP remain). Federated SSO this way also yields real
   * user names (the ID token's `name`/`given_name` claims via the `profile`
   * scope, which stock GCP IAP can't supply).
   */
  oidc: OidcConfig | undefined;
  /**
   * ORCHESTRATOR_ADMIN_EMAILS — comma-separated bootstrap-admin allowlist.
   * Restores the pre-ADR-0051 `auth.bootstrapAdmins` Helm value: matching
   * emails are created with role 'admin' (any JIT path — IAP bridge, OIDC,
   * email/password) and an existing matching user is promoted to admin on
   * sign-in. Normalised (trim + lowercase) at load. Empty when unset → the
   * promotion hooks are fully inert (dev/local default).
   */
  adminEmails: string[];
}

/** A configured generic OIDC provider (better-auth genericOAuth). */
export interface OidcConfig {
  /** Issuer base URL; discovery doc is `${issuer}/.well-known/openid-configuration`. */
  issuer: string;
  clientId: string;
  clientSecret: string;
  /** Stable provider id used in the sign-in call + callback path. */
  providerId: string;
  /** Requested scopes (`profile` pulls the display name). */
  scopes: string[];
}

export function loadConfig(env: Record<string, string | undefined> = process.env): Config {
  const missing: string[] = [];

  // TEST-ONLY ESCAPE: when NODE_ENV==='test' (set automatically by `bun test`)
  // missing required vars get explicit placeholders instead of throwing.
  // This lets unit tests import config without a full env setup.
  // Prod/dev MUST set real values — the Tiltfile already injects all three
  // required vars (BETTER_AUTH_SECRET, CONTROL_PLANE_BEARER,
  // ORCHESTRATOR_DATABASE_URL) into the orchestrator resource.
  const isTest = env["NODE_ENV"] === "test";

  function require(key: string): string {
    const v = env[key];
    if (!v) {
      if (isTest) {
        // test-only placeholder — never used in prod/dev
        return `test-only-${key.toLowerCase().replace(/_/g, "-")}`;
      }
      missing.push(key);
    }
    return v ?? "";
  }

  function optional(key: string, fallback: string): string {
    return env[key] || fallback;
  }

  const portStr = optional("ORCHESTRATOR_PORT", "8787");
  const port = parseInt(portStr, 10);
  const baseUrl = optional("ORCHESTRATOR_PUBLIC_URL", `http://127.0.0.1:${port}`);
  const controlPlaneBearer = require("CONTROL_PLANE_BEARER");
  const controlPlaneGrpcUrl = optional("CONTROL_PLANE_GRPC_URL", "http://127.0.0.1:50061");
  const controlPlaneHttpUrl = optional("CONTROL_PLANE_HTTP_URL", "http://127.0.0.1:8090");
  const databaseUrl = require("ORCHESTRATOR_DATABASE_URL");
  const trustedOriginsRaw = optional("TRUSTED_ORIGINS", "http://localhost:5173");
  const trustedOrigins = trustedOriginsRaw
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);

  // REQUIRED: better-auth 1.6.16 has NO silent dev fallback — it silently
  // uses a publicly known constant when absent, making the secret hole
  // exploitable in any deploy that forgets to set this var. We require it
  // here so the server refuses to start without it. The test-mode escape
  // above applies equally (see isTest comment). Prod/dev must set the real var.
  const betterAuthSecret = require("BETTER_AUTH_SECRET");

  // REQUIRED: ENGRAM_KEK_MASTER_KEY — base64 32-byte KEK shared with the Rust
  // coordinator (engram-crypto). The orchestrator seals user_session_secrets at
  // rest with the same envelope format + same key. The generic `require()`
  // placeholder above would not decode to 32 bytes, so the test-mode escape
  // here returns a FIXED valid base64 32-byte value (all-zero key). Tests that
  // exercise crypto inject their own key via makeSealer(); this placeholder
  // only exists so `loadConfig()` (and seal.ts's lazy default) can be imported
  // under `bun test` without ENGRAM_KEK_MASTER_KEY set. Prod/dev MUST set the
  // real var — the Tiltfile injects it (same value as the coordinator).
  const kekMasterKey = (() => {
    const v = env["ENGRAM_KEK_MASTER_KEY"];
    if (v) return v;
    if (isTest) {
      // base64 of 32 zero bytes — valid length, never used for real sealing.
      return Buffer.alloc(32, 0).toString("base64");
    }
    missing.push("ENGRAM_KEK_MASTER_KEY");
    return "";
  })();

  // OPTIONAL: IAP bridge config. When IAP_AUDIENCE is unset the bridge is
  // fully inert in dev (no overhead, no header reads). Set in prod only.
  // No test placeholder needed — unset is valid and means "inert".
  const iapAudience = env["IAP_AUDIENCE"] || undefined;
  const iapJwksUrl = optional(
    "IAP_JWKS_URL",
    "https://www.gstatic.com/iap/verify/public_key-jwk",
  );

  // OPTIONAL: env-driven OIDC. Opt-in via ORCHESTRATOR_OIDC_ISSUER; when set,
  // CLIENT_ID + CLIENT_SECRET become required (partial config is a hard error,
  // mirroring the old `--auth-mode=oidc` validation — you meant to enable OIDC
  // but under-configured it). Unset = OIDC off.
  const oidcIssuer = env["ORCHESTRATOR_OIDC_ISSUER"]?.trim() || undefined;
  let oidc: OidcConfig | undefined;
  if (oidcIssuer) {
    const clientId = env["ORCHESTRATOR_OIDC_CLIENT_ID"]?.trim() || "";
    const clientSecret = env["ORCHESTRATOR_OIDC_CLIENT_SECRET"]?.trim() || "";
    if (!clientId)
      missing.push("ORCHESTRATOR_OIDC_CLIENT_ID (required when ORCHESTRATOR_OIDC_ISSUER is set)");
    if (!clientSecret)
      missing.push("ORCHESTRATOR_OIDC_CLIENT_SECRET (required when ORCHESTRATOR_OIDC_ISSUER is set)");
    oidc = {
      issuer: oidcIssuer.replace(/\/$/, ""),
      clientId,
      clientSecret,
      providerId: optional("ORCHESTRATOR_OIDC_PROVIDER_ID", "sso"),
      scopes: optional("ORCHESTRATOR_OIDC_SCOPES", "openid email profile")
        .split(/[\s,]+/)
        .filter(Boolean),
    };
  }

  // OPTIONAL: bootstrap-admin allowlist (restores `auth.bootstrapAdmins`).
  // Parsed + normalised (trim/lowercase/de-dup) here; empty when unset, which
  // makes the better-auth promotion hooks fully inert. No test placeholder
  // needed — unset is valid and means "no bootstrap admins".
  const adminEmails = parseAdminEmails(env["ORCHESTRATOR_ADMIN_EMAILS"]);

  if (missing.length > 0) {
    throw new Error(
      `Orchestrator: missing required environment variable(s): ${missing.join(", ")}`,
    );
  }

  if (isNaN(port) || port <= 0 || port > 65535) {
    throw new Error(
      `Orchestrator: ORCHESTRATOR_PORT="${portStr}" is not a valid port number`,
    );
  }

  return {
    port,
    baseUrl,
    databaseUrl,
    controlPlaneGrpcUrl,
    controlPlaneHttpUrl,
    controlPlaneBearer,
    trustedOrigins,
    betterAuthSecret,
    kekMasterKey,
    iapAudience,
    iapJwksUrl,
    oidc,
    adminEmails,
  };
}

export const config: Config = loadConfig();
