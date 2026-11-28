import { expect, test } from "bun:test";
import { create } from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";
import {
  McpRebindDrainReason,
  McpRebindDrainSchema,
} from "../../src/gen/engram/app/v1/integration_pb.ts";
import { commitReviewedRebind } from "./rebind.ts";
import * as rebind from "./rebind.ts";
import { RunnerFailure } from "./context.ts";

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
  for (const [message, category] of [
    ["external MCP service is not configured", "service_unconfigured"],
    ["managed boot authority changed or unavailable", "boot_authority"],
    ["session policy changed or unavailable", "policy_changed"],
    ["host policy unavailable", "host_policy"],
    ["retained policy unavailable", "retained_policy"],
    ["managed guest identity unavailable", "guest_identity"],
    ["managed egress source unavailable", "egress_source"],
    ["managed policy does not support tunnels", "tunnels"],
    ["desired policy changed", "desired_policy"],
    ["managed guest identity changed", "guest_identity_changed"],
    ["host acknowledgement mismatch", "host_ack"],
    ["physical placement changed", "placement"],
    ["managed creation authority changed", "creation_authority"],
  ] as const) {
    cases.push({
      error: new ConnectError(message, Code.FailedPrecondition),
      seam: `rebind_precondition_details_0_reason_none_retry_na_category_${category}`,
    });
  }
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

test("OAuth recovery marks its branch, stage and observed phase without copying arbitrary failures", async () => {
  for (const change of ["account", "scope", "reconnect"] as const) {
    for (const stage of ["prepare", "commit", "started"] as const) {
      for (const error of [
        new Error("private-data"),
        new ConnectError("private-data", Code.FailedPrecondition),
        new RunnerFailure("ASSERTION_FAILED", "private-data"),
      ]) {
        let calls = 0;
        await expect(
          rebind.withRebindDiagnostics(
            change,
            stage,
            async () => ({ phase: "recovering" }),
            async () => {
              calls++;
              throw error;
            },
          ),
        ).rejects.toHaveProperty(
          "seam",
          `oauth_${change}_recovery_${stage}_phase_recovering_unclassified`,
        );
        expect(calls).toBe(1);
      }
    }
  }
  for (const readPhase of [
    async () => ({ phase: "private-data" }),
    async () => {
      throw new Error("private-data");
    },
  ]) {
    await expect(
      rebind.withRebindDiagnostics("account", "commit", readPhase, async () => {
        throw new Error("private-data");
      }),
    ).rejects.toHaveProperty(
      "seam",
      "oauth_account_recovery_commit_phase_unavailable_unclassified",
    );
  }
});

test("OAuth diagnostics retain fixed preconditions and never authorize another commit", async () => {
  for (const error of [
    new ConnectError("private-data", Code.FailedPrecondition),
    pending(McpRebindDrainReason.UNKNOWN_INTENT),
    pending(McpRebindDrainReason.LEGACY_INTENT),
    pending(McpRebindDrainReason.UNSPECIFIED),
    // Unknown wire enum values must also fail closed.
    pending(99 as McpRebindDrainReason),
  ]) {
    let calls = 0;
    try {
      await rebind.withRebindDiagnostics(
        "scope",
        "commit",
        async () => ({ phase: "recovering" }),
        () =>
          commitReviewedRebind(ctx, request, async () => {
            calls++;
            throw error;
          }),
      );
      throw new Error("unexpected success");
    } catch (caught) {
      expect(caught).toBeInstanceOf(RunnerFailure);
      expect(caught).toHaveProperty(
        "seam",
        expect.stringMatching(
          /^oauth_scope_recovery_commit_phase_recovering_rebind_precondition_details_(0|1)_reason_(none|unknown_intent|legacy_intent|unspecified|other)_retry_(na|valid)_category_other$/,
        ),
      );
      expect(String(caught)).not.toContain("private-data");
      expect(String(caught).length).toBeLessThanOrEqual(217);
      expect(calls).toBe(1);
    }
  }
  const calls: (typeof request)[] = [];
  const result = await rebind.withRebindDiagnostics(
    "reconnect",
    "commit",
    async () => {
      throw new Error("must not read on success");
    },
    () =>
      commitReviewedRebind(ctx, request, async (arg) => {
        calls.push({ ...arg });
        if (calls.length === 1) throw pending();
        return { bindingHash: finalHash };
      }),
  );
  expect(calls).toEqual([request, request]);
  expect(result.bindingHash).toBe(finalHash);
});

test("recovery diagnostics retain the fixed identity refusal seam", async () => {
  await expect(
    rebind.withRebindDiagnostics(
      "reconnect",
      "started",
      async () => ({ phase: "started" }),
      async () => {
        throw new RunnerFailure(
          "ASSERTION_FAILED",
          "managed_cold_recovery_identity",
        );
      },
    ),
  ).rejects.toHaveProperty(
    "seam",
    "oauth_reconnect_recovery_started_phase_started_managed_cold_recovery_identity",
  );
});
