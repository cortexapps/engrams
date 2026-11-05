import { describe, expect, test } from "bun:test";
import {
  assertPolicyDenial,
  assertReadOnlyPolicy,
  assertNoDispatch,
  grantUpdateRequest,
  fixedPolicyPlan,
  expectedPlanApproval,
  assertAbsentToolDenial,
  assertNoProjectedAuthority,
  assertDeltaOneConstraints,
} from "./case-grants.ts";
import { create } from "@bufbuild/protobuf";
import {
  ProfileSchema,
  UpdateProfileRequestSchema,
} from "../../src/gen/engram/app/v1/profile_pb.ts";

describe("policy case evidence", () => {
  test("requires a received local refusal, not upstream error or missing event", () => {
    expect(() =>
      assertPolicyDenial({
        resultReceived: true,
        delivery: "available",
        outcome: null,
        isError: true,
      }),
    ).not.toThrow();
    for (const proof of [
      {},
      { resultReceived: true },
      {
        resultReceived: true,
        delivery: "available",
        outcome: "failed",
        isError: true,
      },
      {
        resultReceived: true,
        delivery: "unavailable",
        outcome: null,
        isError: true,
      },
    ])
      expect(() => assertPolicyDenial(proof)).toThrow();
  });
  test("requires unchanged reads, writes and count", () => {
    const before = { reads: 3, effects: { count: 4, executions: 2 } };
    expect(() => assertNoDispatch(before, before)).not.toThrow();
    for (const after of [
      { ...before, reads: 4 },
      { ...before, effects: { count: 5, executions: 2 } },
      { ...before, effects: { count: 4, executions: 3 } },
    ])
      expect(() => assertNoDispatch(before, after)).toThrow();
  });
  test("grant update carries exact current revision and preserves profile fields", () => {
    const profile = create(ProfileSchema, {
      id: "profile",
      name: "Fixture",
      imageId: "image",
      mcpGrantRevision: 7n,
      envVars: { FIXTURE: "neutral" },
      integrationGrants: [
        {
          connectionId: "connection",
          operation: "invoke",
          credentialScope: "org",
          mcp: { upstreamName: "read_counter", unrestricted: true },
        },
      ],
    });
    const request = create(
      UpdateProfileRequestSchema,
      grantUpdateRequest(profile, []),
    );
    expect(request.expectedMcpGrantRevision).toBe(7n);
    expect(request.envVars).toEqual(profile.envVars);
    expect(request.integrationGrants).toEqual([]);
    expect(request.imageId).toBe("image");
  });
  test("fixed child invocation has only the real native input fields", () => {
    const plan = fixedPolicyPlan("mcp_fixture", "read");
    const spawn = plan.steps.find((step) => step.tool === "spawn_session")!;
    expect(spawn.arguments).toEqual({
      task_name: "fixture-child",
      message: "[[fixture:child]]",
      idempotency_key: "fixture-child",
    });
    expect(plan.steps.find((step) => step.marker === "child")?.tool).toBe(
      "mcp_fixture",
    );
    expect(spawn.resultFormat).toBe("json");
  });
});

test("ordinary plan approval has exact native JSON result bytes", () => {
  expect(expectedPlanApproval("claude")).toEqual({
    content: [{ type: "text", text: '{"decision":"approve"}' }],
  });
  expect(expectedPlanApproval("codex")).toEqual({
    success: true,
    contentItems: [{ type: "inputText", text: '{"decision":"approve"}' }],
  });
});

test("absent-tool proof requires the actual native unknown-tool refusal", () => {
  const proof = {
    expectExposure: "absent",
    requestAttempted: true,
    protocolDenialCode: "unknown_tool",
    nativeDenialCode: -32602,
    exposedMcpToolNamesSha256: "sha256:" + "1".repeat(64),
  };
  expect(() => assertAbsentToolDenial("claude", proof)).not.toThrow();
  for (const changed of [
    { ...proof, requestAttempted: false },
    { ...proof, nativeDenialCode: -32603 },
    { ...proof, expectExposure: "present" },
    { ...proof, resultReceived: true },
    { ...proof, selectedSchemaSha256: "sha256:" + "2".repeat(64) },
  ])
    expect(() => assertAbsentToolDenial("claude", changed)).toThrow();
});

test("guest success projection cannot become journal authority or result", () => {
  const pending = { state: "waiting_approval", approvalDecision: "pending" };
  expect(() => assertNoProjectedAuthority(pending, undefined)).not.toThrow();
  expect(() =>
    assertNoProjectedAuthority({ ...pending, state: "canceled" }, undefined),
  ).toThrow();
  for (const state of ["authorized", "dispatching", "succeeded"])
    expect(() =>
      assertNoProjectedAuthority({ ...pending, state }, undefined),
    ).toThrow();
  expect(() =>
    assertNoProjectedAuthority(
      { ...pending, approvalDecision: "approved" },
      undefined,
    ),
  ).toThrow();
  expect(() =>
    assertNoProjectedAuthority(pending, '{"outcome":"succeeded"}'),
  ).toThrow();
});

test("a real pending approval has no projected authority", () => {
  expect(() =>
    assertNoProjectedAuthority(
      { state: "waiting_approval", approvalDecision: "pending" },
      undefined,
    ),
  ).not.toThrow();
});

test("an unknown outcome cannot prove that no dispatch occurred", () => {
  expect(() =>
    assertNoProjectedAuthority(
      { state: "outcome_unknown", approvalDecision: "" },
      undefined,
    ),
  ).toThrow();
});

test("retired guest projection can be denied without creating authority", () => {
  expect(() =>
    assertNoProjectedAuthority(
      {
        state: "canceled",
        approvalDecision: "denied",
        executionId: "call",
        cancellationReceipt: { executionId: "call", state: "canceled" },
      },
      undefined,
    ),
  ).not.toThrow();
  expect(() =>
    assertNoProjectedAuthority(
      {
        state: "canceled",
        approvalDecision: "denied",
        executionId: "call",
        cancellationReceipt: { executionId: "other", state: "canceled" },
      },
      undefined,
    ),
  ).toThrow();
});

test("intersection retains independent repeated schema roots without widening", () => {
  const schema = JSON.stringify({
    type: "object",
    properties: { delta: { const: 1 } },
    required: ["delta"],
    additionalProperties: false,
  });
  expect(() => assertDeltaOneConstraints([schema, schema])).not.toThrow();
  expect(() => assertDeltaOneConstraints([])).toThrow();
  expect(() =>
    assertDeltaOneConstraints([schema, JSON.stringify({ type: "object" })]),
  ).toThrow();
});

test("generic plan approval requires an observed exact read-only MCP policy", () => {
  expect(() =>
    assertReadOnlyPolicy({ writeEligibility: "read_only" }),
  ).not.toThrow();
  for (const policy of [
    undefined,
    { writeEligibility: "" },
    { writeEligibility: "unknown" },
    { writeEligibility: "write_eligible" },
  ]) {
    expect(() => assertReadOnlyPolicy(policy)).toThrow();
  }
});
