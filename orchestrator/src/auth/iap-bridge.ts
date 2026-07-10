/**
 * GCP IAP trusted-SSO bridge — ADR 0051 §5 / Task 30.
 *
 * ## What this does
 *
 * Behind GCP IAP every request carries an `X-Goog-IAP-JWT-Assertion` header
 * signed with ES256 (JWKS at https://www.gstatic.com/iap/verify/public_key-jwk,
 * iss=https://cloud.google.com/iap). This middleware:
 *
 *   1. Verifies the assertion JWT (iss + aud).
 *   2. JIT-creates the better-auth user (role 'user') if absent.
 *   3. Creates a better-auth session and writes the signed session cookie into
 *      BOTH the response (so all subsequent requests ride the cookie and the
 *      bridge becomes a no-op) AND the incoming request's Cookie header (so the
 *      downstream get-session/auth-guard resolves the session on THIS request —
 *      otherwise a first-time user's first request returns a null session and
 *      the SPA bounces them to /login; see injectRequestSessionCookie).
 *
 * ## Inert posture (dev / local)
 *
 * HEALTH-CHECK EXEMPTION (resolved): GCP load-balancer health checks AND
 * kubelet readiness probes bypass IAP and carry no assertion — with
 * IAP_AUDIENCES set, /healthz would 401 and the backend/pod is marked
 * unhealthy. `iapBridge` now short-circuits `/healthz` before any IAP logic
 * (see the top of the function), so probes always reach the health route.
 *
 * When `IAP_AUDIENCES` is empty the bridge is fully inert — it returns
 * immediately without touching headers, allocating memory, or reading env.
 * This is the dev-mode default; the Tiltfile does not set IAP_AUDIENCES.
 *
 * ## Production door story
 *
 * This bridge + disabling public sign-up (Task 22 comment chain) is the prod
 * door story for the orchestrator. Behind IAP every request carries the
 * assertion; the bridge converts it into a better-auth session on first hit
 * so the rest of the app is standard cookie-auth. Public sign-up should be
 * disabled before a prod deploy past Phase 4.
 *
 * ## Placement (covers all entry paths)
 *
 * The bridge is a raw node:http wrapper installed in `buildServer` BEFORE the
 * /rpc vs Hono dispatch. This covers:
 *
 *   - /rpc/* (Connect adapter): the bridge runs, may set the cookie, then
 *     passes to connectHandler. Connect RPCs themselves are bearer-auth, but
 *     the bridge ensures the session cookie is set for any browser hitting
 *     a Connect-shaped URL (unlikely but safe).
 *
 *   - Everything else (Hono via getRequestListener): the bridge runs first,
 *     then Hono — session cookie is set before Hono's auth guard sees it.
 *
 *   - WS upgrade (node:http 'upgrade' event): NOT intercepted by this bridge.
 *     WS sessions are resolved via the cookie set on a prior page load (the
 *     upgrade handler calls app.request which reads cookies). Acceptable: the
 *     bridge will have already set the session cookie before the WS handshake.
 *     Documented explicitly here so future reviewers don't wonder.
 *
 * ## Session creation mechanism (better-auth 1.6.16)
 *
 * better-auth has no public "create a session for an arbitrary verified user"
 * API. After spiking the installed version:
 *
 *   - `auth.api.signInEmail` — won't work (requires a password).
 *   - `auth.$context` — a Promise<BetterAuthContext> exposing `internalAdapter`
 *     which has `findUserByEmail`, `createUser`, and `createSession(userId)`.
 *     This is the same path used by better-auth's own test-utils
 *     (`auth-helpers.mjs`) and the magic-link plugin.
 *
 * Chosen mechanism: `(await auth.$context).internalAdapter`:
 *   - `findUserByEmail(email)` → user or null
 *   - `createUser({ email, name, emailVerified: true })` → user (JIT-create)
 *   - `createSession(userId)` → session (returns { token, ... })
 *
 * Then sign the token with `makeSignature(token, ctx.secret)` (HMAC-SHA256,
 * same as better-auth's internal signCookieValue) and write a Set-Cookie header
 * with the signed token under `ctx.authCookies.sessionToken.name`.
 *
 * ## Email claim format (GCP IAP)
 *
 * IAP puts the user's email in the `email` claim. The `sub` claim uses the
 * format `accounts.google.com:USER@EXAMPLE.COM`. We strip the
 * `accounts.google.com:` prefix from `sub` if present, but prefer the `email`
 * claim directly (cleaner, documented as the canonical email field in GCP docs:
 * https://cloud.google.com/iap/docs/signed-headers-howto).
 *
 * ## Fast-path: existing valid session cookie
 *
 * If the request already carries a valid better-auth session cookie, and the
 * IAP email matches the session's user email, the bridge skips JWT verification
 * entirely (no JWKS fetch, no parse). This is important for cost:
 *
 *   - First request behind IAP: JWT verified → session created → cookie set.
 *   - Subsequent requests: cookie present + email matches → bridge skips.
 *
 * User-switch: if the IAP email differs from the cookie's user email (e.g. a
 * different Google account is now authenticated at the IAP level), the bridge
 * re-bridges: verifies the new JWT, JIT-creates the new user if absent, creates
 * a new session, overwrites the cookie.
 *
 * ## Fail-closed
 *
 * When IAP_AUDIENCES is set but the request lacks a valid assertion AND lacks a
 * valid session cookie, the bridge returns 401. Behind IAP every real request
 * carries the header, so a missing assertion means either the request bypassed
 * IAP (forbidden) or the JWT is invalid (also forbidden). We fail closed.
 */

