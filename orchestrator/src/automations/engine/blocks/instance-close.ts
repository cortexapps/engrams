/** `instance_close` (ADR 0120): close the run's own workstream. Admission's
 * require-open then drops later events for it (audited), the handle ledger
 * keeps routing history but never re-routes, and the next kickoff of the
 * same key mints a FRESH instance.
 *
 * Deliberately narrow: a run closes only the workstream it is bound to —
 * never a sibling by key or id (no cross-instance authority from inside a
 * run). On a non-instanced run it is a warned no-op, so a shared template
 * degrades instead of failing.
 */

import { z } from "zod";

import { registerBlock } from "./registry.ts";

export const instanceCloseConfigSchema = z.object({
  /** Templated close reason, kept on the instance row for the audit trail. */
  reason: z.string().max(500).optional(),
});
export type InstanceCloseConfig = z.infer<typeof instanceCloseConfigSchema>;

export function registerInstanceCloseBlock(): void {
  registerBlock<InstanceCloseConfig>({
    type: "instance_close",
    outputs: ["closed", "not_instanced"],
    configSchema: instanceCloseConfigSchema,
    async execute(config, ctx) {
      if (ctx.instanceId === "") {
        // Not bound to a workstream: nothing to close. A no-op output, not
        // an error — the block may sit in a template shared with
        // non-instanced automations.
        return { kind: "ok", outputs: { closed: false, not_instanced: true } };
      }
      if (ctx.dryRun) {
        return {
          kind: "ok",
          outputs: {
            closed: true,
            not_instanced: false,
            dry_run: true,
            would_execute: { instance_id: ctx.instanceId, ...(config.reason !== undefined ? { reason: config.reason } : {}) },
          },
        };
      }
      const ops = ctx.deps.instances;
      if (!ops) {
        return {
          kind: "error",
          code: "instance_ops_unavailable",
          message: "the instance store is not installed",
          retryable: false,
        };
      }
      // false = already closed (an earlier attempt or a manual close):
      // idempotent success, the run's intent holds either way.
      const closed = await ops.closeInstance({
        instanceId: ctx.instanceId,
        ...(config.reason !== undefined ? { reason: config.reason } : {}),
      });
      return { kind: "ok", outputs: { closed, not_instanced: false } };
    },
  });
}
