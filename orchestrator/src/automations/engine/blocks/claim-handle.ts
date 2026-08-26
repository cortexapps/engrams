/** `claim_handle` (ADR 0120): the explicit escape hatch onto the handle
 * ledger. The automatic writers (the action executor, the pr-link
 * consumer) cover the common cases; this block claims an identifier the
 * run learned some other way — a channel the kickoff named
 * (`slack:<channel>`, the rung-2 shape), an external ticket id, a thread
 * discovered by a code block.
 *
 * Same contract as every writer: `already_ours` converges a replayed
 * step, and a handle another workstream owns is a typed non-retryable
 * error — routing is never silently rebound. Case-folding follows the
 * shared provider rule by sniffing the namespace prefix (`github:…`
 * folds; `slack:…` stays exact), so a claim and a later admission match
 * can never disagree by case.
 */

import { z } from "zod";

import { canonicalHandle } from "../../handles.ts";
import { registerBlock } from "./registry.ts";

export const claimHandleConfigSchema = z.object({
  /** Templated full handle, namespace prefix included
   * (e.g. `slack:${{ inputs.channel_id }}`). */
  handle: z.string().min(1).max(512),
});
export type ClaimHandleConfig = z.infer<typeof claimHandleConfigSchema>;

export function registerClaimHandleBlock(): void {
  registerBlock<ClaimHandleConfig>({
    type: "claim_handle",
    outputs: ["claimed", "handle"],
    configSchema: claimHandleConfigSchema,
    async execute(config, ctx) {
      if (ctx.instanceId === "") {
        return {
          kind: "error",
          code: "not_instanced",
          message:
            "claim_handle needs a workstream-bound run (declare settings.instance); an unbound run has nothing to route to",
          retryable: false,
        };
      }
      const provider = config.handle.split(":", 1)[0] ?? "";
      const handle = canonicalHandle(provider, config.handle);
      if (ctx.dryRun) {
        return {
          kind: "ok",
          outputs: { claimed: true, handle, dry_run: true, would_execute: { handle } },
        };
      }
      const ops = ctx.deps.instances;
      if (!ops?.recordInstanceHandle) {
        return {
          kind: "error",
          code: "instance_ops_unavailable",
          message: "the instance store is not installed",
          retryable: false,
        };
      }
      const result = await ops.recordInstanceHandle({
        automationId: ctx.automationId,
        handle,
        instanceId: ctx.instanceId,
        writtenBy: `${ctx.runId}:${ctx.currentPath}`,
      });
      if (result.kind === "conflict") {
        return {
          kind: "error",
          code: "handle_conflict",
          message: `handle "${handle}" already routes to workstream ${result.instanceId} — close or reuse that workstream instead of rebinding`,
          retryable: false,
        };
      }
      return { kind: "ok", outputs: { claimed: true, handle } };
    },
  });
}
