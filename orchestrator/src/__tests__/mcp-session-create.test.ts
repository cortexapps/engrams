import { expect, test } from "bun:test";
import { create } from "@bufbuild/protobuf";
import { CreatePolicySessionResponseSchema } from "../gen/engram/mcp/v1/session_policy_pb.ts";
import { makeBoundSessionCreator, compileBoundSessionCandidate } from "../mcp/session-create.ts";
import { bindingAuthority } from "./mcp-binding-fixture.ts";
import type { SessionPolicyControlClient } from "../mcp/control-client.ts";

test("closed creation adapter uses pinned policy creation and actual immutable parent", async () => {
  const authority = bindingAuthority();
  const input = compileBoundSessionCandidate(
    { imageUri: "image", mode: "agent", harness: "claude", requestedSessionId: "reserved" },
    {
      launchPolicy: { profileId: authority.profileId, integrationGrants: authority.profileGrants },
      liveAuthority: authority,
      bindingEpoch: 0,
    },
  );
  const calls: Parameters<SessionPolicyControlClient["createPolicySession"]>[0][] = [];
  const creator = makeBoundSessionCreator({
    control: {
      createPolicySession: async (req) => {
        calls.push(req);
        return create(CreatePolicySessionResponseSchema, { session: { sessionId: "reserved" } });
      },
    },
    readReservation: async () => ({
      principalId: "actor",
      parentSessionId: null,
      envelope: input.mcpBindingEnvelope!,
      connections: input.integrationConnections!,
    }),
    authorizeInput: async () => {},
    readAuthority: async () => authority,
    // Test-only trusted enforcement fixture; no production ACK implementation exists.
    enforceManagedDeny: async (expected) => expected,
    readParentCurrent: async () => {
      throw new Error("no parent");
    },
  });
  expect(
    (await creator.create(input, { actorId: "actor", inputId: "authenticated-input" })).sessionId,
  ).toBe("reserved");
  expect(calls[0]?.protocol).toBe(1);
  expect(calls[0]?.parentSessionId).toBeUndefined();
  expect(calls[0]?.inputId).toBe("authenticated-input");
  await expect(
    creator.create(input, { actorId: "other", inputId: "authenticated-input" }),
  ).rejects.toThrow();
  expect(calls.length).toBe(1);
});
test("candidate does not expose tools; acknowledged creation preserves native tools", async () => {
  const authority = bindingAuthority();
  const native = {
    name: "native",
    description: "Native",
    inputSchema: { type: "object" },
    execution: "sync",
    nativeBindings: {},
  };
  const input = compileBoundSessionCandidate(
    {
      imageUri: "image",
      mode: "agent",
      harness: "claude",
      requestedSessionId: "reserved",
      harnessEnv: { ENGRAM_TOOLS: JSON.stringify([native]) },
    },
    {
      launchPolicy: { profileId: authority.profileId, integrationGrants: authority.profileGrants },
      liveAuthority: authority,
      bindingEpoch: 0,
    },
  );
  expect(JSON.parse(input.harnessEnv!.ENGRAM_TOOLS!)).toEqual([native]);
  let names: string[] = [];
  const deps = {
    control: {
      createPolicySession: async (
        req: Parameters<SessionPolicyControlClient["createPolicySession"]>[0],
      ) => {
        names = JSON.parse(req.create!.harnessEnv!.ENGRAM_TOOLS!).map(
          (t: { name: string }) => t.name,
        );
        return create(CreatePolicySessionResponseSchema, { session: { sessionId: "reserved" } });
      },
    },
    readReservation: async () => ({
      principalId: "actor",
      parentSessionId: null,
      envelope: input.mcpBindingEnvelope!,
      connections: input.integrationConnections!,
    }),
    authorizeInput: async () => {},
    readAuthority: async () => authority,
    readParentCurrent: async () => {
      throw new Error("no parent");
    },
  };
  await expect(
    makeBoundSessionCreator(deps).create(input, { actorId: "actor", inputId: "input" }),
  ).rejects.toThrow("enforcement unavailable");
  expect(names).toEqual([]);
  await makeBoundSessionCreator({
    ...deps,
    enforceManagedDeny: async (expected) => expected,
  }).create(input, { actorId: "actor", inputId: "input" });
  expect(names).toContain("native");
  expect(names.length).toBe(2);
});
test("reserved MCP creation can never fall through to generic session creation", async () => {
  const { createReservedTaskSession } = await import("../mcp/session-create.ts");
  let genericCalls = 0;
  const sessions = {
    createSession: async () => {
      genericCalls++;
      return { sessionId: "reserved" };
    },
  };
  await expect(
    createReservedTaskSession(
      { sessions },
      {
        imageUri: "image",
        mode: "agent",
        integrationGrants: [
          {
            connectionId: "c",
            operation: "invoke",
            credentialScope: "org",
            resourceConstraints: [],
            mcp: {
              upstreamName: "Tool",
              unrestricted: true,
              automationWrite: false,
              argumentConstraintDocuments: [],
            },
          },
        ],
      },
      { actorId: "actor", inputId: "input" },
    ),
  ).rejects.toThrow();
  expect(genericCalls).toBe(0);
  await createReservedTaskSession(
    { sessions },
    { imageUri: "image", mode: "agent" },
    { actorId: "actor", inputId: "input" },
  );
  expect(genericCalls).toBe(1);
});

