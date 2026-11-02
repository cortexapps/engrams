import { expect, test } from "bun:test";
import { reviewCatalog } from "../mcp/catalog.ts";
import {
  compileSessionBindings,
  checkBindingCurrent,
  compileMcpManifest,
} from "../mcp/session-bindings.ts";

import { schema, bindingGrant, bindingAuthority } from "./mcp-binding-fixture.ts";
function compile(authority = bindingAuthority()) {
  return compileSessionBindings({
    launchPolicy: { profileId: authority.profileId, integrationGrants: authority.profileGrants },
    liveAuthority: authority,
    bindingEpoch: 0,
  });
}
test("session bindings preserve reviewed schema and freeze every authority input", () => {
  const authority = bindingAuthority();
  const snapshot = compile(authority);
  const manifest = compileMcpManifest(snapshot);
  expect(manifest[0]?.inputSchema).toEqual(schema);
  expect(manifest[0]?.execution).toBe("sync");
  expect(snapshot.managedDenyPolicy.hosts).toEqual(["alias.example.test", "tools.example.test"]);
  const original = structuredClone(snapshot);
  authority.connections[0]!.tools[0]!.inputSchema = { type: "null" };
  authority.operatorPolicies[0]!.aliases.push("new.example.test");
  expect(snapshot).toEqual(original);
  expect(checkBindingCurrent({ snapshot, liveAuthority: authority }).current).toBe(false);
});
test("all identity and authority changes invalidate; token refresh has no effect", () => {
  const snapshot = compile();
  expect(checkBindingCurrent({ snapshot, liveAuthority: bindingAuthority() }).current).toBe(true);
  const mutations = [
    (a: ReturnType<typeof bindingAuthority>) => {
      a.connections[0]!.state.configRevision++;
    },
    (a: ReturnType<typeof bindingAuthority>) => {
      a.connections[0]!.state.authGeneration++;
    },
    (a: ReturnType<typeof bindingAuthority>) => {
      a.connections[0]!.state.catalogRevision++;
    },
    (a: ReturnType<typeof bindingAuthority>) => {
      a.connections[0]!.state.credentialIdentity = "identity-b";
    },
    (a: ReturnType<typeof bindingAuthority>) => {
      a.connections[0]!.enabled = false;
    },
    (a: ReturnType<typeof bindingAuthority>) => {
      a.connections[0]!.state.revoked = true;
    },
    (a: ReturnType<typeof bindingAuthority>) => {
      a.connections[0]!.alias = "renamed";
    },
    (a: ReturnType<typeof bindingAuthority>) => {
      a.connections[0]!.config.endpoint = "https://other.example.test/mcp";
    },
    (a: ReturnType<typeof bindingAuthority>) => {
      a.operatorPolicies[0]!.policyRevision = "2";
    },
    (a: ReturnType<typeof bindingAuthority>) => {
      a.operatorPolicies[0]!.policyDigest = `sha256:${"b".repeat(64)}`;
    },
    (a: ReturnType<typeof bindingAuthority>) => {
      a.operatorPolicies[0]!.aliases = [];
    },
    (a: ReturnType<typeof bindingAuthority>) => {
      a.profileGrants = [];
    },
  ];
  for (const mutate of mutations) {
    const authority = bindingAuthority();
    mutate(authority);
    expect(checkBindingCurrent({ snapshot, liveAuthority: authority }).current).toBe(false);
  }
});
test("children intersect actual parent, profile and explicit request; empty never inherits", () => {
  const authority = bindingAuthority();
  authority.profileGrants[0]!.mcp = {
    ...bindingGrant.mcp!,
    unrestricted: false,
    argumentConstraintDocuments: [schema],
  };
  const parent = compile(authority);
  const childAuthority = bindingAuthority();
  childAuthority.profileGrants[0]!.mcp!.automationWrite = true;
  const args = {
    launchPolicy: {
      profileId: childAuthority.profileId,
      integrationGrants: childAuthority.profileGrants,
    },
    liveAuthority: childAuthority,
    bindingEpoch: 0,
    parentAuthority: { sessionId: "parent", snapshot: parent },
  };
  const empty = compileSessionBindings({ ...args, requestedGrants: [] });
  expect(empty.immutableBindings).toEqual([]);
  const child = compileSessionBindings({ ...args, requestedGrants: childAuthority.profileGrants });
  expect(child.immutableBindings[0]!.tools[0]!.grant.argumentConstraintDocuments).toEqual([schema]);
  expect(child.immutableBindings[0]!.tools[0]!.grant.automationWrite).toBe(false);
  expect(child.ancestorAuthorityRefs[0]?.bindingHash).toBe(parent.bindingHash);
  expect(() =>
    compileSessionBindings({
      ...args,
      requestedGrants: [
        { ...bindingGrant, mcp: { ...bindingGrant.mcp!, upstreamName: "Unknown" } },
      ],
    }),
  ).toThrow();
});
test("write descriptors stay deferred and catalog changes cannot mutate old manifest", () => {
  const authority = bindingAuthority();
  authority.connections[0]!.tools = reviewCatalog([
    {
      upstreamName: "Document.Search",
      description: "Write",
      inputSchema: schema,
      classification: "write",
      enabled: true,
    },
  ]);
  const snapshot = compile(authority);
  const manifest = compileMcpManifest(snapshot);
  expect(manifest[0]!.execution).toBe("deferred");
  manifest[0]!.inputSchema = {};
  expect(compileMcpManifest(snapshot)[0]!.inputSchema).toEqual(schema);
});
test("missing operator approval, altered review fingerprints and over-limit requests fail closed", () => {
  const authority = bindingAuthority();
  authority.operatorPolicies = [];
  expect(() => compile(authority)).toThrow();
  const changed = bindingAuthority();
  changed.connections[0]!.tools[0]!.description = "Changed after review";
  expect(() => compile(changed)).toThrow();
  const tooMany = bindingAuthority();
  tooMany.connections = Array.from({ length: 17 }, (_, i) => ({
    ...tooMany.connections[0]!,
    id: `c${i}`,
    alias: `docs${i}`,
  }));
  tooMany.profileGrants = tooMany.connections.map((c) => ({ ...bindingGrant, connectionId: c.id }));
  expect(() => compile(tooMany)).toThrow();
});
test("changes to managed destinations outside selected grants invalidate the deny set", () => {
  const authority = bindingAuthority();
  authority.connections.push({
    ...structuredClone(authority.connections[0]!),
    id: "ungranted",
    alias: "other",
  });
  authority.operatorPolicies[0]!.authorities.push("second.example.test");
  const snapshot = compile(authority);
  authority.connections[1]!.config.endpoint = "https://second.example.test/mcp";
  expect(checkBindingCurrent({ snapshot, liveAuthority: authority }).current).toBe(false);
});
test("binding freezes the immutable catalog row identifier", () => {
  const authority = bindingAuthority();
  const snapshot = compile(authority);
  expect(snapshot.immutableBindings[0]!.catalogId).toBe(authority.connections[0]!.catalogId);
  authority.connections[0]!.catalogId = "replacement-catalog";
  expect(checkBindingCurrent({ snapshot, liveAuthority: authority }).current).toBe(false);
});
test("merged manifest checks native collisions and the total tool and byte limits", async () => {
  const { appendMcpManifest } = await import("../tools/manifest.ts");
  const snapshot = compile();
  const native = {
    name: "native",
    description: "Native",
    inputSchema: { type: "object" },
    execution: "sync",
    nativeBindings: {},
  };
  const limit = Array.from({ length: 127 }, (_, i) => ({ ...native, name: `native${i}` }));
  expect(appendMcpManifest(JSON.stringify(limit), snapshot)).toHaveLength(128);
  expect(() => appendMcpManifest(JSON.stringify([...limit, native]), snapshot)).toThrow();
  expect(() =>
    appendMcpManifest(
      JSON.stringify([{ ...native, name: snapshot.immutableBindings[0]!.tools[0]!.name }]),
      snapshot,
    ),
  ).toThrow();
  expect(() =>
    appendMcpManifest(
      JSON.stringify([{ ...native, description: "a".repeat(1024 * 1024) }]),
      snapshot,
    ),
  ).toThrow();
});