import type { IncomingMessage, ServerResponse } from "node:http";
import { createRemoteJWKSet, jwtVerify, type JWTPayload } from "jose";
import { auth } from "./better-auth.ts";
import { config } from "../config.ts";
import { extractApiKey } from "./api-key-header.ts";

// ---------------------------------------------------------------------------
// Public (unauthenticated) paths
// ---------------------------------------------------------------------------

/**
 * Paths the bridge lets through WITHOUT any IAP/session check — matched
 * exactly against the path component (query string stripped).
 *
 * Two categories live here:
 *
 *   1. Health/readiness probes. kubelet readiness probes and GCP LB health
 *      checks hit the orchestrator directly, bypassing IAP, so they carry no
 *      X-Goog-IAP-JWT-Assertion. With IAP_AUDIENCES set the bridge fails closed
 *      (401), which would mark the pod / backend perpetually unhealthy.
 *      /healthz only reports {ok, db}, so it is unauthenticated by design.
 *
 *   2. Inbound provider webhooks (ADR 0060). Slack POSTs carry no IAP
 *      assertion and no session cookie — in prod they reach the orchestrator
 *      through a no-IAP GCLB backend (see deploy/helm values-iap overlay), so
 *      the fail-closed path would 401 them before the handler runs. These
 *      routes do NOT rely on the bridge for auth: each verifies the provider's
 *      own signature (Slack signing secret + 5-minute timestamp window via
 *      isValidSlackRequest) and rejects a bad signature with 401 itself. The
 *      bridge would only get in the way, so we exempt the exact webhook paths.
 *      The strings must stay in lockstep with the routes' `app.post(...)` paths
 *      (orchestrator/src/routes/slack-{events,interactivity}.ts).
 *
 * Add new probe/observability/webhook paths here rather than scattering inline
 * checks.
 */
const PUBLIC_PATHS: ReadonlySet<string> = new Set([
  "/healthz",
  "/api/v1/integrations/slack/events",
  "/api/v1/integrations/slack/interactivity",
]);

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

/**
 * Parsed + verified IAP JWT claims we care about.
 * Ref: https://cloud.google.com/iap/docs/signed-headers-howto
 */
interface IapClaims {
  /** User email (canonical; preferred over sub). */
  email: string;
}

// ---------------------------------------------------------------------------
// Module-level JWKS set (created lazily on first request when IAP is active).
// Cached to avoid creating a new RemoteJWKSet on every request.
// ---------------------------------------------------------------------------

let _jwks: ReturnType<typeof createRemoteJWKSet> | undefined;

function getJwks(): ReturnType<typeof createRemoteJWKSet> {
  if (!_jwks) {
    _jwks = createRemoteJWKSet(new URL(config.iapJwksUrl));
  }
  return _jwks;
}

// ---------------------------------------------------------------------------
// Cookie helpers (mirrors better-auth's internal signCookieValue)
// ---------------------------------------------------------------------------

/** Import is dynamic so the module can be imported in test without DB. */
async function signCookieValue(value: string, secret: string): Promise<string> {
  // better-auth signs cookies as `value.HMAC-SHA256-base64(value)`
  // using the `makeSignature` function from better-auth/dist/crypto/index.mjs.
  // We replicate it here to avoid depending on a non-exported path.
  const algorithm = { name: "HMAC", hash: "SHA-256" };
  const secretBuf = new TextEncoder().encode(secret);
  const key = await crypto.subtle.importKey("raw", secretBuf, algorithm, false, ["sign"]);
  const signature = await crypto.subtle.sign(
    algorithm.name,
    key,
    new TextEncoder().encode(value),
  );
  const b64 = btoa(String.fromCharCode(...new Uint8Array(signature)));
  return `${value}.${b64}`;
}