test("clearing child harness mode cannot detach the stored parent policy", async () => {
  const { compileSessionBindings } = await import("../mcp/session-bindings.ts");
  const authority = bindingAuthority();
  const compile = {
    launchPolicy: { profileId: authority.profileId, integrationGrants: authority.profileGrants },
    liveAuthority: authority,
    bindingEpoch: 0,
  };
  const parent = compileSessionBindings(compile);
  const input = compileBoundSessionCandidate(
    {
      imageUri: "image",
      mode: "agent",
      harness: "claude",
      requestedSessionId: "child",
      harnessMode: undefined,
    },
    {
      ...compile,
      parentAuthority: { sessionId: "actual-parent", snapshot: parent },
      requestedGrants: authority.profileGrants,
    },
  );
  let receivedParent: string | undefined;
  const creator = makeBoundSessionCreator({
    control: {
      createPolicySession: async (req) => {
        receivedParent = req.parentSessionId;
        return create(CreatePolicySessionResponseSchema, { session: { sessionId: "child" } });
      },
    },
    readReservation: async () => ({
      principalId: "actor",
      parentSessionId: "actual-parent",
      envelope: input.mcpBindingEnvelope!,
      connections: input.integrationConnections!,
    }),
    authorizeInput: async () => {},
    readAuthority: async () => authority,
    readParentCurrent: async (id) => {
      expect(id).toBe("actual-parent");
      return parent;
    },
    enforceManagedDeny: async (expected) => expected,
  });
  await creator.create(input, { actorId: "actor", inputId: "child-input" });
  expect(receivedParent).toBe("actual-parent");
});

test("MCP response mismatch carries the actual created ID for compensation", async () => {
  const authority = bindingAuthority();
  const input = compileBoundSessionCandidate(
    { imageUri: "image", mode: "agent", harness: "claude", requestedSessionId: "reserved" },
    {
      launchPolicy: { profileId: authority.profileId, integrationGrants: authority.profileGrants },
      liveAuthority: authority,
      bindingEpoch: 0,
    },
  );
  const creator = makeBoundSessionCreator({
    control: {
      createPolicySession: async () =>
        create(CreatePolicySessionResponseSchema, { session: { sessionId: "actual-created" } }),
    },
    readReservation: async () => ({
      principalId: "actor",
      parentSessionId: null,
      envelope: input.mcpBindingEnvelope!,
      connections: input.integrationConnections!,
    }),
    authorizeInput: async () => {},
    readAuthority: async () => authority,
    readParentCurrent: async () => {
      throw new Error("no parent");
    },
    enforceManagedDeny: async (expected) => expected,
  });
  await expect(
    creator.create(input, { actorId: "actor", inputId: "source" }),
  ).rejects.toMatchObject({
    message: "MCP policy session response mismatch",
    createdSessionId: "actual-created",
  });
});
