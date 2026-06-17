/**
 * Bootstrap-admin allowlist — restores the pre-ADR-0051 `auth.bootstrapAdmins`
 * Helm value that seeded a designated root/admin account on a fresh deploy.
 *
 * ## Why this exists
 *
 * Before ADR 0051 the Rust coordinator promoted a configured set of emails to
 * the admin role on JIT user upsert (the old `ENGRAM_BOOTSTRAP_ADMINS` env /
 * `auth.bootstrapAdmins` Helm list). When auth moved into this Bun/Hono
 * orchestrator (better-auth), that mechanism was lost — every JIT-created user
 * gets role 'user', so a fresh deploy had no designated admin and no way to
 * grant the first one without manual SQL.
 *
 * This module reintroduces it as the `ORCHESTRATOR_ADMIN_EMAILS` env
 * (comma-separated), surfaced as the `orchestrator.adminEmails` Helm value.
 *
 * ## How it is wired (see better-auth.ts)
 *
 *   - `databaseHooks.user.create.before`: a NEW user whose email matches the
 *     allowlist is created with role 'admin' directly. This covers every
 *     user-creation path — the IAP bridge's `createUser`, the OIDC callback,
 *     and email/password sign-up.
 *
 *   - `databaseHooks.session.create.after`: when an EXISTING user signs in,
 *     promote them to admin if their email is allowlisted and they are not
 *     already admin (and were not manually demoted — see the role check). This
 *     means adding someone to the allowlist AFTER their first login still
 *     grants admin on their next sign-in, matching the old coordinator
 *     behaviour.
 *
 * ## Inert when unset
 *
 * An empty allowlist makes both hooks no-ops: `isBootstrapAdmin` is always
 * false, so no role is ever forced. This is the dev/local default.
 *
 * Matching is case-insensitive and trims surrounding whitespace on both the
 * configured entries and the candidate email (mirrors the old Rust
 * `to_ascii_lowercase()` + `trim()`).
 */

/** The admin role string the better-auth admin plugin reads from `user.role`. */
export const ADMIN_ROLE = "admin" as const;

/**
 * Normalise an email for allowlist comparison: trim + lowercase.
 * Exported for tests and for normalising the configured list once at load.
 */
export function normalizeEmail(email: string): string {
  return email.trim().toLowerCase();
}

/**
 * Parse a comma-separated `ORCHESTRATOR_ADMIN_EMAILS` value into a normalised,
 * de-duplicated list of emails. Empty / whitespace-only entries are dropped, so
 * an unset or blank env yields `[]` (fully inert).
 */
export function parseAdminEmails(raw: string | undefined): string[] {
  if (!raw) return [];
  const seen = new Set<string>();
  for (const part of raw.split(",")) {
    const e = normalizeEmail(part);
    if (e) seen.add(e);
  }
  return [...seen];
}

/**
 * True when `email` is on the (already-normalised) bootstrap-admin allowlist.
 * Case-insensitive + whitespace-tolerant on the candidate. An empty allowlist
 * is always false (inert).
 */
export function isBootstrapAdmin(
  email: string | null | undefined,
  allowlist: readonly string[],
): boolean {
  if (!email || allowlist.length === 0) return false;
  return allowlist.includes(normalizeEmail(email));
}

/**
 * Decide whether an EXISTING user with the given current role should be
 * promoted to admin on sign-in.
 *
 * Promote only when the email is allowlisted AND the user is not already admin.
 * A user that is already 'admin' needs no change. A user manually demoted to a
 * NON-allowlisted state cannot occur here (they'd have to be off the allowlist),
 * so an allowlisted+non-admin user is always re-promoted — matching the old
 * "adding someone to the allowlist after first login still grants admin"
 * behaviour. Returns the role to set, or `undefined` for "no change".
 */
export function promotedRoleOnLogin(
  email: string | null | undefined,
  currentRole: string | null | undefined,
  allowlist: readonly string[],
): typeof ADMIN_ROLE | undefined {
  if (!isBootstrapAdmin(email, allowlist)) return undefined;
  if (currentRole === ADMIN_ROLE) return undefined;
  return ADMIN_ROLE;
}
