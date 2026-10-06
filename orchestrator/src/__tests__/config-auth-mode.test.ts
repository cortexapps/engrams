/**
 * config.ts auth-mode loading — the ONE sign-in door (ORCHESTRATOR_AUTH_MODE)
 * and the per-mode config it requires. Pure unit tests (no DB, no network):
 * loadConfig() takes an explicit env map. NODE_ENV=test placeholders the
 * unrelated required vars so only the auth branch is exercised.
 */

import { expect, test, describe } from "bun:test";
import { loadConfig } from "../config.ts";

const BASE = { NODE_ENV: "test" } as Record<string, string | undefined>;

const OAUTH = {
  ...BASE,
  ORCHESTRATOR_AUTH_MODE: "oauth",
  ORCHESTRATOR_OAUTH_ISSUER: "https://idp.example.com/",
  ORCHESTRATOR_OAUTH_CLIENT_ID: "cid",
  ORCHESTRATOR_OAUTH_CLIENT_SECRET: "secret",
  ORCHESTRATOR_OAUTH_ALLOWED_DOMAINS: "example.com",
};

describe("config — auth mode", () => {
  test("unset → password mode with sign-up open (the dev default)", () => {
    const cfg = loadConfig({ ...BASE });
    expect(cfg.authMode).toBe("password");
    expect(cfg.passwordSignup).toBe(true);
    expect(cfg.oauth).toBeUndefined();
    expect(cfg.iapAudiences).toEqual([]);
  });

  test("an unknown mode is a hard error", () => {
    expect(() => loadConfig({ ...BASE, ORCHESTRATOR_AUTH_MODE: "none" })).toThrow(
      /ORCHESTRATOR_AUTH_MODE="none" must be one of oauth, iap, password/,
    );
  });
});

describe("config — password mode", () => {
  test.each(["0", "false"])("ORCHESTRATOR_AUTH_PASSWORD_SIGNUP=%s closes sign-up", (v) => {
    const cfg = loadConfig({ ...BASE, ORCHESTRATOR_AUTH_PASSWORD_SIGNUP: v });
    expect(cfg.authMode).toBe("password");
    expect(cfg.passwordSignup).toBe(false);
  });

  test("sign-up is never open outside password mode", () => {
    expect(loadConfig({ ...OAUTH, ORCHESTRATOR_AUTH_PASSWORD_SIGNUP: "true" }).passwordSignup).toBe(
      false,
    );
    expect(
      loadConfig({
        ...BASE,
        ORCHESTRATOR_AUTH_MODE: "iap",
        IAP_AUDIENCES: "/projects/1/global/backendServices/2",
        ORCHESTRATOR_AUTH_PASSWORD_SIGNUP: "true",
      }).passwordSignup,
    ).toBe(false);
  });
});

