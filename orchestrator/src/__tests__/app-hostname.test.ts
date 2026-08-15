/**
 * Session-app hostnames + peer-address env (ADR 0118).
 *
 * The properties that matter: the composed `<app>-<session slug>` label is
 * always a legal DNS label so the wildcard certificate covers it; the env stem
 * survives a hyphenated app name; and `${…}` interpolation substitutes what it
 * knows without destroying what it does not.
 */

import { expect, test, describe } from "bun:test";

import {
  appHostLabel,
  appUrl,
  defaultAppName,
  generateSessionSlug,
  isValidAppName,
  isValidHostLabel,
  schemeFor,
  MAX_APP_NAME_LENGTH,
  MAX_HOST_LABEL_LENGTH,
} from "../apps/hostname.ts";
import { buildIngressEnv, envStem, interpolateEnv, normalizeApps } from "../apps/env.ts";
import type { SessionAppRow } from "../db/session-apps.ts";

function row(name: string, hostLabel: string): SessionAppRow {
  return {
    hostLabel,
    sessionId: "s1",
    name,
    port: 3000,
    ownerUserId: "u1",
    visibility: "org",
    createdAt: new Date(0),
  };
}

describe("session slug generator", () => {
  test("generates an adjective-adjective-noun triple of lowercase words", () => {
    for (let i = 0; i < 200; i++) {
      const slug = generateSessionSlug();
      const parts = slug.split("-");
      expect(parts.length).toBe(3);
      for (const p of parts) expect(p).toMatch(/^[a-z]+$/);
      expect(isValidHostLabel(slug)).toBe(true);
    }
  });

  test("has enough entropy to rarely collide across a handful of mints", () => {
    const seen = new Set<string>();
    for (let i = 0; i < 50; i++) seen.add(generateSessionSlug());
    // 50 mints from ~150k combinations: collisions are very unlikely. Allow a
    // tiny margin so this never flakes (the store's unique index is the real
    // guard).
    expect(seen.size).toBeGreaterThanOrEqual(48);
  });
});

describe("host labels", () => {
  test("the longest legal app name still composes a legal DNS label", () => {
    // This is the property the whole naming scheme rests on: the wildcard
    // certificate covers exactly ONE label under the base domain, so
    // `<app>-<slug>` must never exceed 63 characters.
    const longest = "a".repeat(MAX_APP_NAME_LENGTH);
    expect(isValidAppName(longest)).toBe(true);
    for (let i = 0; i < 200; i++) {
      const label = appHostLabel(longest, generateSessionSlug());
      expect(label.length).toBeLessThanOrEqual(MAX_HOST_LABEL_LENGTH);
      expect(isValidHostLabel(label)).toBe(true);
    }
  });

  test("a hyphenated app name still composes a legal label", () => {
    const label = appHostLabel("brain-api", "tidy-swift-otters");
    expect(label).toBe("brain-api-tidy-swift-otters");
    expect(isValidHostLabel(label)).toBe(true);
  });

  test("isValidAppName rejects what cannot be a DNS label", () => {
    expect(isValidAppName("Api")).toBe(false); // uppercase
    expect(isValidAppName("-leading")).toBe(false);
    expect(isValidAppName("trailing-")).toBe(false);
    expect(isValidAppName("has.dot")).toBe(false);
    expect(isValidAppName("has space")).toBe(false);
    expect(isValidAppName("")).toBe(false);
    expect(isValidAppName("a".repeat(MAX_APP_NAME_LENGTH + 1))).toBe(false);
    expect(isValidAppName("api")).toBe(true);
    expect(isValidAppName("port-3000")).toBe(true);
  });

  test("an ad-hoc exposure is an app with a derived name", () => {
    expect(defaultAppName(3000)).toBe("port-3000");
    expect(isValidAppName(defaultAppName(3000))).toBe(true);
  });

  test("local dev domains are http, everything else https", () => {
    expect(schemeFor("lvh.me:8787")).toBe("http");
    expect(schemeFor("localhost:8787")).toBe("http");
    expect(schemeFor("preview.example.com")).toBe("https");
    expect(appUrl("api-tidy-swift-otters", "preview.example.com")).toBe(
      "https://api-tidy-swift-otters.preview.example.com",
    );
  });
});