/**
 * Extract the session-token cookie value (unsigned) from an IncomingMessage.
 * Returns undefined if the cookie is absent.
 */
function extractRawSessionCookie(req: IncomingMessage, cookieName: string): string | undefined {
  const cookieHeader = req.headers["cookie"] ?? "";
  // Cookies are `name=value; name2=value2`.
  for (const part of cookieHeader.split(";")) {
    const trimmed = part.trim();
    const eqIdx = trimmed.indexOf("=");
    if (eqIdx === -1) continue;
    const name = trimmed.slice(0, eqIdx).trim();
    const value = trimmed.slice(eqIdx + 1).trim();
    if (name === cookieName) return value;
  }
  return undefined;
}

// ---------------------------------------------------------------------------
// IAP JWT verification
// ---------------------------------------------------------------------------

/** Verify the X-Goog-IAP-JWT-Assertion against the trusted audience set and
 * return the email claim. `jose` accepts `audience: string[]` and passes if the
 * token's `aud` matches ANY entry — so one uniform verifier covers every IAP
 * front door (see Config.iapAudiences for why there's more than one). */
async function verifyIapJwt(
  jwt: string,
  audiences: string[],
): Promise<IapClaims> {
  const jwks = getJwks();
  const { payload } = await jwtVerify(jwt, jwks, {
    issuer: "https://cloud.google.com/iap",
    audience: audiences,
    algorithms: ["ES256"],
  });

  const email = extractEmail(payload);
  if (!email) {
    throw new Error("IAP JWT missing email claim");
  }
  return { email };
}

/** Extract email from IAP JWT payload.
 *
 * IAP sets:
 *   - `email` claim: user@example.com (canonical, preferred)
 *   - `sub` claim: accounts.google.com:user@example.com
 *
 * We use `email` if present, otherwise strip the prefix from `sub`.
 */
function extractEmail(payload: JWTPayload): string | undefined {
  if (typeof payload["email"] === "string" && payload["email"]) {
    return payload["email"];
  }
  if (typeof payload["sub"] === "string" && payload["sub"]) {
    const sub = payload["sub"];
    const prefix = "accounts.google.com:";
    return sub.startsWith(prefix) ? sub.slice(prefix.length) : sub;
  }
  return undefined;
}

// ---------------------------------------------------------------------------
// Better-auth JIT user/session creation
// ---------------------------------------------------------------------------

/**
 * Given a verified email, ensure a better-auth user exists (JIT-create if
 * absent) and create a new session. Returns the session token.
 */
async function jitCreateSession(email: string): Promise<string> {
  const ctx = await auth.$context;
  const ia = ctx.internalAdapter;

  // Find or create the user.
  let user = (await ia.findUserByEmail(email))?.user ?? null;
  if (!user) {
    user = await ia.createUser({
      email,
      name: email.split("@")[0] ?? email,
      // IAP only forwards requests for Google-verified accounts, and we just
      // verified IAP's own ES256 assertion — the email is attested by GCP.
      emailVerified: true,
      // role is set by the admin plugin; default is 'user' for new users.
      // The admin plugin reads the `role` column; we don't set it here so
      // it defaults to whatever the admin plugin initialises (null/undefined
      // → treated as 'user' everywhere in the CASL policy).
    });
  }

  const session = await ia.createSession(user.id);
  return session.token;
}

/** A freshly-minted better-auth session cookie, in the two forms we need. */
interface SessionCookie {
  /** Cookie name (e.g. `better-auth.session_token`). */
  name: string;
  /** Signed value (`token.HMAC`) — what both Set-Cookie and the Cookie header carry. */
  signedValue: string;
  /** Full `Set-Cookie` header value (name=value + attributes). */
  setCookieHeader: string;
}

/**
 * Build the better-auth session cookie for a freshly-created session token.
 * Mirrors what better-auth's setSignedCookie does internally, and also returns
 * the raw name/value so the bridge can inject it into the *request* (see
 * `injectRequestSessionCookie`) — not just the response.
 */
async function buildSessionCookie(token: string): Promise<SessionCookie> {
  const ctx = await auth.$context;
  const secret = ctx.secret;
  const cookieName = ctx.authCookies.sessionToken.name;
  const attrs = ctx.authCookies.sessionToken.attributes;

  const signedValue = await signCookieValue(token, secret);

  const parts: string[] = [`${cookieName}=${signedValue}`];
  if (attrs.path) parts.push(`Path=${attrs.path}`);
  if (attrs.httpOnly) parts.push("HttpOnly");
  if (attrs.secure) parts.push("Secure");
  if (attrs.sameSite) parts.push(`SameSite=${attrs.sameSite}`);
  if (attrs.maxAge !== undefined) parts.push(`Max-Age=${attrs.maxAge}`);
  return { name: cookieName, signedValue, setCookieHeader: parts.join("; ") };
}

