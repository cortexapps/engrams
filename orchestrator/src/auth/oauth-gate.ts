/**
 * The OAuth sign-in gate: who may come in through the `oauth` door.
 *
 * An OAuth client is a PUBLIC door. A Google client of user type "External"
 * authenticates every Google account there is, and a multi-tenant identity
 * provider does the same for every tenant. Authentication alone therefore
 * says "this is some account", not "this is one of ours". This module answers
 * the second question, and it runs on EVERY OAuth sign-in (new and existing
 * users alike — see `getUserInfo` in sign-in-door.ts).
 *
 * Two checks, in order:
 *
 *   1. The provider must assert that it verified the email (`email_verified`).
 *      The email is the identity here: it selects the user row, it matches the
 *      bootstrap-admin list, and better-auth links a first OAuth sign-in to an
 *      existing user by it. A provider that lets a user type any address into
 *      a profile field would otherwise let that user become anyone.
 *
 *   2. The allowlist. A user is allowed when the email is in `emails`, or the
 *      domain the provider vouches for is in `domains`. `domains: ["*"]`
 *      allows every verified account.
 *
 * "The domain the provider vouches for" is the email's domain, with one
 * exception. Google lets a consumer account carry an address at any domain
 * (`alice@corp.com` as a personal Google account), and that account outlives
 * the mailbox: a person who leaves the company keeps it. Google states the
 * rule itself — restrict by the `hd` (hosted domain) claim, which is present
 * only for an account the Workspace organization manages. So for the Google
 * issuer the domain is `hd`; with no `hd`, only a Gmail address has a domain
 * Google vouches for.
 *   https://developers.google.com/identity/openid-connect/openid-connect#an-id-tokens-payload
 *
 * Pure functions, no I/O — the unit tests drive them directly.
 */

/** Who may sign in. Both lists are normalised (trimmed, lowercased). */
export interface EmailAllowlist {
  /** Allowed domains (`example.com`). `*` allows every verified account. */
  domains: string[];
  /** Allowed individual emails. Includes every bootstrap-admin email. */
  emails: string[];
}

/** The reason a sign-in is refused. It is the `?error=` code the login page shows. */
export type OAuthRejection = "email_missing" | "email_not_verified" | "account_not_allowed";

const GOOGLE_ISSUER = "https://accounts.google.com";

/** Consumer Gmail: the only addresses Google vouches for without an `hd` claim. */
const GMAIL_DOMAINS: ReadonlySet<string> = new Set(["gmail.com", "googlemail.com"]);

function splitList(raw: string | undefined): string[] {
  if (!raw) return [];
  const seen = new Set<string>();
  for (const part of raw.split(",")) {
    const v = part.trim().toLowerCase();
    if (v) seen.add(v);
  }
  return [...seen];
}

/**
 * Parse the two comma-separated env values into a normalised allowlist.
 * A leading `@` on a domain is accepted (`@example.com` and `example.com` are
 * the same entry). `adminEmails` (already normalised) are folded into `emails`:
 * a bootstrap admin that the allowlist locks out would be a deployment that
 * cannot be administered.
 */
export function parseEmailAllowlist(
  domainsRaw: string | undefined,
  emailsRaw: string | undefined,
  adminEmails: readonly string[],
): EmailAllowlist {
  const domains = splitList(domainsRaw).map((d) => (d.startsWith("@") ? d.slice(1) : d));
  const emails = new Set([...splitList(emailsRaw), ...adminEmails]);
  return { domains: [...new Set(domains)], emails: [...emails] };
}

/** True when the allowlist names nobody — the loader refuses that in `oauth` mode. */
export function allowlistIsEmpty(allowlist: EmailAllowlist): boolean {
  return allowlist.domains.length === 0 && allowlist.emails.length === 0;
}

/** Is this issuer Google's OIDC issuer? (Selects the `hd` rule above.) */
export function isGoogleIssuer(issuer: string): boolean {
  return issuer.replace(/\/$/, "") === GOOGLE_ISSUER;
}

function domainOf(email: string): string {
  return email.slice(email.lastIndexOf("@") + 1);
}

/**
 * The domain the provider vouches for, or `undefined` when it vouches for
 * none (a Google consumer account with a non-Gmail address).
 */
function vouchedDomain(
  email: string,
  profile: Record<string, unknown>,
  issuer: string,
): string | undefined {
  const emailDomain = domainOf(email);
  if (!isGoogleIssuer(issuer)) return emailDomain;
  const hd = profile["hd"];
  if (typeof hd === "string" && hd) return hd.toLowerCase();
  return GMAIL_DOMAINS.has(emailDomain) ? emailDomain : undefined;
}

/**
 * Decide one OAuth sign-in from the provider's claims (the ID token payload).
 * Returns `null` to admit, or the reason to refuse.
 */
export function checkOAuthProfile(
  profile: Record<string, unknown>,
  issuer: string,
  allowlist: EmailAllowlist,
): OAuthRejection | null {
  const rawEmail = profile["email"];
  if (typeof rawEmail !== "string" || !rawEmail.includes("@")) return "email_missing";
  const email = rawEmail.trim().toLowerCase();

  // Some providers send the claim as the string "true". Anything else — false,
  // absent, a different string — is refused.
  const verified = profile["email_verified"];
  if (verified !== true && verified !== "true") return "email_not_verified";

  if (allowlist.emails.includes(email)) return null;
  if (allowlist.domains.includes("*")) return null;

  const domain = vouchedDomain(email, profile, issuer);
  if (domain !== undefined && allowlist.domains.includes(domain)) return null;
  return "account_not_allowed";
}

/** What the `oauth` door writes onto a user row. Nothing else. */
export interface OAuthProfile {
  /** The provider's stable subject (`sub`). The account key, with the provider id. */
  id: string;
  email: string;
  /** Always true: `checkOAuthProfile` admits only a verified email. */
  emailVerified: true;
  name: string;
  image?: string;
}

function str(v: unknown): string {
  return typeof v === "string" ? v.trim() : "";
}

/**
 * The display name for a set of claims.
 *
 * `name` is the provider's full name and is normally right. Some providers
 * fill it with the email address, or with one word (a username), when the
 * directory has no full name. In that case `given_name` + `family_name` are
 * the better source, when both are present. With nothing usable, the mailbox
 * name stands in — which is also what the IAP bridge writes, so an account is
 * never left without a name.
 */
export function displayNameFromClaims(claims: Record<string, unknown>): string {
  const full = str(claims["name"]);
  if (!full || full.includes("@") || !full.includes(" ")) {
    const given = str(claims["given_name"]);
    const family = str(claims["family_name"]);
    if (given && family) return `${given} ${family}`;
  }
  if (full) return full;
  const email = str(claims["email"]).toLowerCase();
  return email.split("@")[0] || email;
}

/**
 * The profile for claims that `checkOAuthProfile` ADMITTED, or `null` when
 * the token has no subject. Call it only after the gate.
 *
 * This is a whitelist on purpose. An ID token carries whatever the provider
 * was configured to send (`role`, `groups`, `banned`, …), and the auth library
 * copies every field it is given onto the user row. A provider-side
 * `role: "admin"` must not become an admin here, so only these five fields
 * leave this function.
 */
export function profileFromClaims(claims: Record<string, unknown>): OAuthProfile | null {
  const sub = claims["sub"];
  const id = typeof sub === "number" ? String(sub) : str(sub);
  if (!id) return null;
  const image = str(claims["picture"]);
  return {
    id,
    email: str(claims["email"]).toLowerCase(),
    emailVerified: true,
    name: displayNameFromClaims(claims),
    ...(image ? { image } : {}),
  };
}
