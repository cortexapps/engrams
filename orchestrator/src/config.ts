/**
 * Orchestrator runtime configuration.
 *
 * All values are read from environment variables at startup. `loadConfig`
 * throws with a clear message listing every missing required variable so
 * the operator knows exactly what to set before the process can boot.
 */

export interface Config {
  /** ORCHESTRATOR_PORT — default 8787 */
  port: number;
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

  // OPTIONAL: IAP bridge config. When IAP_AUDIENCE is unset the bridge is
  // fully inert in dev (no overhead, no header reads). Set in prod only.
  // No test placeholder needed — unset is valid and means "inert".
  const iapAudience = env["IAP_AUDIENCE"] || undefined;
  const iapJwksUrl = optional(
    "IAP_JWKS_URL",
    "https://www.gstatic.com/iap/verify/public_key-jwk",
  );

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
    databaseUrl,
    controlPlaneGrpcUrl,
    controlPlaneHttpUrl,
    controlPlaneBearer,
    trustedOrigins,
    betterAuthSecret,
    iapAudience,
    iapJwksUrl,
  };
}

export const config: Config = loadConfig();
