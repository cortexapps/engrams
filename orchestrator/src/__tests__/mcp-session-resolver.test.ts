import { expect, test } from "bun:test";
import { compileSessionBindings } from "../mcp/session-bindings.ts";
import {
  makeSessionBindingResolver,
  type BoundSessionAuthority,
  type SessionBindingResolverDeps,
} from "../mcp/session-resolver.ts";
import { bindingAuthority, bindingGrant } from "./mcp-binding-fixture.ts";

function fixture() {
  const authority = bindingAuthority();
  const snapshot = compileSessionBindings({
    launchPolicy: { profileId: authority.profileId, integrationGrants: authority.profileGrants },
    liveAuthority: authority,
    bindingEpoch: 0,
  });
  const rows = new Map<string, BoundSessionAuthority>([
    ["session-a", { snapshot, parentSessionId: null, active: true }],
  ]);
  const deps: SessionBindingResolverDeps = {
    readSession: async (id) => rows.get(id) ?? null,
    readAuthority: async () => authority,
    readBindingEpoch: async () => 0,
    // Explicit test adapter only. Production has no enforcement implementation.
    enforceManagedDeny: async (expected) => expected,
  };
  return { authority, snapshot, rows, deps };
}
test("only exact names in this session resolve and results cannot mutate stored snapshots", async () => {
  const f = fixture();
  const resolver = makeSessionBindingResolver(f.deps);
  const name = f.snapshot.immutableBindings[0]!.tools[0]!.name;
  const resolved = await resolver.resolveBoundTool({ sessionId: "session-a", modelToolName: name });
  expect(resolved?.bindingHash).toBe(f.snapshot.bindingHash);
  resolved!.reviewedTool.inputSchema = {};
  expect(
    (await resolver.resolveBoundTool({ sessionId: "session-a", modelToolName: name }))!.reviewedTool
      .inputSchema,
  ).not.toEqual({});
  for (const modelToolName of ["Document.Search", `${name}_spoof`, "mcp_docs_fake"])
    expect(
      await resolver.resolveBoundTool({ sessionId: "session-a", modelToolName }),
    ).toBeUndefined();
  expect(
    await resolver.resolveBoundTool({ sessionId: "session-b", modelToolName: name }),
  ).toBeUndefined();
});
test("absent ACK, wrong digest, restored epoch and changed ancestor grants deny", async () => {
  const f = fixture();
  const childAuthority = bindingAuthority();
  childAuthority.profileId = "profile-child";
  const child = compileSessionBindings({
    launchPolicy: {
      profileId: childAuthority.profileId,
      integrationGrants: childAuthority.profileGrants,
    },
    liveAuthority: childAuthority,
    bindingEpoch: 0,
    requestedGrants: [bindingGrant],
    parentAuthority: { sessionId: "session-a", snapshot: f.snapshot },
  });
  f.rows.set("child", { snapshot: child, parentSessionId: "session-a", active: true });
  f.deps.readAuthority = async (id) =>
    id === childAuthority.profileId ? childAuthority : f.authority;
  const input = { sessionId: "child", modelToolName: child.immutableBindings[0]!.tools[0]!.name };
  expect(await makeSessionBindingResolver(f.deps).resolveBoundTool(input)).toBeDefined();
  expect(
    await makeSessionBindingResolver({ ...f.deps, enforceManagedDeny: undefined }).resolveBoundTool(
      input,
    ),
  ).toBeUndefined();
  expect(
    await makeSessionBindingResolver({
      ...f.deps,
      enforceManagedDeny: async (expected) => ({ ...expected, denyDigest: "wrong" }),
    }).resolveBoundTool(input),
  ).toBeUndefined();
  expect(
    await makeSessionBindingResolver({
      ...f.deps,
      readBindingEpoch: async () => 1,
    }).resolveBoundTool(input),
  ).toBeUndefined();
  f.authority.profileGrants = [];
  expect(await makeSessionBindingResolver(f.deps).resolveBoundTool(input)).toBeUndefined();
  f.authority.profileGrants = [bindingGrant];
  f.rows.get("child")!.parentSessionId = null;
  expect(await makeSessionBindingResolver(f.deps).resolveBoundTool(input)).toBeUndefined();
});
test("two existing sessions resolve different catalogs without a global name registry", async () => {
  const f = fixture();
  const other = bindingAuthority();
  other.profileId = "profile-b";
  other.connections[0]!.tools = [];
  other.profileGrants = [];
  const otherSnapshot = compileSessionBindings({
    launchPolicy: { profileId: other.profileId, integrationGrants: other.profileGrants },
    liveAuthority: other,
    bindingEpoch: 0,
  });
  f.rows.set("session-b", { snapshot: otherSnapshot, parentSessionId: null, active: true });
  f.deps.readAuthority = async (id) => (id === "profile-b" ? other : f.authority);
  const resolver = makeSessionBindingResolver(f.deps),
    name = f.snapshot.immutableBindings[0]!.tools[0]!.name;
  expect(
    await resolver.resolveBoundTool({ sessionId: "session-a", modelToolName: name }),
  ).toBeDefined();
  expect(
    await resolver.resolveBoundTool({ sessionId: "session-b", modelToolName: name }),
  ).toBeUndefined();
});

test("every ACK identity field and parent binding replacement is fenced", async () => {
  const f = fixture(),
    name = f.snapshot.immutableBindings[0]!.tools[0]!.name;
  const input = { sessionId: "session-a", modelToolName: name };
  for (const field of ["sessionId", "bindingHash", "denyDigest"] as const) {
    const deps = {
      ...f.deps,
      enforceManagedDeny: async (
        expected: import("../mcp/session-resolver.ts").ManagedDenyAck,
      ) => ({ ...expected, [field]: "changed" }),
    };
    expect(await makeSessionBindingResolver(deps).resolveBoundTool(input)).toBeUndefined();
  }
  for (const field of ["bindingEpoch", "denyRevision"] as const) {
    const deps = {
      ...f.deps,
      enforceManagedDeny: async (
        expected: import("../mcp/session-resolver.ts").ManagedDenyAck,
      ) => ({ ...expected, [field]: 1 }),
    };
    expect(await makeSessionBindingResolver(deps).resolveBoundTool(input)).toBeUndefined();
  }
  const child = compileSessionBindings({
    launchPolicy: {
      profileId: f.authority.profileId,
      integrationGrants: f.authority.profileGrants,
    },
    liveAuthority: f.authority,
    bindingEpoch: 0,
    requestedGrants: f.authority.profileGrants,
    parentAuthority: { sessionId: "session-a", snapshot: f.snapshot },
  });
  f.rows.set("child", { snapshot: child, parentSessionId: "session-a", active: true });
  f.rows.get("session-a")!.snapshot = compileSessionBindings({
    launchPolicy: {
      profileId: f.authority.profileId,
      integrationGrants: f.authority.profileGrants,
    },
    liveAuthority: f.authority,
    bindingEpoch: 0,
    denyRevision: 1,
  });
  expect(
    await makeSessionBindingResolver(f.deps).resolveBoundTool({
      sessionId: "child",
      modelToolName: name,
    }),
  ).toBeUndefined();
});
