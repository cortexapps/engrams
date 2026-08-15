/**
 * `?next=` validation for the login page (ADR 0118).
 *
 * The preview handler answers an unauthenticated navigation with a 302 to
 * `<main host>/login?next=<the app URL>`, so that after the human logs in the
 * page can return them to the app they were trying to open.
 *
 * That parameter is attacker-controlled — anyone can hand out a link to our own
 * login page with any `next` they like — so it MUST be validated before it is
 * used to navigate. An unvalidated one is a textbook open redirect: our domain
 * lends its credibility to a link that lands somewhere else.
 *
 * Two destinations are allowed, and nothing else:
 *   1. a path on this same origin (where login already sends people), and
 *   2. a single label under the preview base domain — which is exactly the set
 *      of hostnames the preview handler itself routes.
 */

/** Where to go after a successful login when `next` is absent or unusable. */
export const DEFAULT_AFTER_LOGIN = "/";

/**
 * Validate a `next` value against the two allowed destinations.
 *
 * `search` is a query string (`window.location.search`); `previewBaseDomain` is
 * the deployment's preview base domain from `/api/v1/auth-config`, which may be
 * empty on a deployment that runs no preview edge.
 *
 * Returns a URL safe to navigate to, or `DEFAULT_AFTER_LOGIN`.
 */
export function safeNextUrl(
  search: string,
  previewBaseDomain: string | undefined,
  origin: string = window.location.origin,
): string {
  const raw = new URLSearchParams(search).get("next");
  if (!raw) return DEFAULT_AFTER_LOGIN;

  let target: URL;
  try {
    // Resolve against our own origin so a bare path ("/tasks/1") parses, and a
    // protocol-relative value ("//evil.com") resolves to its real host rather
    // than being mistaken for a path.
    target = new URL(raw, origin);
  } catch {
    return DEFAULT_AFTER_LOGIN;
  }

  // Only ever http(s). Blocks `javascript:` and `data:`, which `new URL` parses
  // happily and which would otherwise execute on navigation.
  if (target.protocol !== "http:" && target.protocol !== "https:") {
    return DEFAULT_AFTER_LOGIN;
  }

  // 1. Same origin — the ordinary case.
  if (target.origin === origin) return target.toString();

  // 2. A single label under the preview base domain.
  if (!previewBaseDomain) return DEFAULT_AFTER_LOGIN;
  const suffix = "." + previewBaseDomain.toLowerCase();
  const host = target.host.toLowerCase();
  if (!host.endsWith(suffix)) return DEFAULT_AFTER_LOGIN;
  const label = host.slice(0, -suffix.length);
  // A nested label is not a preview host, and `evil.com.preview.example.com`
  // style prefixes must not sneak through as one.
  if (label === "" || label.includes(".")) return DEFAULT_AFTER_LOGIN;

  return target.toString();
}
