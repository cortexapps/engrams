import { describe, expect, test } from "bun:test";
import { validateMcpConfig } from "../mcp/config.ts";
import { reviewCatalog } from "../mcp/catalog.ts";
import { canonicalDigest } from "../mcp/hash.ts";
import { compileBindings, assertBindingCurrent } from "../mcp/bindings.ts";
import { validateMcpGrant } from "../mcp/types.ts";

const config = {
  endpoint: "https://tools.example.test/mcp",
  zone: "public",
  auth: { kind: "api_key" as const, secretRef: "mcp.service.api-key" },
};
const tool = {
  upstreamName: "Search.Documents",
  description: "Search documents",
  inputSchema: { type: "object" },
  classification: "read" as const,
  enabled: true,
};
const state = {
  configRevision: 1,
  authGeneration: 1,
  credentialIdentity: "account-a",
  catalogRevision: 1,
  revoked: false,
};
const grant = {
  connectionId: "c1",
  operation: "invoke",
  credentialScope: "org" as const,
  resourceConstraints: [],
  mcp: {
    upstreamName: tool.upstreamName,
    automationWrite: false,
    argumentConstraints: {},
    unrestricted: true,
  },
};

describe("MCP domain", () => {
  test("accepts canonical endpoint and references only", () => {
    expect(validateMcpConfig(config)).toEqual(config);
    expect(
      validateMcpConfig({ ...config, endpoint: "https://tools/mcp", zone: "internal" }).zone,
    ).toBe("internal");
    expect(() =>
      validateMcpConfig({ ...config, endpoint: "https://bad..example.test/mcp" }),
    ).toThrow();
    for (const endpoint of [
      "http://tools.example.test/mcp",
      "https://127.0.0.1/mcp",
      "https://[::1]/mcp",
      "https://u:p@tools.example.test/mcp",
      "https://tools.example.test/mcp?q=1",
      "https://tools.example.test/mcp#x",
      "https://tools.example.test:443/mcp",
    ]) {
      expect(() => validateMcpConfig({ ...config, endpoint })).toThrow();
    }
    expect(() => validateMcpConfig({ ...config, token: "secret" })).toThrow();
    expect(() =>
      validateMcpConfig({
        ...config,
        auth: { kind: "api_key" as const, secretRef: "unrelated.key" },
      }),
    ).toThrow();
  });
  test("requires explicit tool scope and unrestricted selection", () => {
    expect(() => validateMcpGrant(grant)).not.toThrow();
    expect(() => validateMcpGrant({ ...grant, credentialScope: "user" })).toThrow();
    expect(() =>
      validateMcpGrant({ ...grant, mcp: { ...grant.mcp, unrestricted: false } }),
    ).toThrow();
    expect(() => validateMcpGrant({ ...grant, operation: tool.upstreamName })).toThrow();
  });
  test("catalog fingerprints bind upstream schema and explicit review", () => {
    const reviewed = reviewCatalog([tool]);
    expect(reviewed[0]!.fingerprint).toBe(canonicalDigest(tool));
    expect(() => reviewCatalog([{ ...tool, classification: undefined }])).toThrow();
    expect(() => reviewCatalog([tool, tool])).toThrow();
    expect(() =>
      reviewCatalog([{ ...tool, inputSchema: { $ref: "https://example.test/schema" } }]),
    ).toThrow();
  });
  test("freezes names and authority and rejects live changes", () => {
    const bindings = compileBindings([
      {
        id: "c1",
        alias: "docs",
        config,
        state,
        enabled: true,
        tools: reviewCatalog([tool]),
        grants: [grant],
      },
    ]);
    expect(bindings[0]!.tools[0]!.name).toMatch(/^mcp_docs_/);
    expect(() => assertBindingCurrent(bindings[0]!, { ...state, enabled: true })).not.toThrow();
    expect(() =>
      assertBindingCurrent(bindings[0]!, { ...state, enabled: true, authGeneration: 2 }),
    ).toThrow();
    expect(() => assertBindingCurrent(bindings[0]!, { ...state, enabled: false })).toThrow();
    const before = bindings[0]!.hash;
    config.auth.secretRef = "mcp.service.replacement";
    expect(bindings[0]!.config.auth).toEqual({
      kind: "api_key" as const,
      secretRef: "mcp.service.api-key",
    });
    expect(bindings[0]!.hash).toBe(before);
    config.auth.secretRef = "mcp.service.api-key";
  });
  test("canonical hash rejects unsupported and non-finite JSON", () => {
    expect(canonicalDigest({ b: 1, a: 2 })).toBe(canonicalDigest({ a: 2, b: 1 }));
    expect(() => canonicalDigest({ n: NaN })).toThrow();
    expect(() => canonicalDigest({ n: undefined })).toThrow();
  });
});

test("MCP child grants intersect constraints and automation permission", async () => {
  const { intersectMcpGrants } = await import("../mcp/bindings.ts");
  const parent = {
    ...grant,
    mcp: {
      ...grant.mcp,
      unrestricted: false,
      argumentConstraints: { properties: { project: { const: "allowed" } }, required: ["project"] },
    },
  };
  const child = { ...grant, mcp: { ...grant.mcp, automationWrite: true } };
  const [effective] = intersectMcpGrants([parent], [child]);
  expect(effective!.mcp!.automationWrite).toBe(false);
  expect(effective!.mcp!.argumentConstraints).toEqual(parent.mcp.argumentConstraints);
  expect(() => intersectMcpGrants([], [child])).toThrow();
});

test("MCP grants cannot become generic guest capabilities", async () => {
  const { grantsToCapabilities } = await import("../integrations/grants.ts");
  const connection = {
    id: "c1",
    alias: "docs",
    provider: "mcp",
    displayName: "Docs",
    config,
    enabled: true,
    isDefault: false,
    testedAt: null,
    createdAt: new Date(0),
    updatedAt: new Date(0),
  };
  expect(grantsToCapabilities([{ grant, connection }])).toEqual([]);
});

test("schema keyword checks do not reinterpret ordinary property names or constants", async () => {
  const { assertSchemaShape } = await import("../mcp/catalog.ts");
  expect(() =>
    assertSchemaShape({
      type: "object",
      properties: { $ref: { type: "string" } },
      const: { $schema: "ordinary data" },
    }),
  ).not.toThrow();
  expect(() =>
    assertSchemaShape({ properties: { child: { $ref: "https://example.test/schema" } } }),
  ).toThrow("local");
});
