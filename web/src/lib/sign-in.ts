/**
 * The login page's view of the deployment's sign-in door.
 *
 * A deployment has ONE door (`mode`), set by the operator and reported by the
 * unauthenticated `GET /api/v1/auth-config`:
 *   - `oauth`    — a "Continue with <provider>" button; the identity provider
 *                  signs the person in and returns them here.
 *   - `password` — the email + password form (`signup` adds registration).
 *   - `iap`      — no door on the page: the proxy in front of the app has
 *                  already signed the person in.
 */

export interface AuthConfig {
  mode: "oauth" | "iap" | "password";
  /** Present in `oauth` mode. */
  oauth?: { providerId: string; displayName: string };
  passwordAuth: boolean;
  signup: boolean;
  /** ADR 0118: needed to validate a `?next=` pointing at a session app. */
  previewBaseDomain?: string;
}

/**
 * Where the identity provider round trip returns to.
 *
 * Both are ABSOLUTE URLs on this origin. The server accepts an absolute URL on
 * a trusted origin whatever its query string holds, while a relative one must
 * match a narrow character set that an encoded `next` can fall outside of.
 *
 * With a `?next=` (ADR 0118: a session app sent the person here), the round
 * trip returns to this login page with the same query. The page then validates
 * `next` against the live preview domain and makes the hop — the one place
 * that can do it safely. The server is never asked to redirect to `next`.
 */
export function oauthReturnUrls(
  search: string,
  origin: string,
): { callbackURL: string; errorCallbackURL: string } {
  const hasNext = Boolean(new URLSearchParams(search).get("next"));
  return {
    callbackURL: hasNext ? `${origin}/login${search}` : `${origin}/`,
    errorCallbackURL: `${origin}/login`,
  };
}

/**
 * The message for a failed OAuth sign-in, from the `?error=` code the server
 * redirects back with. `null` when there is no error to show.
 *
 * The first three codes are this product's own gate
 * (orchestrator/src/auth/oauth-gate.ts); the rest come from the auth library.
 * An unknown code is shown as-is, so a new server-side reason is never
 * swallowed — but it is rendered as text, never as markup or a link.
 */
export function oauthErrorMessage(search: string): string | null {
  const code = new URLSearchParams(search).get("error");
  if (!code) return null;
  switch (code) {
    case "account_not_allowed":
      return "This account does not have access to this deployment. Sign in with a different account, or ask an administrator for access.";
    case "email_not_verified":
      return "Your identity provider has not verified this account's email address, so it cannot be used to sign in.";
    case "email_missing":
    case "email_is_missing":
      return "Your identity provider did not share an email address for this account.";
    case "account_not_linked":
      return "An account with this email address exists, but it cannot be linked to this sign-in. Ask an administrator.";
    case "access_denied":
      return "Sign-in was cancelled.";
    default:
      return `Sign-in failed (${code.slice(0, 80)}). Try again.`;
  }
}
