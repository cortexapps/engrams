/** `lookup_pr_session` (Builder v2 follow-up): map a PR to the session that
 * authored it, via the pr_ref ledger the link consumer maintains for every
 * session that opens a PR.
 *
 * The routing block for "review feedback goes back to the implementer":
 * a review-comment entrypoint looks the PR up, then a `send_prompt` with a
 * `{template}` session ref adopts the session (D11) and delivers the
 * feedback. Read-only — "not found" is a value a filter branches on, and
 * dry runs read live.
 */

import { z } from "zod";

import { registerBlock } from "./registry.ts";

export const lookupPrSessionConfigSchema = z.object({
  /** owner/name, templated (e.g. `${{ event.raw.repository.full_name }}`). */
  repo: z.string().min(1).max(300),
  /** Usually a `$ref` to the event's PR number. */
  prNumber: z.number().int().min(1),
});
export type LookupPrSessionConfig = z.infer<typeof lookupPrSessionConfigSchema>;

export function registerPrLookupBlock(): void {
  registerBlock<LookupPrSessionConfig>({
    type: "lookup_pr_session",
    outputs: ["found", "session_id", "task_id", "head_branch", "url", "title"],
    configSchema: lookupPrSessionConfigSchema,
    async execute(config, ctx) {
      const prRefs = ctx.deps.prRefs;
      if (!prRefs) {
        return {
          kind: "error",
          code: "pr_ref_lookup_unavailable",
          message: "the PR reference lookup is not installed",
          retryable: false,
        };
      }
      const ref = await prRefs.getByPr(config.repo, config.prNumber);
      if (!ref) {
        return {
          kind: "ok",
          outputs: { found: false, session_id: "", task_id: "" },
        };
      }
      return {
        kind: "ok",
        outputs: {
          found: true,
          session_id: ref.sessionId,
          task_id: ref.taskId ?? "",
          head_branch: ref.headBranch,
          url: ref.url,
          title: ref.title,
        },
      };
    },
  });
}
