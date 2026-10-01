/**
 * The sign-in door, end to end (auth/sign-in-door.ts).
 *
 * Each test drives a REAL better-auth instance — built from `signInDoor()`,
 * the same function better-auth.ts uses — through `auth.handler`, against a
 * fake OIDC provider on loopback and an in-memory database. No Postgres and no
 * network beyond loopback, so the suite runs in the plain `bun test` lane.
 *
 * What it proves:
 *   - `oauth` mode: the browser flow signs an allowed account in (and creates
 *     the user on first sign-in); the gate refuses every other account BEFORE
 *     a user or a session exists; the password door answers 400.
 *   - The IAP → OAuth migration: a user the IAP bridge created signs in
 *     through OAuth as the SAME user.
 *   - `password` mode: sign-up follows `passwordSignup`; no OAuth route exists.
 *   - `iap` mode: neither door exists.
 */

import { afterAll, beforeAll, beforeEach, describe, expect, test } from "bun:test";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { betterAuth } from "better-auth";
import { memoryAdapter, type MemoryDB } from "better-auth/adapters/memory";
import type { OAuthConfig } from "../config.ts";
import {
  oauthProviderConfig,
  signInDoor,
  type SignInDoorConfig,
} from "../auth/sign-in-door.ts";

const BASE_URL = "http://localhost:8787";
const LOGIN_URL = `${BASE_URL}/login`;

// ---------------------------------------------------------------------------
// Fake OIDC provider: discovery + token endpoint. The token endpoint returns
// an ID token carrying whatever claims the test set in `nextClaims`.
// ---------------------------------------------------------------------------

let idp: Server;
let issuer: string;
let nextClaims: Record<string, unknown> = {};
/** The form body of the last token request — to assert the code + PKCE verifier arrive. */
let lastTokenRequest: URLSearchParams | undefined;

function b64url(obj: unknown): string {
  return Buffer.from(JSON.stringify(obj)).toString("base64url");
}

/** An ID token. better-auth reads its claims after the back-channel code
 * exchange and does not check the signature (OIDC Core 3.1.3.7 permits that
 * for a token received directly from the token endpoint), so the signature
 * segment here is a placeholder. */
function idToken(claims: Record<string, unknown>): string {
  return `${b64url({ alg: "RS256", typ: "JWT" })}.${b64url({ iss: issuer, aud: "cid", ...claims })}.sig`;
}

beforeAll(async () => {
  idp = createServer((req, res) => {
    const url = new URL(req.url ?? "/", "http://idp");
    if (url.pathname === "/.well-known/openid-configuration") {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          issuer,
          authorization_endpoint: `${issuer}/authorize`,
          token_endpoint: `${issuer}/token`,
          userinfo_endpoint: `${issuer}/userinfo`,
        }),
      );
      return;
    }
    if (url.pathname === "/token" && req.method === "POST") {
      let body = "";
      req.on("data", (c: Buffer) => (body += c.toString()));
      req.on("end", () => {
        lastTokenRequest = new URLSearchParams(body);
        res.writeHead(200, { "content-type": "application/json" });
        res.end(
          JSON.stringify({
            access_token: "at",
            token_type: "Bearer",
            expires_in: 3600,
            id_token: idToken(nextClaims),
          }),
        );
      });
      return;
    }
    res.writeHead(404).end();
  });
  await new Promise<void>((resolve) => idp.listen(0, "127.0.0.1", resolve));
  issuer = `http://127.0.0.1:${(idp.address() as AddressInfo).port}`;
});

afterAll(async () => {
  await new Promise<void>((resolve) => idp.close(() => resolve()));
});

// ---------------------------------------------------------------------------
// A real better-auth instance for one door, over an in-memory database.
// ---------------------------------------------------------------------------

type Row = Record<string, unknown>;

function buildAuth(cfg: SignInDoorConfig) {
  const db: MemoryDB = { user: [], session: [], account: [], verification: [] };
  const door = signInDoor(cfg, LOGIN_URL);
  const auth = betterAuth({
    baseURL: BASE_URL,
    secret: "test-only-secret-test-only-secret-test-only",
    database: memoryAdapter(db),
    emailAndPassword: door.emailAndPassword,
    plugins: door.plugins,
  });
  return { auth, db };
}

function oauthConfig(overrides: Partial<OAuthConfig> = {}): OAuthConfig {
  return {
    issuer,
    clientId: "cid",
    clientSecret: "secret",
    providerId: "sso",
    scopes: ["openid", "email", "profile"],
    displayName: "SSO",
    allowlist: { domains: ["corp.com"], emails: [] },
    ...overrides,
  };
}

