import { expect, test } from "bun:test";
import type { IntegrationPolicyJson } from "../connectors/registry.ts";
import { makeMcpSessionCompiler } from "../mcp/session-compiler.ts";
import { makeMcpOperatorRuntime } from "../mcp/operator-runtime.ts";
import { compileSessionBindings } from "../mcp/session-bindings.ts";
import { restoreBindingSnapshot } from "../mcp/binding-store.ts";
import { buildLaunchPolicy } from "../rpc/task-create.ts";
import { bindingAuthority, operatorPublication } from "./mcp-binding-fixture.ts";

const direct: IntegrationPolicyJson = {
  network: { default: "allow", allow_hosts: [], allow_host_patterns: [] },
  injects: [],
  observes: [],
  secrets: [],
  guest_services: [],
  tunnels: [],
};
function fixture(live = bindingAuthority()) {
  let parent = compileSessionBindings({
    launchPolicy: { profileId: live.profileId, integrationGrants: live.profileGrants },
    liveAuthority: live,
    bindingEpoch: 7,
    requestedGrants: [
      {
        ...live.profileGrants[0]!,
        mcp: {
          ...live.profileGrants[0]!.mcp!,
          unrestricted: false,
          automationWrite: false,
          argumentConstraintDocuments: [
            { type: "object", properties: { name: { maxLength: 10 } } },
          ],
        },
      },
    ],
  });
  let currentParent = true,
    enabled = true,
    parentReads = 0;
  const tls = {
    url: "https://worker.example.test/",
    ca: "ca",
    cert: "cert",
    key: "key",
    serverFingerprint: "AA",
  };
  const operator = makeMcpOperatorRuntime({
    configuration: {
      control: tls,
      infrastructure: operatorPublication,
      workers: new Map([["public", { enabled: true, deploymentId: "workers", tls }]]),
      registrations: [],
    },
    enabled: () => enabled,
    read: async () => ({
      generation: 1,
      identity: operatorPublication.identity,
      policies: live.operatorPolicies,
    }),
  });
  const compile = makeMcpSessionCompiler({
    operator,
    readAuthority: async () => live,
    readParentCurrent: async (id) => {
      expect(id).toBe("parent");
      parentReads++;
      if (!currentParent) throw Error("parent unavailable");
      return parent;
    },
    reservedSecretReferences: async () => new Set(["reserved-reference"]),
  });
  const input = {
    imageUri: "image",
    mode: "agent",
    requestedSessionId: "child",
    harnessEnv: {
      ENGRAM_TOOLS: JSON.stringify([
        {
          name: "native",
          description: "Native",
          inputSchema: {},
          execution: "sync",
          nativeBindings: {},
        },
      ]),
    },
  };
  const context = {
    profileId: live.profileId,
    profileGrants: live.profileGrants,
    grants: live.profileGrants,
    directPolicy: direct,
    spawningSessionId: "parent",
  };
  return {
    live,
    parent,
    compile,
    input,
    context,
    staleParent: () => {
      currentParent = false;
    },
    disable: () => {
      enabled = false;
    },
    reads: () => parentReads,
    replaceParent: (snapshot: typeof parent) => {
      parent = snapshot;
    },
  };
}

test("child compilation uses the live source profile and the parent's frozen effective request", async () => {
  const live = bindingAuthority();
  live.profileGrants[0]!.mcp = {
    ...live.profileGrants[0]!.mcp!,
    unrestricted: false,
    argumentConstraintDocuments: [{ type: "object", properties: { name: { maxLength: 10 } } }],
  };
  const f = fixture(live);
  const root = await f.compile(f.input, {
    profileId: f.live.profileId,
    profileGrants: f.live.profileGrants,
    grants: f.live.profileGrants,
    directPolicy: direct,
  });
  const policy = buildLaunchPolicy(
    {
      id: f.live.profileId,
      name: "Fixture",
      description: "",
      icon: "",
      imageId: "image",
      harness: "claude",
      model: null,
      effort: null,
      programmaticOnly: false,
      includeUserTokens: false,
      envVars: {},
      skills: [],
      integrationGrants: f.live.profileGrants,
      network: { default: "deny", allowHosts: [], allowHostPatterns: [] },
      secrets: [],
      repos: [],
      apps: [],
      createdAt: new Date(0),
      updatedAt: new Date(0),
      deletedAt: null,
    },
    root,
  );
  expect(policy.integrationGrants[0]!.mcp!.argumentConstraintDocuments).toHaveLength(1);
  const parent = restoreBindingSnapshot(root.mcpBindingEnvelope!, root.integrationConnections!);
  f.replaceParent(parent);
  const inherited = {
    ...f.context,
    profileGrants: policy.integrationGrants,
    grants: policy.integrationGrants,
  };
  const child = await f.compile(f.input, inherited);
  const snapshot = restoreBindingSnapshot(child.mcpBindingEnvelope!, child.integrationConnections!);
  expect(snapshot.ancestorAuthorityRefs[0]).toMatchObject({
    sessionId: "parent",
    bindingHash: parent.bindingHash,
    bindingEpoch: 0,
  });
  expect(snapshot.immutableBindings[0]!.tools[0]!.grant).toMatchObject({
    unrestricted: false,
    automationWrite: false,
    argumentConstraintDocuments: f.live.profileGrants[0]!.mcp!.argumentConstraintDocuments,
  });
  expect(f.reads()).toBe(1);
  const empty = await f.compile(f.input, { ...inherited, grants: [] });
  expect(
    restoreBindingSnapshot(empty.mcpBindingEnvelope!, empty.integrationConnections!)
      .immutableBindings,
  ).toHaveLength(0);
  f.staleParent();
  await expect(f.compile(f.input, inherited)).rejects.toThrow("parent unavailable");
  const changedProfileGrants = structuredClone(policy.integrationGrants);
  changedProfileGrants[0]!.mcp!.argumentConstraintDocuments = [
    { type: "object", properties: { name: { maxLength: 5 } } },
  ];
  await expect(
    f.compile(f.input, {
      profileId: f.live.profileId,
      profileGrants: changedProfileGrants,
      grants: policy.integrationGrants,
      directPolicy: direct,
    }),
  ).rejects.toThrow("MCP profile changed during compilation");
});

