import { expect, test } from "bun:test";
import { create } from "@bufbuild/protobuf";
import { createHandlerContext } from "@connectrpc/connect";
import {
  SessionService,
  CompleteToolCallRequestSchema,
  GetMcpWritePolicyRequestSchema,
  McpPlanReviewSchema,
} from "../gen/engram/app/v1/session_pb.ts";
import { makeSessionPolicyOverrides } from "../mcp/session-policy.ts";

const context = () =>
  createHandlerContext({
    service: SessionService,
    method: SessionService.method.completeToolCall,
    protocolName: "connect",
    requestMethod: "POST",
    url: "https://test.invalid/rpc",
    requestHeader: new Headers(),
    responseHeader: new Headers(),
    responseTrailer: new Headers(),
  });
const review = () =>
  create(McpPlanReviewSchema, {
    policyRef: { sessionId: "session-a", policyRevision: 3n, bindingEpoch: 2n },
    toolCallId: "plan-a",
    runId: "run-a",
    eventIdx: 7n,
    requestDigest: "a".repeat(64),
    argsJson: '{"plan":"reviewed"}',
  });
test("human plan acceptance uses exact expected context and never relaxes in a preflight", async () => {
  let accepted = 0;
  let checked = 0;
  const overrides = makeSessionPolicyOverrides({
    getSession: async () => ({ user: { id: "owner-a" } }),
    authorize: async (sid, actor) => {
      checked++;
      expect(sid).toBe("session-a");
      expect(actor).toBe("owner-a");
      return { id: actor, isAdmin: false };
    },
    control: {
      async acceptPlanExit(req) {
        accepted++;
        expect(req.actorId).toBe("owner-a");
        expect(req.expected).toEqual(review());
        throw new Error("unavailable");
      },
    },
  });
  const req = create(CompleteToolCallRequestSchema, {
    sessionId: "session-a",
    toolCallId: "plan-a",
    resultJson: '{"decision":"approve"}',
    expectedMcpPlanReview: review(),
  });
  await expect(overrides["SessionService.CompleteToolCall"]!(req, context())).rejects.toThrow(
    "unavailable",
  );
  expect(accepted).toBe(1);
  expect(checked).toBe(1);
});
test("generic approval does not call the policy authority", async () => {
  const overrides = makeSessionPolicyOverrides();
  expect(
    await overrides["SessionService.CompleteToolCall"]!(
      create(CompleteToolCallRequestSchema, {
        sessionId: "s",
        toolCallId: "c",
        resultJson: '{"decision":"approve"}',
      }),
      context(),
    ),
  ).toBeUndefined();
});
test("changed identity and rejected human authorization prevent policy mutation", async () => {
  let accepted = 0;
  const overrides = makeSessionPolicyOverrides({
    getSession: async () => ({ user: { id: "member-a" } }),
    authorize: async () => {
      throw new Error("owner or administrator required");
    },
    control: {
      async acceptPlanExit() {
        accepted++;
        return {};
      },
    },
  });
  await expect(
    overrides["SessionService.CompleteToolCall"]!(
      create(CompleteToolCallRequestSchema, {
        sessionId: "session-a",
        toolCallId: "plan-a",
        resultJson: '{"decision":"approve"}',
        expectedMcpPlanReview: review(),
      }),
      context(),
    ),
  ).rejects.toThrow();
  expect(accepted).toBe(0);
});
test("policy review is read from trusted control and exposes its action", async () => {
  const overrides = makeSessionPolicyOverrides({
    getSession: async () => ({ user: { id: "owner-a" } }),
    authorize: async () => ({ id: "owner-a", isAdmin: false }),
    control: {
      async readWritePolicy() {
        return {
          state: {
            policy: { writeEligibility: "read_only" },
            planReview: review(),
            reason: "External writes are blocked.",
          },
        };
      },
    },
  });
  const result = await overrides["SessionService.GetMcpWritePolicy"]!(
    create(GetMcpWritePolicyRequestSchema, { sessionId: "session-a", planToolCallId: "plan-a" }),
    context(),
  );
  expect(result?.response).toMatchObject({
    planReview: review(),
    allowedActions: ["approve_current_plan"],
  });
});

test("rejection keeps ordinary result delivery and drops permissive review context", async () => {
  const req = create(CompleteToolCallRequestSchema, {
    sessionId: "session-a",
    toolCallId: "plan-a",
    resultJson: '{"decision":"reject"}',
    expectedMcpPlanReview: review(),
  });
  expect(
    await makeSessionPolicyOverrides()["SessionService.CompleteToolCall"]!(req, context()),
  ).toBeUndefined();
  expect(req.expectedMcpPlanReview).toBeUndefined();
});

test("public approved review replaces generic forwarding only after the trusted commit", async () => {
  const { createClient, createRouterTransport } = await import("@connectrpc/connect");
  const { registerPassthrough } = await import("../rpc/passthrough.ts");
  let generic = 0,
    accepted = 0;
  const getSession = async () => ({ user: { id: "owner-a", role: "user" } });
  const upstream = createRouterTransport((router) =>
    router.service(SessionService, {
      async completeToolCall(req) {
        generic++;
        return { sessionId: req.sessionId, note: "generic" };
      },
    }),
  );
  const transport = createRouterTransport((router) =>
    registerPassthrough(
      router,
      [{ service: SessionService, methods: ["CompleteToolCall"] }],
      upstream,
      getSession,
      async () => "owner-a",
      undefined,
      makeSessionPolicyOverrides({
        getSession,
        authorize: async () => ({ id: "owner-a", isAdmin: false }),
        control: {
          async acceptPlanExit() {
            accepted++;
            return {};
          },
        },
      }),
    ),
  );
  const client = createClient(SessionService, transport);
  const result = await client.completeToolCall({
    sessionId: "session-a",
    toolCallId: "plan-a",
    resultJson: '{"decision":"approve"}',
    expectedMcpPlanReview: review(),
  });
  expect(result.note).toBe("tool result queued");
  expect(accepted).toBe(1);
  expect(generic).toBe(0);
  await client.completeToolCall({
    sessionId: "session-a",
    toolCallId: "other",
    resultJson: '{"decision":"approve"}',
  });
  expect(generic).toBe(1);
  expect(accepted).toBe(1);
});
