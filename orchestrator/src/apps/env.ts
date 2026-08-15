/**
 * Peer-address environment variables for a session's apps (ADR 0118).
 *
 * This is the piece that fixes the multi-service case. A frontend configured to
 * call its API at `localhost:8080` works inside the guest but not in the user's
 * browser, where `localhost` is the user's own machine. So the platform gives
 * every process TWO variables for EVERY app in the session:
 *
 *     apps: [{ name: "web", port: 3000 }, { name: "api", port: 8080 }]
 *
 *     WEB_INGRESS_HOST = web-tidy-swift-otters.preview.example.com
 *     WEB_INGRESS_URL  = https://web-tidy-swift-otters.preview.example.com
 *     API_INGRESS_HOST = api-tidy-swift-otters.preview.example.com
 *     API_INGRESS_URL  = https://api-tidy-swift-otters.preview.example.com
 *
 * Both forms, because they are wanted in different places: a CORS allowlist and
 * a cookie domain want the bare host, an HTTP client base URL wants the scheme.
 *
 * EVERY app's address, not just the app's own, because roughly half of what a
 * service needs is a peer's address — the CORS allowlist and the post-login
 * redirect target both live on the API and want the FRONTEND's address. A model
 * where an app can only name its own address cannot express those at all.
 *
 * A profile then remaps them into whatever names its services actually read,
 * via `${…}` interpolation over `profile.envVars`:
 *
 *     CORS_ALLOWED_ORIGINS: ${WEB_INGRESS_URL}
 *     API_BASE_URL:         ${API_INGRESS_URL}
 *
 * The env map already flows to the guest (harness_env → session_env → the
 * SpawnHarness frame agentd applies to every process it spawns), so none of
 * this needs new plumbing or new bytes on a new wire.
 */

import type { ProfileApp } from "../db/schema.ts";
import type { SessionAppRow, SessionAppSpec } from "../db/session-apps.ts";
import { appUrl, isValidAppName } from "./hostname.ts";

/** A declared app that failed validation, with the reason, for logging. */
export interface RejectedApp {
  name: string;
  reason: string;
}

export interface NormalizedApps {
  specs: SessionAppSpec[];
  /** Declarations we dropped. Never fatal — see `normalizeApps`. */
  rejected: RejectedApp[];
}

/**
 * Validate declared apps and drop the ones we cannot serve.
 *
 * Dropping rather than throwing is deliberate: a profile with one malformed app
 * must still start a session. The caller logs `rejected` so the mistake is
 * visible. Profile save-time validation (rpc/profiles.ts) is where a user gets
 * told about it, and it rejects rather than drops.
 */
export function normalizeApps(apps: readonly ProfileApp[]): NormalizedApps {
  const specs: SessionAppSpec[] = [];
  const rejected: RejectedApp[] = [];
  const seenNames = new Set<string>();
  const seenPorts = new Set<number>();

  for (const app of apps) {
    const name = typeof app?.name === "string" ? app.name.trim().toLowerCase() : "";
    const reason = appSpecError({ name, port: app?.port });
    if (reason) {
      rejected.push({ name: name || String(app?.name ?? ""), reason });
      continue;
    }
    if (seenNames.has(name)) {
      rejected.push({ name, reason: "duplicate app name" });
      continue;
    }
    if (seenPorts.has(app.port)) {
      rejected.push({ name, reason: `duplicate port ${app.port}` });
      continue;
    }
    seenNames.add(name);
    seenPorts.add(app.port);
    specs.push({ name, port: app.port });
  }

  return { specs, rejected };
}

/**
 * Why this app declaration is unusable, or null if it is fine. Shared by
 * `normalizeApps` (which drops) and the profile RPCs (which reject), so the two
 * can never disagree about what a valid app is.
 */
export function appSpecError(app: { name: string; port: unknown }): string | null {
  if (!isValidAppName(app.name)) {
    return "name must be 1-24 lowercase alphanumeric characters or interior hyphens";
  }
  if (!Number.isInteger(app.port) || (app.port as number) < 1 || (app.port as number) > 65535) {
    return "port must be an integer in 1..=65535";
  }
  return null;
}

/**
 * The env-var stem for an app: uppercased, every run of non-alphanumerics
 * collapsed to one `_`. `web` → `WEB`, `brain-api` → `BRAIN_API`.
 */
export function envStem(appName: string): string {
  return appName.toUpperCase().replace(/[^A-Z0-9]+/g, "_");
}

/**
 * `<STEM>_INGRESS_HOST` + `<STEM>_INGRESS_URL` for every reserved app.
 *
 * Built from `rows` — what is actually stored — so a retried create builds env
 * from the hostnames that really exist rather than from what was requested.
 */
export function buildIngressEnv(
  rows: readonly SessionAppRow[],
  baseDomain: string,
): Record<string, string> {
  const env: Record<string, string> = {};
  for (const row of rows) {
    const stem = envStem(row.name);
    env[`${stem}_INGRESS_HOST`] = `${row.hostLabel}.${baseDomain}`;
    env[`${stem}_INGRESS_URL`] = appUrl(row.hostLabel, baseDomain);
  }
  return env;
}

/** `${NAME}` — the only interpolation form we support. */
const INTERPOLATION_RE = /\$\{([A-Za-z_][A-Za-z0-9_]*)\}/g;

/** A `${…}` reference shaped like one of ours. Anything else is the user's own. */
const INGRESS_SHAPED_RE = /_INGRESS_(HOST|URL)$/;

export interface InterpolationResult {
  env: Record<string, string>;
  /** `${…}` references that look like ours but resolved to nothing. */
  unresolved: string[];
}

/**
 * Substitute `${NAME}` in every value of `envVars` from `vars`.
 *
 * An unknown `${NAME}` is left **verbatim**, not blanked: env values legitimately
 * carry shell syntax (`${HOME}`, a `$`-quoted password) and silently emptying
 * one would be worse than leaving it for the shell. But a reference shaped like
 * `${*_INGRESS_HOST}` / `${*_INGRESS_URL}` that resolves to nothing is always a
 * typo or a renamed app, so it is reported for the caller to log.
 */
export function interpolateEnv(
  envVars: Readonly<Record<string, string>>,
  vars: Readonly<Record<string, string>>,
): InterpolationResult {
  const env: Record<string, string> = {};
  const unresolved = new Set<string>();
  for (const [key, value] of Object.entries(envVars)) {
    env[key] = value.replace(INTERPOLATION_RE, (whole, name: string) => {
      const hit = vars[name];
      if (hit !== undefined) return hit;
      if (INGRESS_SHAPED_RE.test(name)) unresolved.add(name);
      return whole;
    });
  }
  return { env, unresolved: [...unresolved] };
}