/** `name=value` pairs from a response's Set-Cookie headers, as a Cookie header. */
function cookiesOf(res: Response): string {
  return res.headers
    .getSetCookie()
    .map((c) => c.split(";")[0])
    .join("; ");
}

function hasSessionCookie(res: Response): boolean {
  return res.headers.getSetCookie().some((c) => c.startsWith("better-auth.session_token="));
}

/**
 * Run the browser flow: start sign-in, then come back from the provider with a
 * code. Returns the callback response (a 302) and the authorization URL.
 */
async function signInWithOAuth(
  auth: ReturnType<typeof buildAuth>["auth"],
  claims: Record<string, unknown>,
): Promise<{ callback: Response; authorizeUrl: URL }> {
  nextClaims = claims;
  const start = await auth.handler(
    new Request(`${BASE_URL}/api/auth/sign-in/oauth2`, {
      method: "POST",
      headers: { "content-type": "application/json", origin: BASE_URL },
      body: JSON.stringify({ providerId: "sso", callbackURL: "/", errorCallbackURL: "/login" }),
    }),
  );
  expect(start.status).toBe(200);
  const { url } = (await start.json()) as { url: string };
  const authorizeUrl = new URL(url);
  const state = authorizeUrl.searchParams.get("state");
  expect(state).toBeTruthy();

  const callback = await auth.handler(
    new Request(`${BASE_URL}/api/auth/oauth2/callback/sso?code=the-code&state=${state}`, {
      headers: { cookie: cookiesOf(start) },
    }),
  );
  return { callback, authorizeUrl };
}

// ---------------------------------------------------------------------------
// oauth mode
// ---------------------------------------------------------------------------

describe("oauth mode — the browser flow", () => {
  let auth: ReturnType<typeof buildAuth>["auth"];
  let db: MemoryDB;
  let oauth: OAuthConfig;

  beforeEach(() => {
    oauth = oauthConfig();
    ({ auth, db } = buildAuth({ authMode: "oauth", passwordSignup: false, oauth }));
    lastTokenRequest = undefined;
  });

  test("an allowed account signs in; the user is created on first sign-in", async () => {
    const { callback, authorizeUrl } = await signInWithOAuth(auth, {
      sub: "idp-1",
      email: "Alice@Corp.com",
      email_verified: true,
      name: "Alice A",
    });

    // The authorization request goes to the provider with PKCE and our
    // registered redirect URI.
    expect(authorizeUrl.origin).toBe(issuer);
    expect(authorizeUrl.searchParams.get("client_id")).toBe("cid");
    expect(authorizeUrl.searchParams.get("redirect_uri")).toBe(
      `${BASE_URL}/api/auth/oauth2/callback/sso`,
    );
    expect(authorizeUrl.searchParams.get("code_challenge")).toBeTruthy();
    expect(authorizeUrl.searchParams.get("scope")).toBe("openid email profile");

    // The code is exchanged on the back channel, with the PKCE verifier.
    expect(lastTokenRequest?.get("code")).toBe("the-code");
    expect(lastTokenRequest?.get("code_verifier")).toBeTruthy();

    expect(callback.status).toBe(302);
    expect(callback.headers.get("location")).toBe("/");
    expect(hasSessionCookie(callback)).toBe(true);

    expect(db["user"]).toHaveLength(1);
    const user = db["user"]![0] as Row;
    expect(user["email"]).toBe("alice@corp.com");
    expect(user["emailVerified"]).toBe(true);
    expect(user["name"]).toBe("Alice A");
    expect(db["session"]).toHaveLength(1);
    expect((db["account"]![0] as Row)["providerId"]).toBe("sso");
  });

  test("a provider that sends no name still signs in, named after the mailbox", async () => {
    const { callback } = await signInWithOAuth(auth, {
      sub: "idp-2",
      email: "bob@corp.com",
      email_verified: true,
    });
    expect(callback.headers.get("location")).toBe("/");
    expect((db["user"]![0] as Row)["name"]).toBe("bob");
  });

  test("an account outside the allowlist is refused: no user, no session, no cookie", async () => {
    const { callback } = await signInWithOAuth(auth, {
      sub: "idp-3",
      email: "mallory@evil.com",
      email_verified: true,
      name: "Mallory",
    });
    expect(callback.status).toBe(302);
    expect(callback.headers.get("location")).toBe(`${LOGIN_URL}?error=account_not_allowed`);
    expect(hasSessionCookie(callback)).toBe(false);
    expect(db["user"]).toHaveLength(0);
    expect(db["session"]).toHaveLength(0);
    expect(db["account"]).toHaveLength(0);
  });

  test("an unverified email is refused, even at an allowed domain", async () => {
    const { callback } = await signInWithOAuth(auth, {
      sub: "idp-4",
      email: "alice@corp.com",
      email_verified: false,
      name: "Not Alice",
    });
    expect(callback.headers.get("location")).toBe(`${LOGIN_URL}?error=email_not_verified`);
    expect(hasSessionCookie(callback)).toBe(false);
    expect(db["user"]).toHaveLength(0);
  });

  test("the gate runs for a RETURNING user: leaving the allowlist ends access", async () => {
    const claims = { sub: "idp-5", email: "carol@corp.com", email_verified: true, name: "Carol" };
    const first = await signInWithOAuth(auth, claims);
    expect(hasSessionCookie(first.callback)).toBe(true);
    expect(db["session"]).toHaveLength(1);

    // The deployment narrows its allowlist; Carol's user row still exists.
    oauth.allowlist.domains = ["other.com"];
    const second = await signInWithOAuth(auth, claims);
    expect(second.callback.headers.get("location")).toBe(`${LOGIN_URL}?error=account_not_allowed`);
    expect(hasSessionCookie(second.callback)).toBe(false);
    expect(db["session"]).toHaveLength(1); // no new session
  });

  test("a callback with a forged state is refused before the code is exchanged", async () => {
    nextClaims = { sub: "idp-6", email: "dave@corp.com", email_verified: true, name: "Dave" };
    const res = await auth.handler(
      new Request(`${BASE_URL}/api/auth/oauth2/callback/sso?code=the-code&state=forged`),
    );
    expect(res.status).toBe(302);
    expect(res.headers.get("location")).toContain("error=");
    expect(hasSessionCookie(res)).toBe(false);
    expect(lastTokenRequest).toBeUndefined();
    expect(db["user"]).toHaveLength(0);
  });
});

