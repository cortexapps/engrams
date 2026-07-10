/**
 * API-key header extraction (ADR 0086).
 *
 * The ONE place that decides whether a request carries an API key. Used by
 * both the better-auth apiKey plugin (`customAPIKeyGetter`) and the IAP
 * bridge's routing bypass — sharing the helper means "does this request have
 * a key?" can never skew between the two.
 *
 * Two accepted forms:
 *   - `x-api-key: engk_…` (primary)
 *   - `Authorization: Bearer engk_…` — accepted ONLY with the `engk_` prefix,
 *     so it can never collide with any other bearer usage.
 *
 * Deliberately dependency-free (no auth/db imports): the IAP bridge and its
 * tests must be able to import this without touching better-auth.
 */

/** Prefix of every engrams API key (the plugin's `defaultPrefix`). */
export const API_KEY_PREFIX = "engk_";

/**
 * Extract an API key from request headers, or null if none present.
 * Presence only — validity is decided by the plugin at getSession time.
 */
export function extractApiKey(headers: Headers): string | null {
  const direct = headers.get("x-api-key");
  if (direct) return direct;
  const bearer = headers.get("authorization")?.match(/^Bearer\s+(\S+)$/i)?.[1];
  if (bearer?.startsWith(API_KEY_PREFIX)) return bearer;
  return null;
}
