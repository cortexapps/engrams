/** Shared Postgres driver-error predicates. */

/**
 * True when `error` or any error in its cause chain carries SQLSTATE `code`.
 * A raw insert can throw the node-postgres error directly (code at the top
 * level), while an insert inside `db.transaction(...)` arrives as a
 * `DrizzleQueryError` whose `.cause` carries the original.
 */
function hasPgCode(error: unknown, code: string): boolean {
  let seen = 0;
  for (let e: unknown = error; e != null && seen < 16; seen++) {
    if (typeof e === "object" && (e as { code?: unknown }).code === code) {
      return true;
    }
    const cause = (e as { cause?: unknown }).cause;
    if (cause === e) break;
    e = cause;
  }
  return false;
}

/** Postgres unique violation (SQLSTATE 23505). */
export function isUniqueViolation(error: unknown): boolean {
  return hasPgCode(error, "23505");
}

/** Postgres foreign-key violation (SQLSTATE 23503). */
export function isForeignKeyViolation(error: unknown): boolean {
  return hasPgCode(error, "23503");
}
