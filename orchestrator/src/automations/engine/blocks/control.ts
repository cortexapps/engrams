/** Control blocks: filter, branch, loop (ADR 0119 D1).
 *
 * The interpreter owns their semantics (checkpointed condition steps, frame
 * recursion); registration here supplies the config schemas the validator
 * enforces at save time.
 */

import { z } from "zod";

import { parseFilterGroup } from "../conditions.ts";
import { MAX_LOOP_ITERATIONS } from "../definition.ts";
import { registerBlock } from "./registry.ts";

const conditionGroup = z.unknown().superRefine((value, ctx) => {
  try {
    parseFilterGroup(value);
  } catch (error) {
    ctx.addIssue({
      code: "custom",
      message: error instanceof Error ? error.message : String(error),
      path: ["conditions"],
    });
  }
});

export const filterConfigSchema = z.object({ conditions: conditionGroup });
export type FilterBlockConfig = z.infer<typeof filterConfigSchema>;

export const branchConfigSchema = z.object({ conditions: conditionGroup });
export type BranchBlockConfig = z.infer<typeof branchConfigSchema>;

export const loopConfigSchema = z.object({
  until: conditionGroup.optional(),
  maxIterations: z.number().int().min(1).max(MAX_LOOP_ITERATIONS),
});
export type LoopBlockConfig = z.infer<typeof loopConfigSchema>;

export function registerControlBlocks(): void {
  registerBlock<FilterBlockConfig>({
    type: "filter",
    configSchema: filterConfigSchema,
  });
  registerBlock<BranchBlockConfig>({
    type: "branch",
    outputs: ["taken"],
    configSchema: branchConfigSchema,
  });
  registerBlock<LoopBlockConfig>({
    type: "loop",
    outputs: ["iterations", "exhausted"],
    configSchema: loopConfigSchema,
  });
}