/**
 * Inject the freshly-minted session cookie into the *incoming request's* Cookie
 * header so the downstream handler (better-auth's get-session, the Hono auth
 * guard, a Connect RPC) resolves the session on THIS request — not only on the
 * next one.
 *
 * Without this, a first-time IAP user's very first request (the SPA's
 * `GET /api/auth/get-session`) carries no better-auth cookie: the bridge writes
 * Set-Cookie on the *response*, but get-session reads the *request* cookie,
 * finds none, and returns a null session. The SPA then bounces the user to
 * /login even though the bridge just authenticated them. Returning users carry
 * the cookie and avoid this, which is why the bug is new-user-only.
 *
 * Any pre-existing cookie of the same name (a stale/other-user session, e.g.
 * the user-switch case) is dropped so the new session wins unambiguously.
 *
 * Both `req.headers.cookie` AND `req.rawHeaders` are updated: @hono/node-server's
 * getRequestListener builds the Fetch Request's headers from `rawHeaders` (NOT
 * the parsed `headers` object), so mutating only `headers.cookie` would be
 * invisible to the Hono/better-auth handler. We rewrite both so every downstream
 * reader (Hono via rawHeaders, Connect/node code via headers) sees one merged
 * Cookie header.
 */
function injectRequestSessionCookie(req: IncomingMessage, name: string, signedValue: string): void {
  // node:http collapses multiple Cookie request headers into a single
  // `headers.cookie` joined with "; ", so this is the full pre-existing set.
  const existing = req.headers["cookie"] ?? "";
  const kept = existing
    .split(";")
    .map((p) => p.trim())
    .filter((p) => p && p.slice(0, p.indexOf("=")).trim() !== name);
  kept.push(`${name}=${signedValue}`);
  const merged = kept.join("; ");

  req.headers["cookie"] = merged;

  // rawHeaders is a flat [key, value, key, value, ...] array. Drop every Cookie
  // pair (case-insensitive) and append a single merged one.
  const raw = req.rawHeaders;
  if (Array.isArray(raw)) {
    const rebuilt: string[] = [];
    for (let i = 0; i + 1 < raw.length; i += 2) {
      if (raw[i]!.toLowerCase() === "cookie") continue;
      rebuilt.push(raw[i]!, raw[i + 1]!);
    }
    rebuilt.push("cookie", merged);
    req.rawHeaders = rebuilt;
  }
}

/**
 * Verify the existing session cookie and return the user's email if valid.
 * Returns undefined if the cookie is absent, invalid, or session not found.
 */
/** Convert IncomingMessage headers to a Fetch Headers object. */
function headersOf(req: IncomingMessage): Headers {
  const headers = new Headers();
  for (const [key, val] of Object.entries(req.headers)) {
    if (!val) continue;
    if (Array.isArray(val)) {
      for (const v of val) headers.append(key, v);
    } else {
      headers.set(key, val);
    }
  }
  return headers;
}

async function verifyExistingSession(
  req: IncomingMessage,
): Promise<{ email: string } | undefined> {
  try {
    const headers = headersOf(req);
    const session = await auth.api.getSession({ headers } as Parameters<
      typeof auth.api.getSession
    >[0]);

    if (!session?.user?.email) return undefined;
    return { email: session.user.email };
  } catch {
    return undefined;
  }
}

// ---------------------------------------------------------------------------
// Bridge handler (public API consumed by buildServer)
// ---------------------------------------------------------------------------

export type BridgeNext = () => void;

/**
 * IAP bridge request handler.
 *
 * Call this at the raw node:http level BEFORE routing to Connect/Hono.
 * It calls `next()` when the request should proceed, or writes a 401 and
 * returns (without calling `next`) when the bridge rejects the request.
 *
 * When IAP_AUDIENCES is empty this is a synchronous no-op (calls next() inline).
 *
 * @param req  Incoming HTTP request.
 * @param res  Server response (used to write Set-Cookie / 401).
 * @param next Callback to invoke when the request should continue.
 */