describe("oauth mode — the IAP → OAuth migration", () => {
  test("a user the IAP bridge created signs in as the SAME user", async () => {
    const { auth, db } = buildAuth({
      authMode: "oauth",
      passwordSignup: false,
      oauth: oauthConfig(),
    });
    // Exactly what iap-bridge.ts jitCreateSession writes: a verified user with
    // no `account` row.
    const ctx = await auth.$context;
    const iapUser = await ctx.internalAdapter.createUser({
      email: "erin@corp.com",
      name: "erin",
      emailVerified: true,
    });

    const { callback } = await signInWithOAuth(auth, {
      sub: "idp-7",
      email: "erin@corp.com",
      email_verified: true,
      name: "Erin E",
    });
    expect(callback.headers.get("location")).toBe("/");
    expect(hasSessionCookie(callback)).toBe(true);

    // One user, now linked to the provider — not a second account that would
    // strand the person's tasks, profiles and keys on the old row.
    expect(db["user"]).toHaveLength(1);
    expect((db["session"]![0] as Row)["userId"]).toBe(iapUser.id);
    expect((db["account"]![0] as Row)["userId"]).toBe(iapUser.id);
  });

  // A password-era account never proved its email. Linking the first OAuth
  // sign-in to it would hand the real owner an account someone else may have
  // seeded (and may still hold a key for), so better-auth refuses.
  test("an UNVERIFIED existing user is not linked", async () => {
    const { auth, db } = buildAuth({
      authMode: "oauth",
      passwordSignup: false,
      oauth: oauthConfig(),
    });
    const ctx = await auth.$context;
    await ctx.internalAdapter.createUser({
      email: "frank@corp.com",
      name: "frank",
      emailVerified: false,
    });

    const { callback } = await signInWithOAuth(auth, {
      sub: "idp-8",
      email: "frank@corp.com",
      email_verified: true,
      name: "Frank",
    });
    expect(callback.headers.get("location")).toContain("error=account_not_linked");
    expect(hasSessionCookie(callback)).toBe(false);
    expect(db["session"]).toHaveLength(0);
    expect(db["account"]).toHaveLength(0);
  });
});

