/**
 * config.ts OIDC loading — env-driven `genericOAuth` parity for the old
 * coordinator `--auth-mode=oidc`. Pure unit tests (no DB, no network):
 * loadConfig() takes an explicit env map. NODE_ENV=test placeholders the
 * unrelated required vars so we exercise only the OIDC branch.
 */

import { expect, test, describe } from "bun:test";
import { loadConfig } from "../config.ts";

const BASE = { NODE_ENV: "test" } as Record<string, string | undefined>;

describe("config — OIDC (env-driven)", () => {
  test("OIDC off when ORCHESTRATOR_OIDC_ISSUER unset", () => {
    expect(loadConfig({ ...BASE }).oidc).toBeUndefined();
  });

  test("full config → enabled with sensible defaults + trailing slash stripped", () => {
    const cfg = loadConfig({
      ...BASE,
      ORCHESTRATOR_OIDC_ISSUER: "https://idp.example.com/",
      ORCHESTRATOR_OIDC_CLIENT_ID: "cid",
      ORCHESTRATOR_OIDC_CLIENT_SECRET: "secret",
    });
    expect(cfg.oidc).toEqual({
      issuer: "https://idp.example.com", // trailing slash stripped
      clientId: "cid",
      clientSecret: "secret",
      providerId: "sso",
      scopes: ["openid", "email", "profile"], // profile → display name
    });
  });

  test("provider id + scopes are overridable", () => {
    const cfg = loadConfig({
      ...BASE,
      ORCHESTRATOR_OIDC_ISSUER: "https://idp.example.com",
      ORCHESTRATOR_OIDC_CLIENT_ID: "cid",
      ORCHESTRATOR_OIDC_CLIENT_SECRET: "secret",
      ORCHESTRATOR_OIDC_PROVIDER_ID: "okta",
      ORCHESTRATOR_OIDC_SCOPES: "openid email profile groups",
    });
    expect(cfg.oidc?.providerId).toBe("okta");
    expect(cfg.oidc?.scopes).toEqual(["openid", "email", "profile", "groups"]);
  });

  test("partial config (issuer set, client id/secret missing) is a hard error", () => {
    expect(() =>
      loadConfig({ ...BASE, ORCHESTRATOR_OIDC_ISSUER: "https://idp.example.com" }),
    ).toThrow(/ORCHESTRATOR_OIDC_CLIENT_ID/);
  });
});
