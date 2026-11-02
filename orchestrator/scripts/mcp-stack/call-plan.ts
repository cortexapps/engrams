import { z } from "zod";
import { requireThat } from "./context.ts";

const id = z.string().regex(/^[A-Za-z0-9_-]{1,64}$/);
export const CallPlanSchema = z.strictObject({
  steps: z
    .array(
      z.strictObject({
        marker: id,
        tool: z.string().regex(/^[A-Za-z0-9_]{1,128}$/),
        arguments: z.record(z.string(), z.unknown()),
        callId: id,
        resultFormat: z.enum(["mcp", "json"]),
        expectedResult: z.record(z.string(), z.unknown()).optional(),
        expectExposure: z.enum(["present", "absent"]).optional(),
        completionProjection: z.literal("premature").optional(),
      }),
    )
    .min(1)
    .max(16),
});
export type CallPlan = z.infer<typeof CallPlanSchema>;
function depth(value: unknown, current = 0): number {
  if (value === null || typeof value !== "object") return current;
  return Math.max(
    current + 1,
    ...Object.values(value).map((child) => depth(child, current + 1)),
  );
}
export function encodeCallPlan(input: CallPlan) {
  const plan = CallPlanSchema.parse(input);
  requireThat(
    new Set(plan.steps.map((step) => step.marker)).size === plan.steps.length &&
      new Set(plan.steps.map((step) => step.callId)).size === plan.steps.length,
    "call_plan_unique_identity",
  );
  for (const step of plan.steps) {
    requireThat(
      step.expectExposure !== "absent" ||
        (step.resultFormat === "mcp" &&
          step.tool.startsWith("mcp_") &&
          step.expectedResult === undefined &&
          step.completionProjection === undefined),
      "call_plan_absent_contract",
    );
    requireThat(
      step.completionProjection === undefined ||
        (step.resultFormat === "mcp" &&
          step.expectExposure !== "absent" &&
          step.expectedResult === undefined),
      "call_plan_premature_contract",
    );
    requireThat(
      Buffer.byteLength(JSON.stringify(step.arguments)) <= 4096 &&
        depth(step.arguments) <= 16,
      "call_plan_argument_bounds",
    );
  }
  const encoded = JSON.stringify(plan);
  requireThat(Buffer.byteLength(encoded) <= 65536, "call_plan_size");
  return encoded;
}
