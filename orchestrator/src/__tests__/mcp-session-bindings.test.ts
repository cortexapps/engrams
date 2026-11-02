import { expect, test } from "bun:test";
import { canonicalDigest } from "../mcp/hash.ts";
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
  expect(manifest[0]?.resultFormat).toBe("mcp");
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
  expect(manifest[0]!.resultFormat).toBe("mcp");
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

test("publication revision and publisher are frozen and cannot silently rebase a session", () => {
  const source = bindingAuthority();
  const authority = {
    ...source,
    profilePublication: {
      revision: 1,
      actorId: "admin-a",
      grantsHash: canonicalDigest(source.profileGrants),
    },
  };
  const snapshot = compileSessionBindings({
    launchPolicy: { profileId: authority.profileId, integrationGrants: authority.profileGrants },
    liveAuthority: authority,
    bindingEpoch: 0,
  });
  expect(snapshot.profilePublication).toEqual(authority.profilePublication);
  authority.profilePublication.revision = 2;
  expect(checkBindingCurrent({ snapshot, liveAuthority: authority }).current).toBe(false);
  expect(snapshot.profilePublication!.revision).toBe(1);
  authority.profilePublication.grantsHash = canonicalDigest([]);
  expect(() =>
    compileSessionBindings({
      launchPolicy: { profileId: authority.profileId, integrationGrants: authority.profileGrants },
      liveAuthority: authority,
      bindingEpoch: 0,
    }),
  ).toThrow("publication");
});

test("complete operator publication denies unused zones and invalidates their changes", () => {
  const authority = bindingAuthority();
  authority.operatorPolicies.push({
    ...authority.operatorPolicies[0]!,
    zoneId: "internal",
    authorities: ["unused.example.test", "unused.example.test:8443"],
    aliases: ["unused-alias.example.test"],
  });
  const snapshot = compile(authority);
  expect(snapshot.managedDenyPolicy.hosts).toEqual([
    "alias.example.test",
    "tools.example.test",
    "unused-alias.example.test",
    "unused.example.test",
  ]);
  authority.operatorPolicies[1]!.policyRevision = "2";
  expect(checkBindingCurrent({ snapshot, liveAuthority: authority }).current).toBe(false);
});

test("binding retains exact publication and distinct infrastructure trust identity", () => {
  const authority = bindingAuthority();
  const publication = {
    generation: 1,
    identity: `sha256:${"b".repeat(64)}`,
    deploymentIdentity: `sha256:${"c".repeat(64)}`,
    networkIdentity: `sha256:${"d".repeat(64)}`,
    trustIdentity: `sha256:${"e".repeat(64)}`,
    caContentDigests: { control: `sha256:${"f".repeat(64)}` },
  };
  Object.assign(authority.operatorPolicies[0]!, { publication });
  const snapshot = compile(authority);
  expect(snapshot.immutableBindings[0]!.operatorPolicy).toMatchObject({ publication });
  publication.caContentDigests.control = `sha256:${"0".repeat(64)}`;
  expect(checkBindingCurrent({ snapshot, liveAuthority: authority }).current).toBe(false);
});
