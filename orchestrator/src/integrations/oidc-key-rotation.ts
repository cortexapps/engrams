/**
 * Scheduled OIDC signing-key rotation (ADR 0109 closeout).
 *
 * The key store has always supported overlapping rotation
 * (`integration-oidc-keys.ts`), but nothing called it — the overlap window
 * never ran in production. This module gives rotation two callers:
 *
 *   1. the SCHEDULED policy below (rotate when the active key reaches its
 *      age limit, publish the retiring key for the overlap window), and
 *   2. the explicit admin trigger (`routes/oidc-key-admin.ts`).
 *
 * `rotateOidcKeyIfDue` is the pure step; `startOidcKeyRotation` is the thin
 * timer wrapper (the spawn()/run_once() split, ADR 0098).
 */

import type { IntegrationOidcKeyStore } from "../db/integration-oidc-keys.ts";
import { errorMessage, log as rootLog } from "../log.ts";

/** Rotate the active signing key after 90 days. */
export const OIDC_KEY_ROTATION_AGE_MS = 90 * 24 * 60 * 60 * 1000;

/**
 * Publish a retiring key for 7 days after rotation. Google caches JWKS on its
 * side; the overlap keeps in-flight subject tokens (≤5 min lifetime) and
 * cached JWKS views verifiable.
 */
export const OIDC_KEY_PUBLISH_OVERLAP_MS = 7 * 24 * 60 * 60 * 1000;

/** How often the driver checks whether rotation is due. */
export const OIDC_KEY_ROTATION_CHECK_INTERVAL_MS = 60 * 60 * 1000;

export interface OidcKeyRotationPolicy {
  rotationAgeMs: number;
  publishOverlapMs: number;
}

export const DEFAULT_OIDC_KEY_ROTATION_POLICY: OidcKeyRotationPolicy = {
  rotationAgeMs: OIDC_KEY_ROTATION_AGE_MS,
  publishOverlapMs: OIDC_KEY_PUBLISH_OVERLAP_MS,
};

export interface OidcKeyRotationOutcome {
  /** "created" (no active key existed), "rotated", or "current". */
  action: "created" | "rotated" | "current";
  kid: string;
}

/**
 * The pure rotation step. Ensures an active key exists; rotates it when it
 * reaches the policy age. Concurrent callers are safe: the partial unique
 * index on `state = 'active'` lets exactly one insert win, and the loser's
 * transaction rolls back.
 */
export async function rotateOidcKeyIfDue(
  keys: IntegrationOidcKeyStore,
  now: Date,
  policy: OidcKeyRotationPolicy = DEFAULT_OIDC_KEY_ROTATION_POLICY,
): Promise<OidcKeyRotationOutcome> {
  const published = await keys.listPublished(now);
  const active = published.find((key) => key.state === "active");
  if (!active) {
    const created = await keys.getOrCreateActive(now);
    return { action: "created", kid: created.kid };
  }
  if (now.getTime() - active.createdAt.getTime() < policy.rotationAgeMs) {
    return { action: "current", kid: active.kid };
  }
  const rotated = await keys.rotate(now, policy.publishOverlapMs);
  return { action: "rotated", kid: rotated.kid };
}

export interface OidcKeyRotationDriver {
  stop(): void;
}

/** Thin timer wrapper around `rotateOidcKeyIfDue`. Runs one step immediately
 * (this also creates the first active key at startup, so the public JWKS GET
 * never has to write), then one step per check interval. */
export function startOidcKeyRotation(deps: {
  keys: IntegrationOidcKeyStore;
  now?: () => Date;
  policy?: OidcKeyRotationPolicy;
  checkIntervalMs?: number;
}): OidcKeyRotationDriver {
  const log = rootLog.child({ component: "oidc-key-rotation" });
  const now = deps.now ?? (() => new Date());
  const policy = deps.policy ?? DEFAULT_OIDC_KEY_ROTATION_POLICY;

  async function step(): Promise<void> {
    try {
      const outcome = await rotateOidcKeyIfDue(deps.keys, now(), policy);
      if (outcome.action !== "current") {
        log.info({ action: outcome.action, kid: outcome.kid }, "oidc signing key rotation step");
      }
    } catch (error) {
      // The next tick retries; an unsigned mint fails loudly on its own.
      log.error({ error: errorMessage(error) }, "oidc signing key rotation step failed");
    }
  }

  void step();
  const timer = setInterval(() => {
    void step();
  }, deps.checkIntervalMs ?? OIDC_KEY_ROTATION_CHECK_INTERVAL_MS);
  // Do not hold the process open for the rotation timer.
  timer.unref?.();
  return {
    stop() {
      clearInterval(timer);
    },
  };
}
