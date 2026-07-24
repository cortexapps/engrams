/**
 * Orchestrator runtime configuration.
 *
 * All values are read from environment variables at startup. `loadConfig`
 * throws with a clear message listing every missing required variable so
 * the operator knows exactly what to set before the process can boot.
 */

import { parseAdminEmails } from "./auth/admin-allowlist.ts";
import {
  HEARTBEAT_INTERVAL_MS,
  SWEEP_GRACE_MS,
  SWEEP_INTERVAL_MS,
} from "./sweep/sweeper.ts";

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
  /** GITHUB_APP_LOGIN — the review GitHub App's handle (its bot login/slug,
   * e.g. "acme-reviewer"), the token users @-mention on a PR to run a review.
   * ADR 0100 uses one App per deployment, so this is deployment config, not a
   * hardcoded string. Empty = `@`-mention commands are disabled (dispatch API
   * and auto-on-open still work). Accepts an optional leading "@" / "[bot]". */
  githubAppLogin: string;
  /** TRUSTED_ORIGINS — comma-separated list; dev default: http://localhost:5173 */
  trustedOrigins: string[];
  /**
   * ORCHESTRATOR_DEVICE_VERIFICATION_URL — the human-facing page `engrams
   * auth login` sends the browser to (the SPA's /device route). Default:
   * `${baseUrl}/device` — ORCHESTRATOR_PUBLIC_URL is the browser-facing
   * origin in both dev (the Tiltfile sets http://localhost:5173) and prod
   * (https://engrams.cortex.io), so the default lands on the SPA either way.
   */
  deviceVerificationUrl: string;
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
   * IAP_AUDIENCES — the SET of GCP IAP audiences the orchestrator trusts
   * (comma-separated in the env). Each audience is a backend-service / app
   * resource string (e.g. /projects/PROJECT_NUM/global/backendServices/ID). An
   * assertion verifies if its `aud` matches ANY entry.
   *
   * Why a set, not one value: the orchestrator sits behind MORE THAN ONE
   * IAP-protected GCP backend service, and a GCP IAP audience IS the backend
   * service resource — there is no way to share one audience across backends.
   * The app enters via the classic web Ingress backend; live-host port previews
   * (ADR 0064) enter via a DEDICATED Gateway backend (required for the wildcard
   * `*.preview` Certificate Manager cert, which the classic Ingress can't hold).
   * Two IAP front doors → two audiences, both trusted here. Verification stays
   * uniform (same JWKS / issuer / ES256); only the accepted `aud` set differs —
   * the auth layer NEVER branches on Host or path.
   *
   * Empty (env unset) → the IAP bridge is fully inert (zero overhead, no header
   * reads). Set in production when the orchestrator sits behind GCP IAP.
   * This is the production door story: IAP bridge + disabled public sign-up
   * replace password auth in prod.
   */
  iapAudiences: string[];
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
   * ORCHESTRATOR_PREVIEW_BASE_DOMAIN — the wildcard base under which live-host
   * port previews are served (ADR 0064): a preview URL is
   * `<scheme>://<slug>.<previewBaseDomain>`. Prod sets
   * `preview.engrams.cortex.io` (behind the IAP wall, *443*). Dev default is
   * `lvh.me:<port>` — `*.lvh.me` resolves to 127.0.0.1, so the preview hits this
   * same orchestrator over plain http with no DNS/cert setup. The edge proxy
   * (P2b) also matches inbound Host headers against this to route previews.
   */
  previewBaseDomain: string;
  /**
   * ORCHESTRATOR_ADMIN_EMAILS — comma-separated bootstrap-admin allowlist.
   * Restores the pre-ADR-0051 `auth.bootstrapAdmins` Helm value: matching
   * emails are created with role 'admin' (any JIT path — IAP bridge, OIDC,
   * email/password) and an existing matching user is promoted to admin on
   * sign-in. Normalised (trim + lowercase) at load. Empty when unset → the
   * promotion hooks are fully inert (dev/local default).
   */
  adminEmails: string[];
  /**
   * ORCHESTRATOR_SWEEP_DISABLED — emergency kill switch for the DBOS orphan
   * sweeper only. The version heartbeat remains active so other pods never
   * mistake this live application version for an abandoned one. "1" or
   * "true" enables it; default false.
   */
  sweepDisabled: boolean;
  /**
   * ORCHESTRATOR_SWEEP_ALERT_CHANNEL — Slack channel for generic DBOS sweep
   * alerts. Empty (the default) logs alerts locally instead; terminal-failure
   * cleanup callbacks still run.
   */
  sweepAlertChannel: string;
  /** ORCHESTRATOR_SWEEP_INTERVAL_MS — default SWEEP_INTERVAL_MS. */
  sweepIntervalMs: number;
  /** ORCHESTRATOR_SWEEP_GRACE_MS — default SWEEP_GRACE_MS. */
  sweepGraceMs: number;
  /**
   * ORCHESTRATOR_SWEEP_HEARTBEAT_INTERVAL_MS — default
   * HEARTBEAT_INTERVAL_MS.
   */
  sweepHeartbeatIntervalMs: number;
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

  function positiveNumber(key: string, fallback: number): number {
    const raw = env[key];
    if (raw === undefined) return fallback;
    const value = Number(raw);
    if (!Number.isFinite(value) || value <= 0) {
      throw new Error(
        `Orchestrator: ${key}="${raw}" must be a positive number`,
      );
    }
    return value;
  }

  const portStr = optional("ORCHESTRATOR_PORT", "8787");
  const port = parseInt(portStr, 10);
  const baseUrl = optional("ORCHESTRATOR_PUBLIC_URL", `http://127.0.0.1:${port}`);
  const controlPlaneBearer = require("CONTROL_PLANE_BEARER");
  const githubAppLogin = optional("GITHUB_APP_LOGIN", "");
  const controlPlaneGrpcUrl = optional("CONTROL_PLANE_GRPC_URL", "http://127.0.0.1:50061");
  const controlPlaneHttpUrl = optional("CONTROL_PLANE_HTTP_URL", "http://127.0.0.1:8090");
  const databaseUrl = require("ORCHESTRATOR_DATABASE_URL");
  const trustedOriginsRaw = optional("TRUSTED_ORIGINS", "http://localhost:5173");
  const trustedOrigins = trustedOriginsRaw
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
  const deviceVerificationUrl = optional(
    "ORCHESTRATOR_DEVICE_VERIFICATION_URL",
    `${baseUrl.replace(/\/$/, "")}/device`,
  );

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

  // OPTIONAL: IAP bridge config. IAP_AUDIENCES is a comma-separated SET of
  // trusted audiences (see the Config.iapAudiences doc for why it's a set).
  // Empty → the bridge is fully inert in dev (no overhead, no header reads).
  // No test placeholder needed — empty is valid and means "inert".
  const iapAudiences = (env["IAP_AUDIENCES"] ?? "")
    .split(",")
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
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

  // OPTIONAL: preview base domain for live-host port URLs (ADR 0064). Dev
  // default `lvh.me:<port>` resolves `*.lvh.me` → 127.0.0.1 → this orchestrator.
  const previewBaseDomain = optional("ORCHESTRATOR_PREVIEW_BASE_DOMAIN", `lvh.me:${port}`);

  // OPTIONAL: bootstrap-admin allowlist (restores `auth.bootstrapAdmins`).
  // Parsed + normalised (trim/lowercase/de-dup) here; empty when unset, which
  // makes the better-auth promotion hooks fully inert. No test placeholder
  // needed — unset is valid and means "no bootstrap admins".
  const adminEmails = parseAdminEmails(env["ORCHESTRATOR_ADMIN_EMAILS"]);

  // OPTIONAL: DBOS orphan-sweep operations. The boolean is deliberately
  // narrow: only the documented "1" and "true" spellings activate the kill
  // switch. Interval values reject explicit invalid input instead of silently
  // falling back.
  const sweepDisabled =
    env["ORCHESTRATOR_SWEEP_DISABLED"] === "1" ||
    env["ORCHESTRATOR_SWEEP_DISABLED"] === "true";
  const sweepAlertChannel = env["ORCHESTRATOR_SWEEP_ALERT_CHANNEL"] ?? "";
  const sweepIntervalMs = positiveNumber(
    "ORCHESTRATOR_SWEEP_INTERVAL_MS",
    SWEEP_INTERVAL_MS,
  );
  const sweepGraceMs = positiveNumber(
    "ORCHESTRATOR_SWEEP_GRACE_MS",
    SWEEP_GRACE_MS,
  );
  const sweepHeartbeatIntervalMs = positiveNumber(
    "ORCHESTRATOR_SWEEP_HEARTBEAT_INTERVAL_MS",
    HEARTBEAT_INTERVAL_MS,
  );
  // A live pod proves its version with a heartbeat every interval, and the
  // SIGTERM handler stops the heartbeat only AFTER the DBOS drain completes
  // (index.ts) — so whenever workflow code can execute, the freshest beat is
  // at most one interval + one missed beat old. 2× the interval is therefore
  // the true floor; below it the sweep can declare a healthy pod's version
  // dead between beats and re-enqueue workflows that still have a live owner
  // (double execution).
  if (sweepGraceMs < 2 * sweepHeartbeatIntervalMs) {
    throw new Error(
      `Orchestrator: ORCHESTRATOR_SWEEP_GRACE_MS (${sweepGraceMs}) must be at ` +
        `least twice ORCHESTRATOR_SWEEP_HEARTBEAT_INTERVAL_MS ` +
        `(${sweepHeartbeatIntervalMs}); a shorter grace window can adopt ` +
        `workflows whose owner pod is alive but between heartbeats`,
    );
  }

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
    githubAppLogin,
    trustedOrigins,
    deviceVerificationUrl,
    betterAuthSecret,
    kekMasterKey,
    iapAudiences,
    iapJwksUrl,
    oidc,
    previewBaseDomain,
    adminEmails,
    sweepDisabled,
    sweepAlertChannel,
    sweepIntervalMs,
    sweepGraceMs,
    sweepHeartbeatIntervalMs,
  };
}

export const config: Config = loadConfig();
