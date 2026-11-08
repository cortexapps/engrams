import { expect, test } from "bun:test";
import { create } from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";
import {
  McpRebindDrainReason,
  McpRebindDrainSchema,
} from "../../src/gen/engram/app/v1/integration_pb.ts";
import { commitReviewedRebind } from "./rebind.ts";

const oldHash = `sha256:${"1".repeat(64)}`;
const previewHash = `sha256:${"2".repeat(64)}`;
const finalHash = `sha256:${"3".repeat(64)}`;
const request = {
  sessionId: "00000000-0000-4000-8000-000000000001",
  expectedBindingHash: oldHash,
  expectedCandidateHash: previewHash,
};
const ctx = { deadlines: { stepMs: 1000, requestMs: 1000, totalMs: 1000 } };
function pending(reason = McpRebindDrainReason.DISPATCH_IN_PROGRESS) {
  return new ConnectError("pending", Code.FailedPrecondition, undefined, [
    {
      desc: McpRebindDrainSchema,
      value: create(McpRebindDrainSchema, {
        reason,
        retryAfter: "2026-11-07T00:00:00Z",
      }),
    },
  ]);
}

test("replays the exact reviewed request and returns the finalized hash", async () => {
  const calls: (typeof request)[] = [];
  const result = await commitReviewedRebind(ctx, request, async (arg) => {
    calls.push({ ...arg });
    if (calls.length === 1) throw pending();
    return { bindingHash: finalHash };
  });
  expect(calls).toEqual([request, request]);
  expect(result.bindingHash).toBe(finalHash);
  expect(result.bindingHash).not.toBe(previewHash);
});

test("does not retry unexpected errors or other drain reasons", async () => {
  for (const error of [
    new ConnectError("denied", Code.PermissionDenied),
    pending(McpRebindDrainReason.UNKNOWN_INTENT),
    new Error("unavailable"),
  ]) {
    let calls = 0;
    await expect(
      commitReviewedRebind(ctx, request, async () => {
        calls++;
        throw error;
      }),
    ).rejects.toThrow();
    expect(calls).toBe(1);
  }
});

test("reports only fixed precondition categories without retrying unexpected details", async () => {
  const cases = [
    {
      error: new ConnectError("private-data", Code.FailedPrecondition),
      seam: "rebind_precondition_details_0_reason_none_retry_na_category_other",
    },
    {
      error: new ConnectError(
        "MCP operation could not be completed",
        Code.FailedPrecondition,
      ),
      seam: "rebind_precondition_details_0_reason_none_retry_na_category_operation",
    },
    {
      error: pending(McpRebindDrainReason.UNKNOWN_INTENT),
      seam: "rebind_precondition_details_1_reason_unknown_intent_retry_valid_category_other",
    },
    {
      error: new ConnectError(
        "private-data",
        Code.FailedPrecondition,
        undefined,
        [
          {
            desc: McpRebindDrainSchema,
            value: create(McpRebindDrainSchema, {
              reason: McpRebindDrainReason.DISPATCH_IN_PROGRESS,
              retryAfter: "private-data",
            }),
          },
        ],
      ),
      seam: "rebind_precondition_details_1_reason_dispatch_retry_invalid_category_other",
    },
    {
      error: new ConnectError(
        "private-data",
        Code.FailedPrecondition,
        undefined,
        [
          { desc: McpRebindDrainSchema, value: create(McpRebindDrainSchema) },
          { desc: McpRebindDrainSchema, value: create(McpRebindDrainSchema) },
        ],
      ),
      seam: "rebind_precondition_details_many_reason_none_retry_na_category_other",
    },
  ];
  for (const { error, seam } of cases) {
    let calls = 0;
    try {
      await commitReviewedRebind(ctx, request, async () => {
        calls++;
        throw error;
      });
      throw new Error("unexpected success");
    } catch (caught) {
      expect(caught).toHaveProperty("seam", seam);
      expect(String(caught)).not.toContain("private-data");
      expect(calls).toBe(1);
    }
  }
});

test("rejects malformed drain detail and final binding hashes", async () => {
  const invalidDate = new ConnectError(
    "pending",
    Code.FailedPrecondition,
    undefined,
    [
      {
        desc: McpRebindDrainSchema,
        value: create(McpRebindDrainSchema, {
          reason: McpRebindDrainReason.DISPATCH_IN_PROGRESS,
          retryAfter: "invalid",
        }),
      },
    ],
  );
  let calls = 0;
  await expect(
    commitReviewedRebind(ctx, request, async () => {
      calls++;
      throw invalidDate;
    }),
  ).rejects.toThrow();
  expect(calls).toBe(1);
  for (const bindingHash of [oldHash, "bad"]) {
    await expect(
      commitReviewedRebind(ctx, request, async () => ({ bindingHash })),
    ).rejects.toThrow();
  }
});
