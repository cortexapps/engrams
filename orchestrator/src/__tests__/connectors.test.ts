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
  isGraphqlMatch,
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
  credential: { source: "inject", injects: [{ header: "DD-API-KEY", secretRef: "datadog-api-key", template: "{}" }] },
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

// ADR 0059: a connector mixing REST + GraphQL operations under shared powers.
const githubGraphqlRaw = {
  provider: "github",
  protocol: "http",
  credential: { source: "mint", mint: { kind: "github_app" } },
  hosts: ["api.github.com"],
  graphqlEndpoint: "/graphql",
  operations: [
    { grants: ["pulls:write"], match: { method: "POST", path: "/repos/*/pulls" } },
    { grants: ["pulls:write"], match: { operation: "mutation", field: "mergePullRequest" } },
    {
      grants: ["issues:write"],
      match: { operation: "mutation", field: "createIssue" },
      asset: {
        kind: "issue",
        surface: "asset",
        success: { noGraphqlErrors: true },
        // GraphQL parity: `title` is a fallback chain (response-first, request
        // variables as the supplement); the number is derived from the returned
        // URL when the client didn't select it.
        data: {
          id: "$.resp.data.createIssue.issue.id",
          title: ["$.resp.data.createIssue.issue.title", "$.vars.input.title"],
        },
        fetchable: { external: "$.resp.data.createIssue.issue.url" },
        urlFallback: {
          pattern: "https://github.com/{owner}/{name}/issues/{number:int}",
          fields: { number: "{number}" },
        },
      },
    },
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
      expect(c.credential.injects[0]!.header).toBe("DD-API-KEY");
      expect(c.credential.injects[0]!.secretRef).toBe("datadog-api-key");
    }
  });

  test("accepts a mint connector", () => {
    const c = parseConnector(githubRaw, "github");
    expect(c.credential.source).toBe("mint");
  });

  test("parses a bounded declarative webhook facet", () => {
    const c = parseConnector(
      {
        ...githubRaw,
        webhook: {
          verificationScheme: "github_hmac_sha256",
          events: [{ key: "issues.opened", displayName: "Issue opened" }],
          aliases: [
            { path: "issue.title", alias: "issue.title" },
            { path: "sender.login", alias: "actor.login" },
          ],
        },
      },
      "github",
    );
    expect(c.webhook).toEqual({
      verificationScheme: "github_hmac_sha256",
      events: [{ key: "issues.opened", displayName: "Issue opened" }],
      aliases: [
        { path: "issue.title", alias: "issue.title" },
        { path: "sender.login", alias: "actor.login" },
      ],
    });
  });

  test("rejects invalid webhook facet shapes and drops non-allowlisted fields", () => {
    expect(() =>
      parseConnector(
        {
          ...githubRaw,
          webhook: {
            verificationScheme: "module_ref",
            events: [],
            aliases: [],
            mapper: "./mapper.ts",
          },
        },
        "custom",
      ),
    ).toThrow(/verificationScheme/);

    const withModuleReference = parseConnector(
      {
        ...githubRaw,
        webhook: {
          verificationScheme: "github_hmac_sha256",
          events: [],
          aliases: [],
          mapper: "./mapper.ts",
        },
      },
      "custom",
    );
    expect(Object.keys(withModuleReference.webhook ?? {}).sort()).toEqual([
      "aliases",
      "events",
      "verificationScheme",
    ]);

    expect(() =>
      parseConnector(
        {
          ...githubRaw,
          webhook: {
            verificationScheme: "github_hmac_sha256",
            events: [],
            aliases: [{ path: "issue.title", alias: "raw.issue" }],
          },
        },
        "custom",
      ),
    ).toThrow(/event.raw/);

    for (const alias of ["__proto__.polluted", "constructor.prototype.polluted"]) {
      expect(() =>
        parseConnector(
          {
            ...githubRaw,
            webhook: {
              verificationScheme: "github_hmac_sha256",
              events: [],
              aliases: [{ path: "issue.title", alias }],
            },
          },
          "custom",
        ),
      ).toThrow(/alias/);
    }

    expect(() =>
      parseConnector(
        {
          ...githubRaw,
          webhook: {
            verificationScheme: "github_hmac_sha256",
            events: [{ key: `issue.${"x".repeat(200)}`, displayName: "Too long" }],
            aliases: [],
          },
        },
        "custom",
      ),
    ).toThrow(/key/);

    expect(() =>
      parseConnector(
        {
          ...githubRaw,
          webhook: {
            verificationScheme: "github_hmac_sha256",
            events: [],
            aliases: [
              { path: "issue", alias: "issue" },
              { path: "issue.title", alias: "issue.title" },
            ],
          },
        },
        "custom",
      ),
    ).toThrow(/conflicts/);
  });

  test("rejects a non-http protocol", () => {
    expect(() => parseConnector({ ...datadogRaw, protocol: "grpc" }, "x")).toThrow(/protocol/);
  });

  test("accepts an optional test probe path (ADR 0058)", () => {
    const c = parseConnector({ ...datadogRaw, test: { path: "/api/v1/dashboard" } }, "datadog");
    expect(c.test?.path).toBe("/api/v1/dashboard");
  });

  test("a connector without a test facet leaves it undefined (default `/` coord-side)", () => {
    expect(parseConnector(datadogRaw, "datadog").test).toBeUndefined();
  });

  test("rejects a test path that does not start with /", () => {
    expect(() => parseConnector({ ...datadogRaw, test: { path: "api/v1/dashboard" } }, "x")).toThrow(/test.path/);
  });

  test("rejects inject without a header", () => {
    const bad = { ...datadogRaw, credential: { source: "inject", injects: [{ secretRef: "r" }] } };
    expect(() => parseConnector(bad, "x")).toThrow(/header/);
  });

  test("rejects an inject credential with an empty injects array", () => {
    const bad = { ...datadogRaw, credential: { source: "inject", injects: [] } };
    expect(() => parseConnector(bad, "x")).toThrow(/non-empty array/);
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

  test("rejects malformed asset data extractor values", () => {
    const withData = (data: unknown) => ({
      ...datadogRaw,
      operations: [{ grants: ["logs:read"], asset: { kind: "k", surface: "asset", data } }],
    });
    expect(() => parseConnector(withData({ f: [] }), "x")).toThrow(/asset\.data\.f/);
    expect(() => parseConnector(withData({ f: [42] }), "x")).toThrow(/asset\.data\.f/);
    expect(() => parseConnector(withData({ f: "" }), "x")).toThrow(/asset\.data\.f/);
    // Both the single-path and chain forms parse.
    const c = parseConnector(withData({ a: "$.resp.a", b: ["$.resp.b", "$.vars.b"] }), "x");
    expect(c.operations[0]!.asset?.data).toEqual({ a: "$.resp.a", b: ["$.resp.b", "$.vars.b"] });
  });

  test("rejects a urlFallback with a bad pattern or an undeclared capture", () => {
    const withFallback = (urlFallback: unknown) => ({
      ...datadogRaw,
      operations: [
        { grants: ["logs:read"], asset: { kind: "k", surface: "asset", urlFallback } },
      ],
    });
    expect(() => parseConnector(withFallback({ fields: {} }), "x")).toThrow(/pattern/);
    expect(() => parseConnector(withFallback({ pattern: "https://x/{a}", fields: [] }), "x")).toThrow(/fields/);
    // A field template referencing a capture the pattern doesn't declare is a
    // silent no-derive at runtime — the loader rejects the typo up front.
    expect(() =>
      parseConnector(withFallback({ pattern: "https://x/{a}", fields: { f: "{typo}" } }), "x"),
    ).toThrow(/\{typo\}/);
    // The valid shape parses and is carried on the op.
    const c = parseConnector(
      withFallback({ pattern: "https://x/{a}/{n:int}", fields: { f: "{a}", n: "{n}" } }),
      "x",
    );
    expect(c.operations[0]!.asset?.urlFallback).toEqual({
      pattern: "https://x/{a}/{n:int}",
      fields: { f: "{a}", n: "{n}" },
    });
  });

  // ADR 0059: GraphQL operations.
  test("accepts a GraphQL match + defaults graphqlEndpoint to /graphql", () => {
    const c = parseConnector(githubGraphqlRaw, "github");
    expect(c.graphqlEndpoint).toBe("/graphql");
    const gqlOp = c.operations.find((o) => o.match && isGraphqlMatch(o.match));
    expect(gqlOp).toBeDefined();
    if (gqlOp?.match && isGraphqlMatch(gqlOp.match)) {
      expect(gqlOp.match.operation).toBe("mutation");
      expect(gqlOp.match.field).toBe("mergePullRequest");
    }
  });

  test("a pure-REST connector leaves graphqlEndpoint undefined", () => {
    expect(parseConnector(githubRaw, "github").graphqlEndpoint).toBeUndefined();
  });

  test("honors an explicit graphqlEndpoint", () => {
    const c = parseConnector({ ...githubGraphqlRaw, graphqlEndpoint: "/api/graphql" }, "github");
    expect(c.graphqlEndpoint).toBe("/api/graphql");
  });

  test("rejects a graphqlEndpoint that isn't an absolute path", () => {
    expect(() => parseConnector({ ...githubGraphqlRaw, graphqlEndpoint: "graphql" }, "x")).toThrow(/graphqlEndpoint/);
  });

  test("rejects a match mixing HTTP and GraphQL shapes", () => {
    const bad = { ...githubRaw, operations: [{ grants: ["x"], match: { method: "POST", operation: "mutation", field: "f" } }] };
    expect(() => parseConnector(bad, "x")).toThrow(/not both/);
  });

  test("rejects a bad GraphQL operation type", () => {
    const bad = { ...githubRaw, operations: [{ grants: ["x"], match: { operation: "subscribe", field: "f" } }] };
    expect(() => parseConnector(bad, "x")).toThrow(/match\.operation/);
  });

  test("rejects a GraphQL field that isn't a valid name", () => {
    const bad = { ...githubRaw, operations: [{ grants: ["x"], match: { operation: "mutation", field: "no spaces" } }] };
    expect(() => parseConnector(bad, "x")).toThrow(/match\.field/);
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
        mint_source: null,
        methods: ["GET"],
        path_globs: ["/api/v2/logs/events*"],
        graphql_operation: "",
        graphql_field: "",
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
    // static secret. The typed mint source accompanies an empty `secret_ref`.
    const injects = compileIntegrationPolicy(["github:issues:write"], reg).injects;
    expect(injects).toHaveLength(1);
    // Gating + the mint marker are policy-owned; the header is filled
    // coordinator-side from the integration (scheme is the provider's), so it's
    // empty in the compiled policy.
    expect(injects[0]).toMatchObject({
      mint_source: { kind: "provider", provider: "github" },
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

  test("ADR 0057: a granted power opens egress to its connector's hosts", () => {
    // No profile-typed allow-list — the host must come from the granted cap, or
    // the agent gets a DNS "could not resolve host" despite injection being wired.
    const policy = compileIntegrationPolicy(["datadog:logs:read"], reg);
    expect(policy.network.allow_hosts).toEqual(["api.datadoghq.com"]);
    // A cap whose host can be reached is itself policy content.
    expect(policyHasContent(policy)).toBe(true);
  });

  test("ADR 0057: granted-power hosts union with the profile's hand-typed allow-list (deduped, admin first)", () => {
    const policy = compileIntegrationPolicy(["datadog:logs:read", "github:issues:write"], reg, {
      // `api.datadoghq.com` is typed AND granted — must appear once, not twice.
      network: { default: "deny", allowHosts: ["sentry.io", "api.datadoghq.com"] },
    });
    expect(policy.network.allow_hosts).toEqual(["sentry.io", "api.datadoghq.com", "api.github.com"]);
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
    expect(policy.injects[0]).toMatchObject({
      mint_source: { kind: "provider", provider: "github" },
    });
    expect(policy.observes).toEqual([
      {
        hosts: ["api.github.com"],
        methods: ["POST"],
        path_globs: ["/repos/*/issues"],
        provider: "github",
        asset_kind: "issue",
        surface: "asset",
        success_status_class: "2xx",
        success_no_graphql_errors: false,
        graphql_operation: "",
        graphql_field: "",
        data: [
          ["number", "$.resp.number"],
          ["title", "$.resp.title"],
        ],
        fetchable: "$.resp.html_url",
        url_fallback: null,
      },
    ]);
  });

  test("observes dedupe across duplicate capabilities", () => {
    const policy = compileIntegrationPolicy(["github:issues:write", "github:issues:write"], reg);
    expect(policy.observes).toHaveLength(1);
  });
});

describe("compileIntegrationPolicy — GraphQL (ADR 0059)", () => {
  const reg = registryOf(githubGraphqlRaw);

  test("a GraphQL mint op compiles to a POST /graphql inject gated by operation+field", () => {
    const policy = compileIntegrationPolicy(["github:pulls:write"], reg);
    const gql = policy.injects.find((i) => i.graphql_field === "mergePullRequest");
    expect(gql).toMatchObject({
      mint_source: { kind: "provider", provider: "github" },
      methods: ["POST"],
      path_globs: ["/graphql"],
      graphql_operation: "mutation",
      graphql_field: "mergePullRequest",
    });
  });

  test("the same power flows to BOTH the REST and GraphQL surfaces", () => {
    // The core ADR 0059 property: granting `pulls:write` opens the REST endpoint
    // AND the GraphQL mutation — one inject per surface.
    const policy = compileIntegrationPolicy(["github:pulls:write"], reg);
    const paths = policy.injects.map((i) => i.path_globs.join(","));
    expect(paths).toContain("/repos/*/pulls");
    expect(paths).toContain("/graphql");
    expect(policy.injects).toHaveLength(2);
  });

  test("a GraphQL asset op compiles to a graphql observe with the noGraphqlErrors rule", () => {
    const policy = compileIntegrationPolicy(["github:issues:write"], reg);
    expect(policy.observes).toHaveLength(1);
    expect(policy.observes[0]).toMatchObject({
      provider: "github",
      asset_kind: "issue",
      methods: ["POST"],
      path_globs: ["/graphql"],
      graphql_operation: "mutation",
      graphql_field: "createIssue",
      success_no_graphql_errors: true,
      success_status_class: null,
    });
    // A chained data value flattens to repeated [field, path] pairs in order
    // (the proxy takes the first that resolves) — the wire shape is unchanged.
    expect(policy.observes[0]!.data).toEqual([
      ["id", "$.resp.data.createIssue.issue.id"],
      ["title", "$.resp.data.createIssue.issue.title"],
      ["title", "$.vars.input.title"],
    ]);
    // GraphQL parity: the URL fallback compiles to the snake_case wire shape.
    expect(policy.observes[0]!.url_fallback).toEqual({
      pattern: "https://github.com/{owner}/{name}/issues/{number:int}",
      fields: [["number", "{number}"]],
    });
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
    expect(reg.get("github")!.webhook?.events.some((event) => event.key === "issues.opened")).toBe(true);
    expect(reg.get("slack")!.webhook?.verificationScheme).toBe("slack_v0");
  });

  test("the shipped datadog connector compiles BOTH pup injects (api + app key)", () => {
    const policy = compileIntegrationPolicy(["datadog:metrics:read"]);
    // metrics:read activates several ops (query, metric metadata, v2 query); each
    // emits one inject per header (DD-API-KEY + DD-APPLICATION-KEY) gated to that
    // op's path — so assert on the unique header/secret SET, not the entry count.
    const headers = [...new Set(policy.injects.map((i) => i.header_name))].sort();
    expect(headers).toEqual(["DD-API-KEY", "DD-APPLICATION-KEY"]);
    const refs = [...new Set(policy.injects.map((i) => i.secret_ref))].sort();
    expect(refs).toEqual(["datadog-api-key", "datadog-app-key"]);
  });

  test("the shipped github issues:write compiles minted injects + issue observes (REST + GraphQL)", () => {
    const policy = compileIntegrationPolicy(["github:issues:write"]);
    // issues:write activates several gated ops — the REST create/edit/comment/label
    // endpoints AND the GraphQL createIssue/updateIssue/… mutations (ADR 0059); each
    // emits a minted inject for github.
    expect(policy.injects.length).toBeGreaterThan(0);
    expect(policy.injects.every((i) =>
      i.mint_source?.kind === "provider" && i.mint_source.provider === "github"
    )).toBe(true);
    // Two issue assets are observed: the REST create (POST /repos/*/issues, gated by
    // the 2xx status) and the GraphQL createIssue mutation (gated by noGraphqlErrors).
    expect(policy.observes.every((o) => o.provider === "github" && o.asset_kind === "issue")).toBe(true);
    const rest = policy.observes.find((o) => o.path_globs.includes("/repos/*/issues") && !o.graphql_field);
    expect(rest?.fetchable).toBe("$.resp.html_url");
    expect(rest?.success_status_class).toBe("2xx");
    const gql = policy.observes.find((o) => o.graphql_field === "createIssue");
    expect(gql?.success_no_graphql_errors).toBe(true);
  });

  test("the shipped github createPullRequest observe reaches REST parity (vars + URL fallback)", () => {
    // The gh regression: `gh pr create` selects only `pullRequest { id url }`,
    // so the shipped GraphQL asset must source title/branches from the request
    // variables and derive repo/number from the returned PR URL.
    const policy = compileIntegrationPolicy(["github:pulls:write"]);
    const gql = policy.observes.find((o) => o.graphql_field === "createPullRequest");
    expect(gql).toBeDefined();
    // title/branches are response-first fallback chains: `gh` selects only
    // id+url (vars fills them), while a client that inlines its arguments but
    // selects the fields still gets the response values (PR #904 review).
    const pathsFor = (field: string) => gql!.data.filter(([k]) => k === field).map(([, p]) => p);
    expect(pathsFor("title")).toEqual([
      "$.resp.data.createPullRequest.pullRequest.title",
      "$.vars.input.title",
    ]);
    expect(pathsFor("head_branch")).toEqual([
      "$.resp.data.createPullRequest.pullRequest.headRefName",
      "$.vars.input.headRefName",
    ]);
    expect(pathsFor("base_branch")).toEqual([
      "$.resp.data.createPullRequest.pullRequest.baseRefName",
      "$.vars.input.baseRefName",
    ]);
    expect(gql!.url_fallback).toEqual({
      pattern: "https://github.com/{owner}/{name}/pull/{number:int}",
      fields: [
        ["repo", "{owner}/{name}"],
        ["number", "{number}"],
      ],
    });
    // Same-field REST parity: the REST create observe extracts the same keys.
    const rest = policy.observes.find((o) => o.path_globs.includes("/repos/*/pulls"));
    expect(new Set(rest!.data.map(([k]) => k))).toEqual(new Set(gql!.data.map(([k]) => k)));
  });
});

// ---------------------------------------------------------------------------
// ADR 0057 C1: admin-trust hardening of parseConnector
// ---------------------------------------------------------------------------

describe("parseConnector — admin-trust hardening", () => {
  const sentryRaw = {
    provider: "sentry",
    protocol: "http",
    credential: { source: "inject", injects: [{ header: "Authorization", secretRef: "sentry-token", template: "Bearer {}" }] },
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
    const bad = { ...sentryRaw, credential: { source: "inject", injects: [{ header: "Bad Header", secretRef: "r", template: "{}" }] } };
    expect(() => parseConnector(bad, "x")).toThrow(/header name/);
  });

  test("rejects an inject template missing the {} placeholder", () => {
    const bad = { ...sentryRaw, credential: { source: "inject", injects: [{ header: "Authorization", secretRef: "r", template: "Bearer" }] } };
    expect(() => parseConnector(bad, "x")).toThrow(/placeholder/);
  });

  test("rejects an inject template with a newline (header injection)", () => {
    const bad = { ...sentryRaw, credential: { source: "inject", injects: [{ header: "Authorization", secretRef: "r", template: "Bearer {}\r\nX: y" }] } };
    expect(() => parseConnector(bad, "x")).toThrow(/newline/);
  });

  test("rejects a secretRef with whitespace", () => {
    const bad = { ...sentryRaw, credential: { source: "inject", injects: [{ header: "Authorization", secretRef: "a b", template: "{}" }] } };
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
  // A fictional provider that is NOT one of the shipped on-disk seeds, so these
  // tests exercise the custom (DB) merge path without colliding with a built-in.
  const customRaw = {
    provider: "customco",
    protocol: "http",
    credential: { source: "inject", injects: [{ header: "Authorization", secretRef: "customco-token", template: "Bearer {}" }] },
    hosts: ["api.customco.example"],
    operations: [{ grants: ["issues:read"], match: { method: "GET", path: "/api/0/projects/*/issues/" } }],
  };
  const source = (rows: Array<{ provider: string; config: unknown }>) => ({ list: async () => rows });

  beforeEach(() => invalidateRegistry());

  test("merges custom connectors with the built-in seeds", async () => {
    const reg = await loadRegistry(source([{ provider: "customco", config: customRaw }]));
    expect(reg.has("datadog")).toBe(true); // built-in seed
    expect(reg.has("github")).toBe(true); // built-in seed
    expect(reg.has("customco")).toBe(true); // custom
    expect(grantsCapability("customco", "issues:read", reg)).toBe(true);
  });

  test("built-in seeds take precedence — a custom row can't shadow one", async () => {
    const evilGithub = { ...customRaw, provider: "github", hosts: ["evil.example.com"] };
    const reg = await loadRegistry(source([{ provider: "github", config: evilGithub }]));
    // The built-in github (mint) wins; the custom inject row is ignored.
    expect(reg.get("github")!.credential.source).toBe("mint");
  });

  test("skips an invalid custom row without failing the whole load", async () => {
    const reg = await loadRegistry(
      source([
        { provider: "bad", config: { provider: "bad", protocol: "ftp" } },
        { provider: "customco", config: customRaw },
      ]),
    );
    expect(reg.has("bad")).toBe(false);
    expect(reg.has("customco")).toBe(true);
  });

  test("skips a row whose config.provider mismatches the row key", async () => {
    const reg = await loadRegistry(source([{ provider: "customco", config: { ...customRaw, provider: "other" } }]));
    expect(reg.has("customco")).toBe(false);
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
    expect(first.has("customco")).toBe(false);
    invalidateRegistry();
    const second = await loadRegistry(source([{ provider: "customco", config: customRaw }]));
    expect(second.has("customco")).toBe(true);
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
      bins: ["pup"],
      dummyEnv: { DD_API_KEY: "x-engrams-managed", DD_APP_KEY: "x-engrams-managed" },
      doc: "Use `pup` to query Datadog.",
    },
  };
  const githubCli = {
    ...githubRaw,
    cli: { bins: ["gh"], dummyEnv: { GH_TOKEN: "x-engrams-managed" }, doc: "Use `gh` for PRs and issues." },
  };
  // ADR 0058 UB2: a connector whose CLI is an admin-uploaded binary (mount_catalog
  // bundle "acme-cli"), not a bundled one.
  const uploadedCli = {
    ...datadogRaw,
    cli: { bins: ["acme"], binSource: "uploaded", bundle: "acme-cli", doc: "Use `acme` to query Acme." },
  };

  test("parses a valid cli facet, defaulting binSource + credentialDelivery", () => {
    const c = parseConnector(datadogCli, "datadog");
    expect(c.cli?.bins).toEqual(["pup"]);
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
  test("rejects an npx binSource (still a later arm)", () => {
    expect(() => parseConnector({ ...datadogRaw, cli: { bins: ["dd"], binSource: "npx", doc: "x" } }, "x")).toThrow(/not yet implemented/);
  });
  test("UB2: accepts an uploaded binSource with a catalog bundle", () => {
    const c = parseConnector(uploadedCli, "datadog");
    expect(c.cli?.binSource).toBe("uploaded");
    expect(c.cli?.bundle).toBe("acme-cli");
  });
  test("UB2: rejects uploaded without a bundle", () => {
    expect(() => parseConnector({ ...datadogRaw, cli: { bins: ["acme"], binSource: "uploaded", doc: "x" } }, "x")).toThrow(/cli.bundle.*required/);
  });
  test("UB2: rejects a bundle on a non-uploaded binSource", () => {
    expect(() => parseConnector({ ...datadogRaw, cli: { bins: ["dd"], bundle: "acme-cli", doc: "x" } }, "x")).toThrow(/only valid when/);
  });
  test("UB2: rejects a malformed bundle name", () => {
    expect(() => parseConnector({ ...datadogRaw, cli: { bins: ["acme"], binSource: "uploaded", bundle: "Bad Name", doc: "x" } }, "x")).toThrow(/cli.bundle/);
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

  test("the built-in slack seed ships the slack CLI + an auth.test gate", () => {
    const slack = connectorRegistry().get("slack")!;
    expect(slack.cli?.bins).toEqual(["slack"]);
    expect(slack.cli?.binSource).toBe("bundled");
    expect(slack.cli?.dummyEnv).toEqual({ SLACK_TOKEN: "x-engrams-managed" });
    // `slack whoami` hits /api/auth.test — gated so the token is injected there too.
    expect(slack.operations.some((o) => o.match && !isGraphqlMatch(o.match) && o.match.path === "/api/auth.test")).toBe(true);
  });

  test("the slack seed requests the ADR-0060 trigger scopes", () => {
    const scopes = connectorRegistry().get("slack")!.oauth!.scopes;
    // The five scopes the external-triggers reverse channel needs (in addition
    // to the existing post/read/upload set): receive the @mention, read thread
    // replies (history, not just metadata), react for the acks, and read the
    // email for identity matching.
    for (const s of [
      "app_mentions:read",
      "channels:history",
      "groups:history",
      "reactions:write",
      "users:read.email",
    ]) {
      expect(scopes).toContain(s);
    }
  });

  test("granting a slack power enables the slack CLI + the shared bundle", () => {
    const plan = compileCliIntegrations(["slack:chat:write"], connectorRegistry());
    const slack = plan.enabled.find((e) => e.provider === "slack");
    expect(slack?.bins).toEqual(["slack"]);
    expect(plan.dummyEnv.SLACK_TOKEN).toBe("x-engrams-managed");
    expect(plan.bundles).toContain(INTEGRATIONS_CLI_BUNDLE);
  });

  test("UB2: an uploaded CLI routes BOTH the shared bundle and its own catalog bundle", () => {
    const plan = compileCliIntegrations(["datadog:logs:read"], registryOf(uploadedCli));
    expect(plan.enabled.map((e) => e.provider)).toEqual(["datadog"]);
    expect(plan.enabled[0]!.bins).toEqual(["acme"]);
    // The shared discovery bundle (helper + SKILL.md) AND the uploaded binary's
    // own catalog bundle both ride selected_skills (deduped, one slot each).
    expect(plan.bundles.sort()).toEqual([INTEGRATIONS_CLI_BUNDLE, "acme-cli"].sort());
  });

  test("UB2: one uploaded bundle backing two granted connectors mounts once", () => {
    const other = {
      ...githubRaw,
      cli: { bins: ["acme2"], binSource: "uploaded", bundle: "acme-cli", doc: "Acme via GitHub host." },
    };
    const plan = compileCliIntegrations(["datadog:logs:read", "github:issues:write"], registryOf(uploadedCli, other));
    // acme-cli appears once despite backing two connectors; integrations-cli once.
    expect(plan.bundles.sort()).toEqual([INTEGRATIONS_CLI_BUNDLE, "acme-cli"].sort());
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

  test("the on-disk github + datadog connectors expose their cli facet", () => {
    const plan = compileCliIntegrations(["github:pulls:write", "datadog:metrics:read"], connectorRegistry());
    expect(plan.enabled.map((e) => e.provider).sort()).toEqual(["datadog", "github"]);
    // WS4: the github connector's dummy token is GITHUB_TOKEN (not GH_TOKEN) so it
    // stays outside bufgen's `GH_TOKEN:-GITHUB_PASSWORD` password chain.
    expect(plan.dummyEnv.GITHUB_TOKEN).toBe("x-engrams-managed");
    expect(plan.dummyEnv.GH_TOKEN).toBeUndefined();
    expect(plan.dummyEnv.DD_API_KEY).toBe("x-engrams-managed");
    expect(plan.dummyEnv.DD_APP_KEY).toBe("x-engrams-managed");
    expect(plan.enabled.find((e) => e.provider === "datadog")?.bins).toEqual(["pup"]);
    expect(plan.bundles).toEqual([INTEGRATIONS_CLI_BUNDLE]);
    // gh's doc carries the PR-open guidance.
    expect(plan.enabled.find((e) => e.provider === "github")?.doc).toMatch(/gh pr create/);
  });
});
