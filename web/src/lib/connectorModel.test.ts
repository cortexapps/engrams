import { describe, expect, test } from "vitest";

import {
  accessOf,
  defaultDisplayName,
  defaultIconColor,
  defaultIconMono,
  fallbackIdentity,
  humanizeAction,
  parseConnectorConfig,
  resourceLabel,
  resourceOf,
} from "./connectorModel";

describe("identity defaults (mirror of the orchestrator)", () => {
  test("monogram = first two alphanumerics, uppercased", () => {
    expect(defaultIconMono("github")).toBe("GI");
    expect(defaultIconMono("pager_duty")).toBe("PA");
    expect(defaultIconMono("x")).toBe("X");
  });

  test("display name title-cases the provider id", () => {
    expect(defaultDisplayName("datadog")).toBe("Datadog");
    expect(defaultDisplayName("pager_duty")).toBe("Pager Duty");
  });

  test("default tint is a deterministic hex per provider", () => {
    expect(defaultIconColor("github")).toMatch(/^#[0-9a-f]{6}$/i);
    expect(defaultIconColor("github")).toBe(defaultIconColor("github"));
  });

  test("fallback identity is complete", () => {
    const id = fallbackIdentity("sentry");
    expect(id).toEqual({
      provider: "sentry",
      name: "Sentry",
      category: "Other",
      blurb: "",
      icon: { mono: "SE", color: defaultIconColor("sentry") },
    });
  });
});

describe("display derivations", () => {
  test("accessOf reads from the HTTP method", () => {
    expect(accessOf("GET")).toBe("read");
    expect(accessOf("head")).toBe("read");
    expect(accessOf("POST")).toBe("write");
    expect(accessOf(undefined)).toBe("write");
  });

  test("resourceOf strips the read/write suffix", () => {
    expect(resourceOf("pulls:write")).toBe("pulls");
    expect(resourceOf("logs:read")).toBe("logs");
  });

  test("resourceLabel uses the curated map then humanizes", () => {
    expect(resourceLabel("pulls")).toBe("Pull requests");
    expect(resourceLabel("widgets")).toBe("Widgets");
  });

  test("humanizeAction reads slug → 'Verb resource'", () => {
    expect(humanizeAction("issues:write")).toBe("Write issues");
    expect(humanizeAction("contents:read")).toBe("Read contents");
  });
});

describe("parseConnectorConfig", () => {
  const github = JSON.stringify({
    provider: "github",
    protocol: "http",
    display: {
      name: "GitHub",
      category: "Source control",
      blurb: "b",
      icon: { mono: "GH", color: "#1f2328" },
    },
    credential: { source: "mint", mint: { kind: "github_app" } },
    hosts: ["api.github.com"],
    operations: [
      { grants: ["contents:read"], match: { method: "GET", path: "/repos/*/contents/*" } },
      {
        grants: ["pulls:write"],
        match: { method: "POST", path: "/repos/*/pulls" },
        asset: { kind: "pull_request" },
      },
    ],
  });

  test("derives display + powers + mint credential", () => {
    const c = parseConnectorConfig(github, "github");
    expect(c.credentialSource).toBe("mint");
    expect(c.mintKind).toBe("github_app");
    expect(c.hosts).toEqual(["api.github.com"]);
    expect(c.display).toEqual({
      name: "GitHub",
      category: "Source control",
      blurb: "b",
      icon: { mono: "GH", color: "#1f2328" },
    });
    expect(c.capabilities).toEqual([
      { action: "contents:read", access: "read" },
      { action: "pulls:write", access: "write", asset: "pull_request" },
    ]);
  });

  test("defaults display off the provider when absent; inject credential", () => {
    const raw = JSON.stringify({
      provider: "sentry",
      credential: {
        source: "inject",
        injects: [{ header: "Authorization", template: "Bearer {}", secretRef: "sentry-token" }],
      },
      hosts: ["sentry.io"],
      operations: [{ grants: ["issues:read"], match: { method: "GET", path: "/api/0/issues/" } }],
    });
    const c = parseConnectorConfig(raw, "sentry");
    expect(c.credentialSource).toBe("inject");
    expect(c.injects).toEqual([
      { header: "Authorization", template: "Bearer {}", secretRef: "sentry-token" },
    ]);
    expect(c.display.name).toBe("Sentry");
    expect(c.display.icon.mono).toBe("SE");
  });

  test("surfaces ALL injected headers (ADR 0058 multi-credential, e.g. Datadog pup)", () => {
    const raw = JSON.stringify({
      provider: "datadog",
      credential: {
        source: "inject",
        injects: [
          { header: "DD-API-KEY", secretRef: "datadog-api-key", template: "{}" },
          { header: "DD-APPLICATION-KEY", secretRef: "datadog-app-key" },
        ],
      },
      hosts: ["api.datadoghq.com"],
      operations: [{ grants: ["read"], match: { method: "GET", path: "/api/*" } }],
    });
    const c = parseConnectorConfig(raw, "datadog");
    expect(c.injects).toEqual([
      { header: "DD-API-KEY", secretRef: "datadog-api-key", template: "{}" },
      { header: "DD-APPLICATION-KEY", secretRef: "datadog-app-key", template: "{}" },
    ]);
  });

  test("malformed JSON yields an empty inject connector (no throw)", () => {
    const c = parseConnectorConfig("{not json", "x");
    expect(c.credentialSource).toBe("inject");
    expect(c.capabilities).toEqual([]);
    expect(c.hosts).toEqual([]);
  });
});