export async function iapBridge(
  req: IncomingMessage,
  res: ServerResponse,
  next: BridgeNext,
): Promise<void> {
  // PUBLIC-PATH EXEMPTION (resolves the deploy landmine in the module header):
  // health/readiness probes bypass IAP and carry no assertion, so the
  // fail-closed path below would 401 them → the pod never goes Ready. Let the
  // allowlist through before any IAP logic. Checked even when IAP is inert so
  // the path is identical in dev and prod. See PUBLIC_PATHS for the rationale.
  const path = (req.url ?? "/").split("?", 1)[0];
  if (PUBLIC_PATHS.has(path)) {
    next();
    return;
  }

  // INERT PATH: IAP_AUDIENCES empty → bridge is fully off.
  if (config.iapAudiences.length === 0) {
    next();
    return;
  }

  // API-KEY BYPASS (ADR 0086): a programmatic request carrying an API key
  // (x-api-key or `Authorization: Bearer engk_…`) skips the IAP wall — IAP can
  // only attest humans, and the fail-closed 401 below would otherwise block
  // every keyed caller. This is a pure ROUTING bypass: nothing is minted or
  // granted here. getSession at the auth seams validates the key (the apiKey
  // plugin's mock-session hook); a junk key sails past the bridge and still
  // fails closed at every seam with its normal Unauthenticated/401. Checked
  // BEFORE verifyExistingSession: the mock session it would resolve carries a
  // @service.local email that can never match an IAP assertion, and the
  // user-switch logic below would 401 the request.
  if (extractApiKey(headersOf(req))) {
    next();
    return;
  }

  const audiences = config.iapAudiences;

  // ---------------------------------------------------------------------------
  // Fast path: check existing session cookie first (avoid JWT verification).
  // ---------------------------------------------------------------------------
  const existingSession = await verifyExistingSession(req);
  if (existingSession) {
    const iapJwt = req.headers["x-goog-iap-jwt-assertion"] as string | undefined;

    if (iapJwt) {
      // We have both: check for user-switch without full verification overhead.
      // Decode the email claim WITHOUT verifying. This is sound because the
      // HMAC session cookie is the credential — the unverified email only
      // chooses skip-vs-reverify. No cookie → full verification; a stolen
      // valid cookie already owns that session (the no-header branch passes
      // it anyway); a forged MISMATCHING email forces full verification and
      // 401s. Zero privilege derives from the unverified claim.
      try {
        const parts = iapJwt.split(".");
        if (parts.length === 3) {
          const rawPayload = JSON.parse(
            Buffer.from(parts[1]!, "base64url").toString("utf8"),
          ) as JWTPayload;
          const iapEmail = extractEmail(rawPayload);
          if (iapEmail && iapEmail.toLowerCase() === existingSession.email.toLowerCase()) {
            // Same user — fast path, skip full JWT verification.
            next();
            return;
          }
          // Different IAP user → fall through to re-bridge below.
        }
      } catch {
        // Malformed JWT — fall through to full verification (will likely 401).
      }
    } else {
      // No IAP assertion header but we have a valid session. We're behind IAP
      // so every request SHOULD carry the header. However if somehow the
      // assertion is absent but a valid session cookie is present (e.g. the
      // request came through a load balancer that strips the header), we
      // let the session cookie stand and pass through. This is a conservative
      // choice: the cookie was set by a prior verified bridge request.
      next();
      return;
    }
  }

  // ---------------------------------------------------------------------------
  // Full path: verify the IAP JWT and bridge into a better-auth session.
  // ---------------------------------------------------------------------------
  const iapJwt = req.headers["x-goog-iap-jwt-assertion"] as string | undefined;

  if (!iapJwt) {
    // Behind IAP, every request carries the assertion. Missing = reject.
    res.writeHead(401, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ error: "missing IAP assertion" }));
    return;
  }

  let claims: IapClaims;
  try {
    claims = await verifyIapJwt(iapJwt, audiences);
  } catch (err) {
    res.writeHead(401, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ error: "invalid IAP assertion" }));
    return;
  }

  // JIT-create user + session.
  let token: string;
  try {
    token = await jitCreateSession(claims.email);
  } catch (err) {
    // DB error or similar — 500.
    res.writeHead(500, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ error: "session creation failed" }));
    return;
  }

  // Write the session cookie into the response (for subsequent requests) AND
  // into this request (so the downstream get-session/auth-guard resolves the
  // session on THIS request — see injectRequestSessionCookie for the new-user
  // /login-bounce bug this prevents).
  const cookie = await buildSessionCookie(token);
  res.setHeader("Set-Cookie", cookie.setCookieHeader);
  injectRequestSessionCookie(req, cookie.name, cookie.signedValue);

  next();
}

// ---------------------------------------------------------------------------
// Test-only exports (never import in non-test code)
// ---------------------------------------------------------------------------

/** @internal Exposed for unit tests. Reset the cached JWKS set. */
export function _resetJwksCache(): void {
  _jwks = undefined;
}
