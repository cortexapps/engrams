import { expect, test } from "bun:test";
import { compileBoundSessionCandidate, createReservedTaskSession } from "../mcp/session-create.ts";
import { managedCreationInput } from "../mcp/managed-create-input.ts";
import { fromBinary } from "@bufbuild/protobuf";
import { CreatePolicySessionRequestSchema } from "../gen/engram/mcp/v1/session_policy_pb.ts";
import { compileSessionBindings } from "../mcp/session-bindings.ts";
import { bindingAuthority } from "./mcp-binding-fixture.ts";

const native = {
  name: "native",
  description: "Native tool",
  inputSchema: { type: "object" },
  execution: "sync",
};
function candidate() {
  const authority = bindingAuthority();
  return compileBoundSessionCandidate(
    {
      imageUri: "image",
      mode: "agent",
      requestedSessionId: "reserved",
      harnessEnv: { ENGRAM_TOOLS: JSON.stringify([native]) },
    },
    {
      launchPolicy: { profileId: authority.profileId, integrationGrants: authority.profileGrants },
      liveAuthority: authority,
      bindingEpoch: 0,
    },
  );
}
test("candidate and held reserve carry only the frozen native manifest", () => {
  const input = candidate();
  expect(JSON.parse(input.harnessEnv!.ENGRAM_TOOLS!)).toEqual([native]);
  const request = managedCreationInput(input, {
    actorId: "actor",
    inputId: "input",
    taskId: "task",
  });
  const wire = fromBinary(CreatePolicySessionRequestSchema, request.request.creationRequest);
  expect(JSON.parse(wire.create!.harnessEnv.ENGRAM_TOOLS!)).toEqual([native]);
  expect(wire.create!.requestedSessionId).toBe("reserved");
});
test("reserved MCP creation requires the managed capability and never calls generic create", async () => {
  let generic = 0;
  const sessions = {
    async createSession() {
      generic++;
      return { sessionId: "reserved" };
    },
  };
  await expect(
    createReservedTaskSession({ sessions }, candidate(), { actorId: "actor", inputId: "input" }),
  ).rejects.toThrow("not configured");
  expect(generic).toBe(0);
  await createReservedTaskSession(
    { sessions },
    { imageUri: "image", mode: "agent" },
    { actorId: "actor", inputId: "input" },
  );
  expect(generic).toBe(1);
});
test("child reserve binds the actual parent while cleared harness mode cannot remove ancestry", () => {
  const authority = bindingAuthority();
  const compile = {
    launchPolicy: { profileId: authority.profileId, integrationGrants: authority.profileGrants },
    liveAuthority: authority,
    bindingEpoch: 0,
  };
  const parent = compileSessionBindings(compile);
  const input = compileBoundSessionCandidate(
    { imageUri: "image", mode: "agent", requestedSessionId: "child" },
    {
      ...compile,
      parentAuthority: { sessionId: "actual-parent", snapshot: parent },
      requestedGrants: authority.profileGrants,
    },
  );
  const request = managedCreationInput(
    { ...input, harnessMode: undefined },
    { taskId: "task", actorId: "actor", inputId: "input", parentSessionId: "actual-parent" },
  );
  const wire = fromBinary(CreatePolicySessionRequestSchema, request.request.creationRequest);
  expect(wire.parentSessionId).toBe("actual-parent");
  expect(input.mcpBindingEnvelope!.ancestorAuthorityRefs[0]!.sessionId).toBe("actual-parent");
  expect(request.identity.parentSessionId).toBe("actual-parent");
});
test("returned identity mismatch preserves the actual ID for native delegation compensation", async () => {
  await expect(
    createReservedTaskSession(
      {
        sessions: {
          async createSession() {
            return { sessionId: "actual-created" };
          },
        },
      },
      { imageUri: "image", mode: "agent", requestedSessionId: "reserved" },
      { actorId: "actor", inputId: "source" },
    ),
  ).rejects.toMatchObject({ createdSessionId: "actual-created" });
});
