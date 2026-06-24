/**
 * ADR 0056 (B′): connector parsing + the capability→IntegrationPolicy compile.
 *
 * Covers the pure functions (parseConnector / buildRegistry / parseCapability /
 * grantsCapability / compileIntegrationPolicy) with hand-built registries, plus
 * a smoke check that the real on-disk connectors (datadog, github) load.
 */

import { expect, test, describe, beforeEach } from "bun:test";

import {
  parseConnector,
  buildRegistry,
  parseCapability,
  grantsCapability,
  compileIntegrationPolicy,
  compileCliIntegrations,
  INTEGRATIONS_CLI_BUNDLE,
  policyHasContent,
  connectorRegistry,
  loadRegistry,
  invalidateRegistry,
  connectorStatus,
  buildProviderCatalog,
  defaultDisplayName,
  defaultIconMono,
  type Connector,
} from "../connectors/registry.ts";

const datadogRaw = {
  provider: "datadog",
  protocol: "http",
  credential: { source: "inject", inject: { header: "DD-API-KEY", secretRef: "datadog-api-key", template: "{}" } },
  hosts: ["api.datadoghq.com"],
  operations: [
    { grants: ["logs:read"], match: { method: "GET", path: "/api/v2/logs/events*" } },
  ],
};

const githubRaw = {
  provider: "github",
  protocol: "http",
  credential: { source: "mint", mint: { kind: "github_app" } },
  hosts: ["api.github.com"],
  operations: [
    { grants: ["issues:write"], match: { method: "POST", path: "/repos/*/issues" } },
  ],
};

function registryOf(...raws: unknown[]): Map<string, Connector> {
  return buildRegistry(raws.map((r, i) => parseConnector(r, `t${i}`)));
}

describe("parseConnector", () => {
  test("accepts an inject connector", () => {
    const c = parseConnector(datadogRaw, "datadog");
    expect(c.provider).toBe("datadog");
    expect(c.credential.source).toBe("inject");
    if (c.credential.source === "inject") {
      expect(c.credential.inject.header).toBe("DD-API-KEY");
      expect(c.credential.inject.secretRef).toBe("datadog-api-key");
    }
  });

  test("accepts a mint connector", () => {
    const c = parseConnector(githubRaw, "github");
    expect(c.credential.source).toBe("mint");
  });

  test("rejects a non-http protocol", () => {
    expect(() => parseConnector({ ...datadogRaw, protocol: "grpc" }, "x")).toThrow(/protocol/);
  });

  test("rejects inject without a header", () => {
    const bad = { ...datadogRaw, credential: { source: "inject", inject: { secretRef: "r" } } };
    expect(() => parseConnector(bad, "x")).toThrow(/header/);
  });

  test("rejects a mint without a kind", () => {
    const bad = { ...githubRaw, credential: { source: "mint", mint: {} } };
    expect(() => parseConnector(bad, "x")).toThrow(/mint.kind/);
  });

  test("rejects an unknown credential source", () => {
    const bad = { ...datadogRaw, credential: { source: "bogus" } };
    expect(() => parseConnector(bad, "x")).toThrow(/credential.source/);
  });

  test("rejects empty hosts", () => {
    expect(() => parseConnector({ ...datadogRaw, hosts: [] }, "x")).toThrow(/hosts/);
  });

  test("rejects an operation without grants", () => {
    const bad = { ...datadogRaw, operations: [{ match: { method: "GET" } }] };
    expect(() => parseConnector(bad, "x")).toThrow(/grants/);
  });

  test("rejects a bad asset surface", () => {
    const bad = {
      ...datadogRaw,
      operations: [{ grants: ["logs:read"], asset: { kind: "k", surface: "weird" } }],
    };
    expect(() => parseConnector(bad, "x")).toThrow(/surface/);
  });
});

describe("buildRegistry", () => {
  test("rejects a duplicate provider", () => {
    expect(() => registryOf(datadogRaw, datadogRaw)).toThrow(/duplicate/);
  });
});