describe("config — oauth mode", () => {
  test("full config → provider with sensible defaults + trailing slash stripped", () => {
    expect(loadConfig(OAUTH).oauth).toEqual({
      issuer: "https://idp.example.com", // trailing slash stripped
      clientId: "cid",
      clientSecret: "secret",
      providerId: "sso",
      scopes: ["openid", "email", "profile"], // profile → display name
      displayName: "SSO",
      allowlist: { domains: ["example.com"], emails: [] },
    });
  });

  test("the Google issuer labels the button Google", () => {
    const cfg = loadConfig({ ...OAUTH, ORCHESTRATOR_OAUTH_ISSUER: "https://accounts.google.com" });
    expect(cfg.oauth?.displayName).toBe("Google");
  });

  test("provider id, scopes and display name are overridable", () => {
    const cfg = loadConfig({
      ...OAUTH,
      ORCHESTRATOR_OAUTH_PROVIDER_ID: "okta",
      ORCHESTRATOR_OAUTH_SCOPES: "openid email profile groups",
      ORCHESTRATOR_OAUTH_DISPLAY_NAME: "Okta",
    });
    expect(cfg.oauth?.providerId).toBe("okta");
    expect(cfg.oauth?.scopes).toEqual(["openid", "email", "profile", "groups"]);
    expect(cfg.oauth?.displayName).toBe("Okta");
  });

  test("missing issuer / client id / client secret is a hard error naming each", () => {
    expect(() =>
      loadConfig({
        ...BASE,
        ORCHESTRATOR_AUTH_MODE: "oauth",
        ORCHESTRATOR_OAUTH_ALLOWED_DOMAINS: "example.com",
      }),
    ).toThrow(
      /ORCHESTRATOR_OAUTH_ISSUER.*ORCHESTRATOR_OAUTH_CLIENT_ID.*ORCHESTRATOR_OAUTH_CLIENT_SECRET/s,
    );
  });

  test("an empty allowlist is a hard error — an OAuth client is a public door", () => {
    expect(() => loadConfig({ ...OAUTH, ORCHESTRATOR_OAUTH_ALLOWED_DOMAINS: undefined })).toThrow(
      /ORCHESTRATOR_OAUTH_ALLOWED_DOMAINS or ORCHESTRATOR_OAUTH_ALLOWED_EMAILS/,
    );
  });

  test("bootstrap admins are always on the allowlist, and satisfy the requirement", () => {
    const cfg = loadConfig({
      ...OAUTH,
      ORCHESTRATOR_OAUTH_ALLOWED_DOMAINS: undefined,
      ORCHESTRATOR_ADMIN_EMAILS: "Root@Example.com",
    });
    expect(cfg.oauth?.allowlist).toEqual({ domains: [], emails: ["root@example.com"] });
  });

  test("the allowlist is normalised: case, whitespace, a leading @, duplicates", () => {
    const cfg = loadConfig({
      ...OAUTH,
      ORCHESTRATOR_OAUTH_ALLOWED_DOMAINS: " @Example.com , example.com, corp.io ",
      ORCHESTRATOR_OAUTH_ALLOWED_EMAILS: "Contractor@Other.dev",
    });
    expect(cfg.oauth?.allowlist).toEqual({
      domains: ["example.com", "corp.io"],
      emails: ["contractor@other.dev"],
    });
  });

  test("provider config in another mode is a hard error", () => {
    expect(() =>
      loadConfig({ ...BASE, ORCHESTRATOR_OAUTH_ISSUER: "https://idp.example.com" }),
    ).toThrow(/ORCHESTRATOR_OAUTH_ISSUER is set but ORCHESTRATOR_AUTH_MODE is "password"/);
  });
});

describe("config — iap mode", () => {
  test("audiences parse as a set", () => {
    const cfg = loadConfig({
      ...BASE,
      ORCHESTRATOR_AUTH_MODE: "iap",
      IAP_AUDIENCES: "/projects/1/global/backendServices/2, /projects/1/global/backendServices/3",
    });
    expect(cfg.authMode).toBe("iap");
    expect(cfg.iapAudiences).toEqual([
      "/projects/1/global/backendServices/2",
      "/projects/1/global/backendServices/3",
    ]);
  });

  test("iap mode with no audience is a hard error", () => {
    expect(() => loadConfig({ ...BASE, ORCHESTRATOR_AUTH_MODE: "iap" })).toThrow(
      /IAP_AUDIENCES \(required when ORCHESTRATOR_AUTH_MODE=iap\)/,
    );
  });

  // The bridge fails closed whenever the set is non-empty. A stray audience in
  // another mode would 401 every anonymous request — the login page included —
  // so it must stop the boot instead of arming the bridge.
  test.each(["oauth", "password"])("IAP_AUDIENCES in %s mode is a hard error", (mode) => {
    const env = mode === "oauth" ? OAUTH : { ...BASE, ORCHESTRATOR_AUTH_MODE: mode };
    expect(() =>
      loadConfig({ ...env, IAP_AUDIENCES: "/projects/1/global/backendServices/2" }),
    ).toThrow(/IAP_AUDIENCES is set but ORCHESTRATOR_AUTH_MODE is/);
  });
});
