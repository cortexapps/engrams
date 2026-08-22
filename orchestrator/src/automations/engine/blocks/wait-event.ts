/** wait_event: receive a joined trigger delivery mid-run (ADR 0119 D3/D4).
 *
 * With concurrency policy `join`, the dispatcher routes a later delivery for
 * the same key into the active run's mailbox instead of starting a new run.
 * This block consumes those messages — the Slack-thread "next message in the
 * thread" primitive.
 */

import { z } from "zod";

import { MAX_WAIT_DEADLINE_S } from "../definition.ts";

import { evaluateFilter, parseFilterGroup } from "../conditions.ts";
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

export const waitEventConfigSchema = z.object({
  eventKeys: z.array(z.string().min(1)).optional(),
  conditions: conditionGroup.optional(),
  deadlineSeconds: z.number().int().min(1).max(MAX_WAIT_DEADLINE_S).optional(),
  /** What a deadline means. `fail_run` (default) ends the run `deadline`, as
   * every wait did in phase 1. `continue` records `outcome: "deadline"` on
   * the step and lets the graph go on — a loop's `until` can read it, which
   * is how a conversation ends when its thread goes quiet. */
  onDeadline: z.enum(["fail_run", "continue"]).optional(),
});
export type WaitEventConfig = z.infer<typeof waitEventConfigSchema>;

const DEFAULT_EVENT_DEADLINE_S = 3600;

export function registerWaitEventBlock(): void {
  registerBlock<WaitEventConfig>({
    type: "wait_event",
    outputs: ["outcome", "event"],
    configSchema: waitEventConfigSchema,
    wait: {
      deadlineSeconds(config) {
        return config.deadlineSeconds ?? DEFAULT_EVENT_DEADLINE_S;
      },
      matches(msg, config) {
        if (msg.kind !== "event") return null;
        if (config.eventKeys && !config.eventKeys.includes(msg.eventKey)) return "ignore";
        if (config.conditions !== undefined) {
          const group = parseFilterGroup(config.conditions);
          if (!evaluateFilter(group, { event: msg.payload })) return "ignore";
        }
        return {
          outcome: "event",
          event_key: msg.eventKey,
          delivery_key: msg.deliveryKey,
          event: msg.payload,
          received_at: msg.receivedAt,
        };
      },
      onDeadline() {
        return { outcome: "deadline" };
      },
      // A dry run routes no events, so the wait "expires" at once; a loop's
      // `until` on outcome=deadline then exits the way a quiet thread does.
      dryRunOutcome() {
        return { outcome: "deadline" };
      },
    },
  });
}
