/** Own-property JSON path helpers shared by the automation engine (ADR 0119).
 *
 * `webhook.ts` keeps private copies until its consumers move here; this module
 * is the canonical home for new code. Both walkers refuse inherited
 * properties, so `__proto__`-style payload keys can never reach a lookup.
 */

export const SAFE_PATH_RE = /^[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)*$/;
export const UNSAFE_PATH_SEGMENTS = new Set(["__proto__", "constructor", "prototype"]);

export function isSafePath(path: string): boolean {
  return (
    SAFE_PATH_RE.test(path) &&
    !path.split(".").some((segment) => UNSAFE_PATH_SEGMENTS.has(segment))
  );
}

/** Walk `path` through own properties only; `undefined` when any hop is absent. */
export function ownPath(value: unknown, path: string): unknown {
  let current: unknown = value;
  for (const segment of path.split(".")) {
    if (
      typeof current !== "object" ||
      current === null ||
      !Object.prototype.hasOwnProperty.call(current, segment)
    ) {
      return undefined;
    }
    current = (current as Record<string, unknown>)[segment];
  }
  return current;
}

/** Structural equality over JSON values (objects compared by own keys). */
export function jsonEqual(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (typeof a !== typeof b) return false;
  if (typeof a !== "object" || a === null || b === null) return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  if (Array.isArray(a) && Array.isArray(b)) {
    if (a.length !== b.length) return false;
    return a.every((item, i) => jsonEqual(item, b[i]));
  }
  const aKeys = Object.keys(a as Record<string, unknown>);
  const bRecord = b as Record<string, unknown>;
  if (aKeys.length !== Object.keys(bRecord).length) return false;
  return aKeys.every(
    (key) =>
      Object.prototype.hasOwnProperty.call(bRecord, key) &&
      jsonEqual((a as Record<string, unknown>)[key], bRecord[key]),
  );
}
