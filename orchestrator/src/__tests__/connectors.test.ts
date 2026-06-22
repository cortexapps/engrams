/**
 * ADR 0056 (B′): connector parsing + the capability→IntegrationPolicy compile.
 *
 * Covers the pure functions (parseConnector / buildRegistry / parseCapability /
 * grantsCapability / compileIntegrationPolicy) with hand-built registries, plus
 * a smoke check that the real on-disk connectors (datadog, github) load.
 */

import { expect, test, describe } from "bun:test";

import {
  parseConnector,
  buildRegistry,
  parseCapability,
  grantsCapability,
  compileIntegrationPolicy,
  connectorRegistry,
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
        methods: ["GET"],
        path_prefixes: ["/api/v2/logs/events"],
      },
    ]);
  });

  test("the resource suffix is ignored for inject gating", () => {
    const a = compileIntegrationPolicy(["datadog:logs:read"], reg);
    const b = compileIntegrationPolicy(["datadog:logs:read@idx-1"], reg);
    expect(b).toEqual(a);
  });

  test("a mint capability compiles to no injects (Phase 5)", () => {
    expect(compileIntegrationPolicy(["github:issues:write"], reg).injects).toEqual([]);
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

  test("an op with an asset compiles to an observe — even for a mint connector", () => {
    const policy = compileIntegrationPolicy(["github:issues:write"], reg);
    expect(policy.injects).toEqual([]); // mint → no inject
    expect(policy.observes).toEqual([
      {
        hosts: ["api.github.com"],
        methods: ["POST"],
        path_prefixes: ["/repos/"],
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

  test("the shipped github issues:write compiles an issue observe (mint, no inject)", () => {
    const policy = compileIntegrationPolicy(["github:issues:write"]);
    expect(policy.injects).toEqual([]);
    expect(policy.observes).toHaveLength(1);
    expect(policy.observes[0]!.provider).toBe("github");
    expect(policy.observes[0]!.asset_kind).toBe("issue");
    expect(policy.observes[0]!.fetchable).toBe("$.resp.html_url");
  });
});
