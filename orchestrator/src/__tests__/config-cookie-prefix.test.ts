/**
 * config.ts + better-auth cookie naming (ORCHESTRATOR_COOKIE_PREFIX).
 *
 * Why this exists: an engrams stack running as a session app of another engrams
 * could not log in. The outer preview edge strips any cookie whose name contains
 * `better-auth` in BOTH directions (routes/preview-proxy.ts) — it cannot tell a
 * visitor's outer session token from a nested stack's own cookie of the
 * identical name — so the nested stack never saw its own session and bounced
 * every login back to the form. The nested stack renames its cookie instead.
 */

import { describe, expect, test } from "bun:test";
import { betterAuth } from "better-auth";
import { loadConfig } from "../config.ts";

const BASE = { NODE_ENV: "test" } as Record<string, string | undefined>;

/** The cookie name better-auth would mint for a given `advanced` block. */
async function sessionCookieName(advanced?: Record<string, unknown>): Promise<string> {
  const auth = betterAuth({
    baseURL: "https://web-x.preview.example.com",
    secret: "x".repeat(32),
    ...(advanced ? { advanced } : {}),
  } as never);
  return (await auth.$context).authCookies.sessionToken.name;
}

describe("config — ORCHESTRATOR_COOKIE_PREFIX", () => {
  test("absent by default, so the better-auth default name stands", () => {
    expect(loadConfig({ ...BASE }).cookiePrefix).toBe("");
  });

  test("carried through verbatim when set", () => {
    expect(loadConfig({ ...BASE, ORCHESTRATOR_COOKIE_PREFIX: "engrams-dev" }).cookiePrefix).toBe(
      "engrams-dev",
    );
  });
});

describe("better-auth cookie naming", () => {
  // The whole point: the renamed cookie must not contain the marker the outer
  // preview edge strips on. If better-auth ever stopped honouring the option,
  // nested login would silently break again and this is the tripwire.
  test("a prefix renames the session cookie away from `better-auth`", async () => {
    expect(await sessionCookieName()).toContain("better-auth");
    const renamed = await sessionCookieName({ cookiePrefix: "engrams-dev" });
    expect(renamed).toContain("engrams-dev");
    expect(renamed).not.toContain("better-auth");
  });

  // The two `advanced` members must coexist: prod sets the cookie domain, a
  // nested dev stack sets the prefix, and an earlier version of this wiring
  // would have let one clobber the other.
  test("a prefix and a cross-subdomain domain coexist", async () => {
    const auth = betterAuth({
      baseURL: "https://engrams.example.com",
      secret: "x".repeat(32),
      advanced: {
        crossSubDomainCookies: { enabled: true, domain: ".example.com" },
        cookiePrefix: "engrams-dev",
      },
    } as never);
    const { name, attributes } = (await auth.$context).authCookies.sessionToken;
    expect(name).toContain("engrams-dev");
    expect(attributes.domain).toBe(".example.com");
  });
});