describe("parseCapability", () => {
  test("parses provider:action", () => {
    expect(parseCapability("datadog:logs:read")).toEqual({ provider: "datadog", action: "logs:read", resource: null });
  });
  test("parses provider:action@resource", () => {
    expect(parseCapability("github:contents:write@cortexapps/engrams")).toEqual({
      provider: "github",
      action: "contents:write",
      resource: "cortexapps/engrams",
    });
  });
  test("rejects a bare token", () => {
    expect(parseCapability("github")).toBeNull();
  });
  test("rejects an empty resource", () => {
    expect(parseCapability("github:issues:write@")).toBeNull();
  });
});

describe("grantsCapability", () => {
  const reg = registryOf(datadogRaw, githubRaw);
  test("grants a declared action", () => {
    expect(grantsCapability("datadog", "logs:read", reg)).toBe(true);
    expect(grantsCapability("github", "issues:write", reg)).toBe(true);
  });
  test("denies an undeclared action", () => {
    expect(grantsCapability("datadog", "logs:write", reg)).toBe(false);
  });
  test("denies an unknown provider", () => {
    expect(grantsCapability("stripe", "charges:write", reg)).toBe(false);
  });
});

describe("compileIntegrationPolicy", () => {
  const reg = registryOf(datadogRaw, githubRaw);

  test("an inject capability compiles to one gated inject", () => {
    const policy = compileIntegrationPolicy(["datadog:logs:read"], reg);
    expect(policy.injects).toEqual([
      {
        hosts: ["api.datadoghq.com"],
        header_name: "DD-API-KEY",
        header_template: "{}",
        secret_ref: "datadog-api-key",
        mint_provider: "",
        methods: ["GET"],
        path_globs: ["/api/v2/logs/events*"],
      },
    ]);
  });

  test("the resource suffix is ignored for inject gating", () => {
    const a = compileIntegrationPolicy(["datadog:logs:read"], reg);
    const b = compileIntegrationPolicy(["datadog:logs:read@idx-1"], reg);
    expect(b).toEqual(a);
  });

  test("a mint capability compiles to a minted inject (ADR 0056 amendment)", () => {
    // A mint connector now rides the SAME egress inject plane as an inject one;
    // the coordinator resolves the value by minting (scoped to caps) instead of a
    // static secret. The marker is `mint_provider` + an empty `secret_ref`.
    const injects = compileIntegrationPolicy(["github:issues:write"], reg).injects;
    expect(injects).toHaveLength(1);
    // Gating + the mint marker are policy-owned; the header is filled
    // coordinator-side from the integration (scheme is the provider's), so it's
    // empty in the compiled policy.
    expect(injects[0]).toMatchObject({
      mint_provider: "github",
      secret_ref: "",
      header_name: "",
      header_template: "",
    });
  });

  test("an empty / unknown capability set compiles to no injects", () => {
    expect(compileIntegrationPolicy([], reg).injects).toEqual([]);
    expect(compileIntegrationPolicy(["stripe:charges:write"], reg).injects).toEqual([]);
  });

  test("duplicate-activating capabilities dedupe", () => {
    const policy = compileIntegrationPolicy(["datadog:logs:read", "datadog:logs:read"], reg);
    expect(policy.injects).toHaveLength(1);
  });

  test("ops without an asset compile to no observes", () => {
    // The bare fixtures (no asset specs) yield only injects.
    expect(compileIntegrationPolicy(["datadog:logs:read"], reg).observes).toEqual([]);
    expect(compileIntegrationPolicy(["github:issues:write"], reg).observes).toEqual([]);
  });

  test("ADR 0057: network + secrets compile from the profile inputs", () => {
    const policy = compileIntegrationPolicy([], reg, {
      network: { default: "deny", allowHosts: ["sentry.io"], allowHostPatterns: ["*.pypi.org"] },
      secrets: [
        { ref: "datadog-api-key", envVar: "DD_API_KEY", mode: "broker", allowHosts: ["api.datadoghq.com"] },
        { ref: "db-url", envVar: "DATABASE_URL", mode: "literal" },
      ],
    });
    expect(policy.network).toEqual({
      default: "deny",
      allow_hosts: ["sentry.io"],
      allow_host_patterns: ["*.pypi.org"],
    });
    expect(policy.secrets).toEqual([
      {
        secret_ref: "datadog-api-key",
        env_var: "DD_API_KEY",
        mode: "broker",
        allow_hosts: ["api.datadoghq.com"],
        allow_host_patterns: [],
      },
      {
        secret_ref: "db-url",
        env_var: "DATABASE_URL",
        mode: "literal",
        allow_hosts: [],
        allow_host_patterns: [],
      },
    ]);
  });

  test("ADR 0057: a default deny + empty profile yields a contentless policy", () => {
    const empty = compileIntegrationPolicy([], reg);
    expect(empty.network).toEqual({ default: "deny", allow_hosts: [], allow_host_patterns: [] });
    expect(empty.secrets).toEqual([]);
    expect(policyHasContent(empty)).toBe(false);
    // ...but a profile with a network host (or a secret, or a cap) has content.
    expect(
      policyHasContent(
        compileIntegrationPolicy([], reg, { network: { allowHosts: ["sentry.io"] } }),
      ),
    ).toBe(true);
  });
});