test("unchanged constrained grants stay bounded through deep child compilation", async () => {
  const live = bindingAuthority();
  const constraint = { type: "object", properties: { name: { maxLength: 10 } } };
  live.profileGrants[0]!.mcp = {
    ...live.profileGrants[0]!.mcp!,
    unrestricted: false,
    argumentConstraintDocuments: [constraint],
  };
  let parent = compileSessionBindings({
    launchPolicy: { profileId: live.profileId, integrationGrants: live.profileGrants },
    liveAuthority: live,
    bindingEpoch: 0,
    requestedGrants: live.profileGrants,
  });
  for (let depth = 0; depth < 12; depth++) {
    const grants = [
      { ...live.profileGrants[0]!, mcp: parent.immutableBindings[0]!.tools[0]!.grant },
    ];
    const snapshot = compileSessionBindings({
      launchPolicy: { profileId: live.profileId, integrationGrants: live.profileGrants },
      liveAuthority: live,
      bindingEpoch: 0,
      requestedGrants: grants,
      parentAuthority: { sessionId: `parent-${depth}`, snapshot: parent },
    });
    expect(snapshot.immutableBindings[0]!.tools[0]!.grant.argumentConstraintDocuments).toEqual([
      constraint,
    ]);
    parent = snapshot;
  }
});

test("actual child compiler freezes native tools and intersects the fresh parent authority", async () => {
  const f = fixture();
  const child = await f.compile(f.input, f.context);
  const snapshot = restoreBindingSnapshot(child.mcpBindingEnvelope!, child.integrationConnections!);
  expect(snapshot.bindingEpoch).toBe(0);
  expect(snapshot.ancestorAuthorityRefs[0]).toMatchObject({
    sessionId: "parent",
    bindingHash: f.parent.bindingHash,
    bindingEpoch: 7,
  });
  expect(snapshot.immutableBindings[0]!.tools[0]!.grant).toMatchObject({
    unrestricted: false,
    automationWrite: false,
    argumentConstraintDocuments:
      f.parent.immutableBindings[0]!.tools[0]!.grant.argumentConstraintDocuments,
  });
  expect(
    JSON.parse(child.harnessEnv!.ENGRAM_TOOLS!).map((tool: { name: string }) => tool.name),
  ).toEqual(["native"]);
  expect(f.reads()).toBe(1);
  f.staleParent();
  await expect(f.compile(f.input, { ...f.context, grants: [] })).rejects.toThrow(
    "parent unavailable",
  );
});

test("actual child compiler preserves managed ancestry without granting implicit tools", async () => {
  const f = fixture();
  const child = await f.compile(f.input, { ...f.context, grants: [] });
  const snapshot = restoreBindingSnapshot(child.mcpBindingEnvelope!, child.integrationConnections!);
  expect(snapshot.immutableBindings).toHaveLength(0);
  expect(snapshot.ancestorAuthorityRefs[0]!.bindingHash).toBe(f.parent.bindingHash);
  expect(snapshot.managedDenyPolicy.hosts).toContain("tools.example.test");
  expect(snapshot.managedDenyPolicy.hosts).toContain("alias.example.test");
});

test("final child policy cannot bypass managed destinations or inject stored credential references", async () => {
  const f = fixture();
  for (const policy of [
    { ...direct, network: { ...direct.network, allow_hosts: ["tools.example.test"] } },
    { ...direct, network: { ...direct.network, allow_host_patterns: ["*.example.test"] } },
    {
      ...direct,
      secrets: [
        {
          secret_ref: "reserved-reference",
          env_var: "TOKEN",
          mode: "literal" as const,
          allow_hosts: [],
          allow_host_patterns: [],
        },
      ],
    },
  ])
    await expect(f.compile(f.input, { ...f.context, directPolicy: policy })).rejects.toThrow();
  f.disable();
  await expect(f.compile(f.input, f.context)).rejects.toThrow("disabled");
});

test("child compiler rejects the combined tool limit before creating a held reservation", async () => {
  const f = fixture();
  const native = (count: number) =>
    JSON.stringify(
      Array.from({ length: count }, (_, i) => ({
        name: `native-${i}`,
        description: "Native",
        inputSchema: {},
        execution: "sync",
        nativeBindings: {},
      })),
    );
  const accepted = await f.compile(
    { ...f.input, harnessEnv: { ENGRAM_TOOLS: native(127) } },
    f.context,
  );
  expect(JSON.parse(accepted.harnessEnv!.ENGRAM_TOOLS!)).toHaveLength(127);
  await expect(
    f.compile({ ...f.input, harnessEnv: { ENGRAM_TOOLS: native(128) } }, f.context),
  ).rejects.toThrow("manifest exceeds limits");
});