describe("normalizeApps", () => {
  test("lowercases and trims, and drops rather than throws", () => {
    const { specs, rejected } = normalizeApps([
      { name: "  Web  ", port: 3000 },
      { name: "bad name", port: 8080 },
      { name: "api", port: 0 },
      { name: "ok", port: 8080 },
    ]);
    expect(specs).toEqual([
      { name: "web", port: 3000 },
      { name: "ok", port: 8080 },
    ]);
    expect(rejected.map((r) => r.name)).toEqual(["bad name", "api"]);
  });

  test("drops duplicate names and duplicate ports", () => {
    const { specs, rejected } = normalizeApps([
      { name: "web", port: 3000 },
      { name: "web", port: 4000 },
      { name: "other", port: 3000 },
    ]);
    expect(specs).toEqual([{ name: "web", port: 3000 }]);
    expect(rejected).toEqual([
      { name: "web", reason: "duplicate app name" },
      { name: "other", reason: "duplicate port 3000" },
    ]);
  });
});

describe("peer-address env", () => {
  test("every app gets both a bare host and a URL", () => {
    const env = buildIngressEnv(
      [row("web", "web-tidy-swift-otters"), row("api", "api-tidy-swift-otters")],
      "preview.example.com",
    );
    expect(env).toEqual({
      WEB_INGRESS_HOST: "web-tidy-swift-otters.preview.example.com",
      WEB_INGRESS_URL: "https://web-tidy-swift-otters.preview.example.com",
      API_INGRESS_HOST: "api-tidy-swift-otters.preview.example.com",
      API_INGRESS_URL: "https://api-tidy-swift-otters.preview.example.com",
    });
  });

  test("the env stem survives a hyphenated app name", () => {
    expect(envStem("brain-api")).toBe("BRAIN_API");
    expect(envStem("port-3000")).toBe("PORT_3000");
    expect(envStem("web")).toBe("WEB");
  });
});

describe("interpolateEnv", () => {
  const vars = {
    WEB_INGRESS_URL: "https://web-x.preview.example.com",
    API_INGRESS_HOST: "api-x.preview.example.com",
  };

  test("substitutes a peer's address into any variable name", () => {
    // The point of the whole model: the CORS allowlist lives on the API and
    // wants the FRONTEND's address.
    const { env } = interpolateEnv(
      {
        CORS_ALLOWED_ORIGINS: "${WEB_INGRESS_URL}",
        COOKIE_DOMAIN: "${API_INGRESS_HOST}",
      },
      vars,
    );
    expect(env.CORS_ALLOWED_ORIGINS).toBe("https://web-x.preview.example.com");
    expect(env.COOKIE_DOMAIN).toBe("api-x.preview.example.com");
  });

  test("substitutes more than once inside one value", () => {
    const { env } = interpolateEnv(
      { ALLOWED: "${WEB_INGRESS_URL},${API_INGRESS_HOST}" },
      vars,
    );
    expect(env.ALLOWED).toBe("https://web-x.preview.example.com,api-x.preview.example.com");
  });

  test("leaves an unknown reference VERBATIM rather than blanking it", () => {
    // Env values legitimately carry shell syntax; emptying one would be worse
    // than leaving it for the shell to resolve.
    const { env, unresolved } = interpolateEnv(
      { PATH_LIKE: "${HOME}/bin", PASSWORD: "a$b{c}" },
      vars,
    );
    expect(env.PATH_LIKE).toBe("${HOME}/bin");
    expect(env.PASSWORD).toBe("a$b{c}");
    expect(unresolved).toEqual([]);
  });

  test("reports an ingress-shaped reference that resolves to nothing", () => {
    // That shape is always a typo or a renamed app, so the caller logs it.
    const { env, unresolved } = interpolateEnv(
      { A: "${WBE_INGRESS_URL}", B: "${GONE_INGRESS_HOST}" },
      vars,
    );
    expect(env.A).toBe("${WBE_INGRESS_URL}");
    expect(unresolved.sort()).toEqual(["GONE_INGRESS_HOST", "WBE_INGRESS_URL"]);
  });
});