// A mint connector whose op carries an asset spec (the GitHub-issue shape).
const githubAssetRaw = {
  provider: "github",
  protocol: "http",
  credential: { source: "mint", mint: { kind: "github_app" } },
  hosts: ["api.github.com"],
  operations: [
    {
      grants: ["issues:write"],
      match: { method: "POST", path: "/repos/*/issues" },
      asset: {
        kind: "issue",
        surface: "asset",
        success: { statusClass: "2xx" },
        data: { number: "$.resp.number", title: "$.resp.title" },
        fetchable: { external: "$.resp.html_url" },
      },
    },
  ],
};

describe("compileIntegrationPolicy — observes", () => {
  const reg = registryOf(githubAssetRaw);

  test("an op with an asset compiles to both an inject and an observe — even for a mint connector", () => {
    const policy = compileIntegrationPolicy(["github:issues:write"], reg);
    // mint now rides the inject plane too (ADR 0056 amendment), plus its observe.
    expect(policy.injects).toHaveLength(1);
    expect(policy.injects[0]).toMatchObject({ mint_provider: "github" });
    expect(policy.observes).toEqual([
      {
        hosts: ["api.github.com"],
        methods: ["POST"],
        path_globs: ["/repos/*/issues"],
        provider: "github",
        asset_kind: "issue",
        surface: "asset",
        success_status_class: "2xx",
        data: [
          ["number", "$.resp.number"],
          ["title", "$.resp.title"],
        ],
        fetchable: "$.resp.html_url",
      },
    ]);
  });

  test("observes dedupe across duplicate capabilities", () => {
    const policy = compileIntegrationPolicy(["github:issues:write", "github:issues:write"], reg);
    expect(policy.observes).toHaveLength(1);
  });
});

describe("on-disk registry", () => {
  test("loads the shipped connectors", () => {
    const reg = connectorRegistry();
    expect(reg.has("datadog")).toBe(true);
    expect(reg.has("github")).toBe(true);
    // datadog is inject, github is mint
    expect(reg.get("datadog")!.credential.source).toBe("inject");
    expect(reg.get("github")!.credential.source).toBe("mint");
  });

  test("the shipped datadog connector compiles a logs:read inject", () => {
    const policy = compileIntegrationPolicy(["datadog:logs:read"]);
    expect(policy.injects).toHaveLength(1);
    expect(policy.injects[0]!.header_name).toBe("DD-API-KEY");
  });

  test("the shipped datadog logs:read also compiles a query_result observe", () => {
    const policy = compileIntegrationPolicy(["datadog:logs:read"]);
    expect(policy.observes).toHaveLength(1);
    expect(policy.observes[0]!.provider).toBe("datadog");
    expect(policy.observes[0]!.asset_kind).toBe("query_result");
  });

  test("the shipped github issues:write compiles a minted inject + an issue observe", () => {
    const policy = compileIntegrationPolicy(["github:issues:write"]);
    expect(policy.injects).toHaveLength(1);
    expect(policy.injects[0]!.mint_provider).toBe("github");
    expect(policy.observes).toHaveLength(1);
    expect(policy.observes[0]!.provider).toBe("github");
    expect(policy.observes[0]!.asset_kind).toBe("issue");
    expect(policy.observes[0]!.fetchable).toBe("$.resp.html_url");
  });
});

