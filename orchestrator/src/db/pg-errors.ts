/** Shared Postgres driver-error predicates. */

/**
 * Postgres unique-violation SQLSTATE (23505), unwrapping the layers Drizzle
 * wraps around the driver error. A raw insert can throw the node-postgres error
 * directly (code at the top level), while an insert inside `db.transaction(...)`
 * arrives as a `DrizzleQueryError` whose `.cause` carries the original — so we
 * walk the cause chain rather than checking only the top level.
 */
export function isUniqueViolation(error: unknown): boolean {
  let seen = 0;
  for (let e: unknown = error; e != null && seen < 16; seen++) {
    if (typeof e === "object" && (e as { code?: unknown }).code === "23505") {
      return true;
    }
    const cause = (e as { cause?: unknown }).cause;
    if (cause === e) break;
    e = cause;
  }
  return false;
}
