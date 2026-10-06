/**
 * /api/v1/auth-config tests (bun test).
 *
 * The endpoint exposes the public auth posture to the SPA login page. It reads
 * `config` live per-request, so we set the auth fields on the `config`
 * singleton (a plain object, not frozen — same pattern as iap-bridge.test.ts)
 * to exercise every door. No DB or network required.
 */

import { expect, test, describe, afterEach } from "bun:test";
import authConfigRoute from "../routes/auth-config.ts";
import { config, type Config } from "../config.ts";

type AuthFields = Pick<Config, "authMode" | "passwordSignup" | "oauth">;

function setAuth(fields: AuthFields): void {
  Object.assign(config, fields);
}

async function getAuthConfig(): Promise<unknown> {
  const res = await authConfigRoute.request("/api/v1/auth-config");
  expect(res.status).toBe(200);
  return res.json();
}

describe("GET /api/v1/auth-config", () => {
  const saved: AuthFields = {
    authMode: config.authMode,
    passwordSignup: config.passwordSignup,
    oauth: config.oauth,
  };
  afterEach(() => setAuth(saved));

  test("password mode, sign-up open → the form + the sign-up toggle", async () => {
    setAuth({ authMode: "password", passwordSignup: true, oauth: undefined });
    expect(await getAuthConfig()).toEqual({
      mode: "password",
      passwordAuth: true,
      signup: true,
      previewBaseDomain: "lvh.me:8787",
    });
  });

  test("password mode, sign-up closed → the form without the toggle", async () => {
    setAuth({ authMode: "password", passwordSignup: false, oauth: undefined });
    expect(await getAuthConfig()).toEqual({
      mode: "password",
      passwordAuth: true,
      signup: false,
      previewBaseDomain: "lvh.me:8787",
    });
  });

  test("oauth mode → the provider button; no password door; no allowlist leak", async () => {
    setAuth({
      authMode: "oauth",
      passwordSignup: false,
      oauth: {
        issuer: "https://accounts.google.com",
        clientId: "cid",
        clientSecret: "secret",
        providerId: "sso",
        scopes: ["openid", "email", "profile"],
        displayName: "Google",
        allowlist: { domains: ["example.com"], emails: ["root@example.com"] },
      },
    });
    // Exact equality: nothing but the provider id + label may leave the server.
    expect(await getAuthConfig()).toEqual({
      mode: "oauth",
      oauth: { providerId: "sso", displayName: "Google" },
      passwordAuth: false,
      signup: false,
      previewBaseDomain: "lvh.me:8787",
    });
  });

  test("iap mode → no door on the page", async () => {
    setAuth({ authMode: "iap", passwordSignup: false, oauth: undefined });
    expect(await getAuthConfig()).toEqual({
      mode: "iap",
      passwordAuth: false,
      signup: false,
      previewBaseDomain: "lvh.me:8787",
    });
  });
});