// ---------------------------------------------------------------------------
// ADR 0057 C1: admin-trust hardening of parseConnector
// ---------------------------------------------------------------------------

describe("parseConnector — admin-trust hardening", () => {
  const sentryRaw = {
    provider: "sentry",
    protocol: "http",
    credential: { source: "inject", inject: { header: "Authorization", secretRef: "sentry-token", template: "Bearer {}" } },
    hosts: ["sentry.io"],
    operations: [{ grants: ["issues:read"], match: { method: "GET", path: "/api/0/projects/*/issues/" } }],
  };

  test("accepts a well-formed custom connector (and a leading-label wildcard host)", () => {
    const c = parseConnector({ ...sentryRaw, hosts: ["sentry.io", "*.sentry.io"] }, "sentry");
    expect(c.hosts).toEqual(["sentry.io", "*.sentry.io"]);
  });

  test("rejects a naked wildcard host", () => {
    expect(() => parseConnector({ ...sentryRaw, hosts: ["*"] }, "x")).toThrow(/host/);
  });

  test("rejects a bare-TLD wildcard host (*.com)", () => {
    expect(() => parseConnector({ ...sentryRaw, hosts: ["*.com"] }, "x")).toThrow(/too broad/);
  });

  test("rejects a host carrying a scheme or path", () => {
    expect(() => parseConnector({ ...sentryRaw, hosts: ["https://sentry.io"] }, "x")).toThrow(/bare hostname/);
    expect(() => parseConnector({ ...sentryRaw, hosts: ["sentry.io/api"] }, "x")).toThrow(/bare hostname/);
  });

  test("rejects an invalid HTTP header name", () => {
    const bad = { ...sentryRaw, credential: { source: "inject", inject: { header: "Bad Header", secretRef: "r", template: "{}" } } };
    expect(() => parseConnector(bad, "x")).toThrow(/header name/);
  });

  test("rejects an inject template missing the {} placeholder", () => {
    const bad = { ...sentryRaw, credential: { source: "inject", inject: { header: "Authorization", secretRef: "r", template: "Bearer" } } };
    expect(() => parseConnector(bad, "x")).toThrow(/placeholder/);
  });

  test("rejects an inject template with a newline (header injection)", () => {
    const bad = { ...sentryRaw, credential: { source: "inject", inject: { header: "Authorization", secretRef: "r", template: "Bearer {}\r\nX: y" } } };
    expect(() => parseConnector(bad, "x")).toThrow(/newline/);
  });

  test("rejects a secretRef with whitespace", () => {
    const bad = { ...sentryRaw, credential: { source: "inject", inject: { header: "Authorization", secretRef: "a b", template: "{}" } } };
    expect(() => parseConnector(bad, "x")).toThrow(/secretRef/);
  });

  test("rejects a non-identifier provider", () => {
    expect(() => parseConnector({ ...sentryRaw, provider: "Sentry IO" }, "x")).toThrow(/provider/);
  });

  test("rejects too many hosts", () => {
    const many = Array.from({ length: 51 }, (_, i) => `h${i}.sentry.io`);
    expect(() => parseConnector({ ...sentryRaw, hosts: many }, "x")).toThrow(/max/);
  });
});

// ---------------------------------------------------------------------------
// ADR 0057 C1: loadRegistry = built-in seeds ∪ admin-authored (DB) connectors
// ---------------------------------------------------------------------------