describe("oauth mode — the Google provider", () => {
  const GOOGLE = "https://accounts.google.com";

  test("asks for the account chooser and hints the one allowed domain", () => {
    const provider = oauthProviderConfig(oauthConfig({ issuer: GOOGLE }), LOGIN_URL);
    expect(provider.discoveryUrl).toBe(`${GOOGLE}/.well-known/openid-configuration`);
    expect(provider.prompt).toBe("select_account");
    expect(provider.authorizationUrlParams).toEqual({ hd: "corp.com" });
    expect(provider.pkce).toBe(true);
  });

  // The hint narrows Google's account chooser to ONE domain, so it is only
  // sent when one domain is the whole allowlist.
  test.each([
    ["two domains", { domains: ["corp.com", "corp.io"], emails: [] }],
    ["the wildcard", { domains: ["*"], emails: [] }],
    ["emails only", { domains: [], emails: ["a@corp.com"] }],
  ])("sends no domain hint for %s", (_label, allowlist) => {
    const provider = oauthProviderConfig(oauthConfig({ issuer: GOOGLE, allowlist }), LOGIN_URL);
    expect(provider.authorizationUrlParams).toBeUndefined();
    expect(provider.prompt).toBe("select_account");
  });

  test("another provider gets neither: not every provider accepts select_account", () => {
    const provider = oauthProviderConfig(oauthConfig(), LOGIN_URL);
    expect(provider.prompt).toBeUndefined();
    expect(provider.authorizationUrlParams).toBeUndefined();
    expect(provider.pkce).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// The password door, per mode
// ---------------------------------------------------------------------------

async function signUp(auth: ReturnType<typeof buildAuth>["auth"], email: string): Promise<Response> {
  return auth.handler(
    new Request(`${BASE_URL}/api/auth/sign-up/email`, {
      method: "POST",
      headers: { "content-type": "application/json", origin: BASE_URL },
      body: JSON.stringify({ email, password: "correct-horse-battery", name: "X" }),
    }),
  );
}

async function signIn(auth: ReturnType<typeof buildAuth>["auth"], email: string): Promise<Response> {
  return auth.handler(
    new Request(`${BASE_URL}/api/auth/sign-in/email`, {
      method: "POST",
      headers: { "content-type": "application/json", origin: BASE_URL },
      body: JSON.stringify({ email, password: "correct-horse-battery" }),
    }),
  );
}

describe("the password door", () => {
  test("oauth mode: sign-up and sign-in are both refused, and nothing is created", async () => {
    const { auth, db } = buildAuth({ authMode: "oauth", passwordSignup: false, oauth: oauthConfig() });
    const up = await signUp(auth, "anyone@anywhere.io");
    expect(up.status).toBe(400);
    expect(hasSessionCookie(up)).toBe(false);
    expect((await signIn(auth, "anyone@anywhere.io")).status).toBe(400);
    expect(db["user"]).toHaveLength(0);
    expect(db["session"]).toHaveLength(0);
  });

  test("iap mode: no password door and no OAuth route", async () => {
    const { auth, db } = buildAuth({ authMode: "iap", passwordSignup: false, oauth: undefined });
    expect((await signUp(auth, "anyone@anywhere.io")).status).toBe(400);
    expect(db["user"]).toHaveLength(0);
    const oauthStart = await auth.handler(
      new Request(`${BASE_URL}/api/auth/sign-in/oauth2`, {
        method: "POST",
        headers: { "content-type": "application/json", origin: BASE_URL },
        body: JSON.stringify({ providerId: "sso", callbackURL: "/" }),
      }),
    );
    expect(oauthStart.status).toBe(404);
  });

  test("password mode, sign-up open: registration works; no OAuth route", async () => {
    const { auth, db } = buildAuth({ authMode: "password", passwordSignup: true, oauth: undefined });
    const up = await signUp(auth, "dev@example.com");
    expect(up.status).toBe(200);
    expect(hasSessionCookie(up)).toBe(true);
    expect(db["user"]).toHaveLength(1);
    const oauthStart = await auth.handler(
      new Request(`${BASE_URL}/api/auth/sign-in/oauth2`, {
        method: "POST",
        headers: { "content-type": "application/json", origin: BASE_URL },
        body: JSON.stringify({ providerId: "sso", callbackURL: "/" }),
      }),
    );
    expect(oauthStart.status).toBe(404);
  });

  test("password mode, sign-up closed: an existing account signs in; nobody can make a new one", async () => {
    // One database, two instances: the account is made while sign-up is open,
    // then the deployment closes it.
    const open = buildAuth({ authMode: "password", passwordSignup: true, oauth: undefined });
    expect((await signUp(open.auth, "dev@example.com")).status).toBe(200);

    const closedDoor = signInDoor(
      { authMode: "password", passwordSignup: false, oauth: undefined },
      LOGIN_URL,
    );
    const closed = betterAuth({
      baseURL: BASE_URL,
      secret: "test-only-secret-test-only-secret-test-only",
      database: memoryAdapter(open.db),
      emailAndPassword: closedDoor.emailAndPassword,
      plugins: closedDoor.plugins,
    });

    const up = await signUp(closed, "newcomer@example.com");
    expect(up.status).toBe(400);
    expect(hasSessionCookie(up)).toBe(false);
    expect(open.db["user"]).toHaveLength(1);

    const back = await signIn(closed, "dev@example.com");
    expect(back.status).toBe(200);
    expect(hasSessionCookie(back)).toBe(true);
  });
});