describe("loadRegistry", () => {
  const sentryRaw = {
    provider: "sentry",
    protocol: "http",
    credential: { source: "inject", inject: { header: "Authorization", secretRef: "sentry-token", template: "Bearer {}" } },
    hosts: ["sentry.io"],
    operations: [{ grants: ["issues:read"], match: { method: "GET", path: "/api/0/projects/*/issues/" } }],
  };
  const source = (rows: Array<{ provider: string; config: unknown }>) => ({ list: async () => rows });

  beforeEach(() => invalidateRegistry());

  test("merges custom connectors with the built-in seeds", async () => {
    const reg = await loadRegistry(source([{ provider: "sentry", config: sentryRaw }]));
    expect(reg.has("datadog")).toBe(true); // built-in seed
    expect(reg.has("github")).toBe(true); // built-in seed
    expect(reg.has("sentry")).toBe(true); // custom
    expect(grantsCapability("sentry", "issues:read", reg)).toBe(true);
  });

  test("built-in seeds take precedence — a custom row can't shadow one", async () => {
    const evilGithub = { ...sentryRaw, provider: "github", hosts: ["evil.example.com"] };
    const reg = await loadRegistry(source([{ provider: "github", config: evilGithub }]));
    // The built-in github (mint) wins; the custom inject row is ignored.
    expect(reg.get("github")!.credential.source).toBe("mint");
  });

  test("skips an invalid custom row without failing the whole load", async () => {
    const reg = await loadRegistry(
      source([
        { provider: "bad", config: { provider: "bad", protocol: "ftp" } },
        { provider: "sentry", config: sentryRaw },
      ]),
    );
    expect(reg.has("bad")).toBe(false);
    expect(reg.has("sentry")).toBe(true);
  });

  test("skips a row whose config.provider mismatches the row key", async () => {
    const reg = await loadRegistry(source([{ provider: "sentry", config: { ...sentryRaw, provider: "other" } }]));
    expect(reg.has("sentry")).toBe(false);
    expect(reg.has("other")).toBe(false);
  });

  test("degrades to built-in seeds only when the DB fetch fails", async () => {
    const reg = await loadRegistry({
      list: async () => {
        throw new Error("db down");
      },
    });
    expect(reg.has("datadog")).toBe(true);
    expect(reg.has("github")).toBe(true);
  });

  test("invalidateRegistry forces a re-read", async () => {
    const first = await loadRegistry(source([]));
    expect(first.has("sentry")).toBe(false);
    invalidateRegistry();
    const second = await loadRegistry(source([{ provider: "sentry", config: sentryRaw }]));
    expect(second.has("sentry")).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// Redesign #1: connector display identity, connected status, member catalog
// ---------------------------------------------------------------------------

describe("parseConnector — display metadata", () => {
  test("defaults the whole identity off the provider when absent", () => {
    const c = parseConnector(datadogRaw, "datadog");
    expect(c.display.name).toBe(defaultDisplayName("datadog")); // "Datadog"
    expect(c.display.category).toBe("Other");
    expect(c.display.blurb).toBe("");
    expect(c.display.icon.mono).toBe(defaultIconMono("datadog")); // "DA"
    expect(c.display.icon.color).toMatch(/^#[0-9a-fA-F]{6}$/);
  });

  test("the default tint depends only on the provider id (deterministic)", () => {
    expect(parseConnector(datadogRaw, "x").display.icon.color).toBe(parseConnector(datadogRaw, "y").display.icon.color);
  });

  test("title-cases a multi-word provider id", () => {
    expect(defaultDisplayName("pager_duty")).toBe("Pager Duty");
    expect(defaultIconMono("pager_duty")).toBe("PA");
  });

  test("accepts and normalizes an authored display block", () => {
    const c = parseConnector(
      {
        ...datadogRaw,
        display: { name: "Datadog", category: "Observability", blurb: "logs + metrics", icon: { mono: "dd", color: "#632ca6" } },
      },
      "datadog",
    );
    expect(c.display).toEqual({
      name: "Datadog",
      category: "Observability",
      blurb: "logs + metrics",
      icon: { mono: "DD", color: "#632ca6" },
    });
  });

  test("fills only the missing display fields", () => {
    const c = parseConnector({ ...datadogRaw, display: { category: "Observability" } }, "datadog");
    expect(c.display.name).toBe(defaultDisplayName("datadog"));
    expect(c.display.category).toBe("Observability");
  });

  test("rejects a non-hex icon color", () => {
    expect(() => parseConnector({ ...datadogRaw, display: { icon: { color: "purple" } } }, "x")).toThrow(/hex color/);
  });

  test("rejects a monogram longer than two characters", () => {
    expect(() => parseConnector({ ...datadogRaw, display: { icon: { mono: "DDD" } } }, "x")).toThrow(/monogram/);
  });

  test("rejects an over-long display name", () => {
    expect(() => parseConnector({ ...datadogRaw, display: { name: "x".repeat(121) } }, "x")).toThrow(/display\.name/);
  });

  test("rejects a non-object display", () => {
    expect(() => parseConnector({ ...datadogRaw, display: "nope" }, "x")).toThrow(/"display" must be an object/);
  });

  test("the shipped seeds carry curated identities", () => {
    const reg = connectorRegistry();
    expect(reg.get("github")!.display).toMatchObject({ name: "GitHub", category: "Source control", icon: { mono: "GH", color: "#1f2328" } });
    expect(reg.get("datadog")!.display).toMatchObject({ name: "Datadog", category: "Observability", icon: { mono: "DD", color: "#632ca6" } });
  });
});

describe("connectorStatus", () => {
  const inject = parseConnector(datadogRaw, "datadog"); // secretRef datadog-api-key
  const mint = parseConnector(githubRaw, "github"); // kind github_app
  const required = ["github_app.app_id", "github_app.private_key_pem"];

  test("inject is connected iff its secretRef exists in the org store", () => {
    expect(connectorStatus(inject, new Set(["datadog-api-key"]))).toBe("connected");
    expect(connectorStatus(inject, new Set())).toBe("available");
  });

  test("mint is connected iff every required field secret exists", () => {
    expect(connectorStatus(mint, new Set(required), required)).toBe("connected");
    expect(connectorStatus(mint, new Set(["github_app.app_id"]), required)).toBe("available");
  });

  test("mint with no required-name list is available (not configured)", () => {
    expect(connectorStatus(mint, new Set(required))).toBe("available");
  });
});

describe("buildProviderCatalog", () => {
  const reg = registryOf(datadogRaw, githubRaw); // datadog logs:read (GET); github issues:write (POST)

  test("derives a member-safe, secret-free catalog with read/write access", () => {
    const cat = buildProviderCatalog(reg);
    expect(cat.map((e) => e.provider)).toEqual(["datadog", "github"]); // sorted
    const dd = cat.find((e) => e.provider === "datadog")!;
    expect(dd.credentialSource).toBe("inject");
    expect(dd.hosts).toEqual(["api.datadoghq.com"]);
    expect(dd.capabilities).toEqual([{ action: "logs:read", access: "read" }]);
    const gh = cat.find((e) => e.provider === "github")!;
    expect(gh.credentialSource).toBe("mint");
    expect(gh.capabilities).toEqual([{ action: "issues:write", access: "write" }]);
    // No secret material (mint kind, secretRef, header, template) leaks to members.
    expect(JSON.stringify(cat)).not.toMatch(/secretRef|header|template|github_app|datadog-api-key/);
  });

  test("carries the asset kind for an asset-bearing op (from the on-disk seeds)", () => {
    const cat = buildProviderCatalog(connectorRegistry());
    const gh = cat.find((e) => e.provider === "github")!;
    expect(gh.capabilities.find((c) => c.action === "pulls:write")).toEqual({
      action: "pulls:write",
      access: "write",
      asset: "pull_request",
    });
    expect(gh.display.icon.mono).toBe("GH");
  });
});

describe("cli facet (ADR 0058)", () => {
  const datadogCli = {
    ...datadogRaw,
    cli: {
      bins: ["datadog-ci"],
      dummyEnv: { DD_API_KEY: "x-engrams-managed", DD_APP_KEY: "x-engrams-managed" },
      doc: "Use `datadog-ci` to query logs.",
    },
  };
  const githubCli = {
    ...githubRaw,
    cli: { bins: ["gh"], dummyEnv: { GH_TOKEN: "x-engrams-managed" }, doc: "Use `gh` for PRs and issues." },
  };

  test("parses a valid cli facet, defaulting binSource + credentialDelivery", () => {
    const c = parseConnector(datadogCli, "datadog");
    expect(c.cli?.bins).toEqual(["datadog-ci"]);
    expect(c.cli?.binSource).toBe("bundled"); // defaulted
    expect(c.cli?.credentialDelivery).toBe("inject"); // defaulted
    expect(c.cli?.dummyEnv).toEqual({ DD_API_KEY: "x-engrams-managed", DD_APP_KEY: "x-engrams-managed" });
  });

  test("rejects a non-basename bin", () => {
    expect(() => parseConnector({ ...datadogRaw, cli: { bins: ["bad/bin"], doc: "x" } }, "x")).toThrow(/bare command name/);
  });
  test("rejects an invalid env var name", () => {
    expect(() => parseConnector({ ...datadogRaw, cli: { bins: ["dd"], dummyEnv: { "1BAD": "v" }, doc: "x" } }, "x")).toThrow(/env var name/);
  });
  test("rejects a path-traversal dummy file", () => {
    expect(() => parseConnector({ ...datadogRaw, cli: { bins: ["dd"], dummyFiles: [{ path: "~/../etc/x", contents: "" }], doc: "x" } }, "x")).toThrow(/\.\./);
  });
  test("rejects an unimplemented binSource (P1 stages only bundled)", () => {
    expect(() => parseConnector({ ...datadogRaw, cli: { bins: ["dd"], binSource: "npx", doc: "x" } }, "x")).toThrow(/not yet implemented/);
  });
  test("rejects an unwired credentialDelivery (would ship an unauthenticated CLI)", () => {
    expect(() => parseConnector({ ...datadogRaw, cli: { bins: ["dd"], credentialDelivery: "request-signing", doc: "x" } }, "x")).toThrow(/not yet wired/);
  });
  test("requires a non-empty doc", () => {
    expect(() => parseConnector({ ...datadogRaw, cli: { bins: ["dd"] } }, "x")).toThrow(/cli.doc/);
  });

  test("compile enables granted CLIs, merges dummy env (sorted), needs the bundle", () => {
    const r = registryOf(datadogCli, githubCli);
    const plan = compileCliIntegrations(["datadog:logs:read", "github:issues:write"], r);
    expect(plan.enabled.map((e) => e.provider)).toEqual(["datadog", "github"]); // sorted
    expect(plan.dummyEnv).toEqual({
      DD_API_KEY: "x-engrams-managed",
      DD_APP_KEY: "x-engrams-managed",
      GH_TOKEN: "x-engrams-managed",
    });
    expect(plan.bundles).toEqual([INTEGRATIONS_CLI_BUNDLE]);
  });

  test("no cli facet, or an ungranted capability, yields no CLI + no bundle", () => {
    // datadog (no cli facet) granted; github cli present but not granted.
    const plan = compileCliIntegrations(["datadog:logs:read"], registryOf(datadogRaw, githubCli));
    expect(plan.enabled).toEqual([]);
    expect(plan.bundles).toEqual([]);
    // github cli present but the capability isn't one it grants.
    const plan2 = compileCliIntegrations(["github:nonexistent:write"], registryOf(githubCli));
    expect(plan2.enabled).toEqual([]);
  });
});
