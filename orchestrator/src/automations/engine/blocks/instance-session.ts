/** `lookup_instance_session` (ADR 0120 + D8): the kept session of this
 * workstream's earlier runs, so a new run in the same workstream continues
 * it instead of starting over. The Slack brain's thread continuity: the run
 * that answered a thread ends when the thread goes quiet, the session is
 * kept, and the next mention in the thread — a new run in the SAME
 * workstream — resumes that session.
 *
 * Read-only. "not found" is a value a branch acts on: no earlier run kept a
 * session (`none`), the kept session is gone (`gone` — swept, deleted), or
 * the run is not bound to a workstream (`unbound`). A `send_prompt` or a
 * relay with a `{template}` session ref adopts the found session (D11): the
 * binding row moves to this run because its owner is terminal and in the
 * same workstream.
 */

import { z } from "zod";

import { sessionWebUrl } from "../../../links.ts";
import { registerBlock } from "./registry.ts";

export const LOOKUP_INSTANCE_SESSION_TYPE = "lookup_instance_session";

export const lookupInstanceSessionConfigSchema = z.object({});
export type LookupInstanceSessionConfig = z.infer<typeof lookupInstanceSessionConfigSchema>;

export function registerInstanceSessionLookupBlock(): void {
  registerBlock<LookupInstanceSessionConfig>({
    type: LOOKUP_INSTANCE_SESSION_TYPE,
    outputs: ["found", "session_id", "web_url", "run_id", "reason"],
    configSchema: lookupInstanceSessionConfigSchema,
    async execute(_config, ctx) {
      // A dry run binds to no workstream and must not read live rows as
      // though it did.
      if (ctx.dryRun) return { kind: "ok", outputs: { found: false, reason: "dry_run" } };
      if (ctx.instanceId === "")
        return { kind: "ok", outputs: { found: false, reason: "unbound" } };
      const previous = await ctx.deps.store.latestKeptInstanceSession(ctx.instanceId, ctx.runId);
      if (previous === null) return { kind: "ok", outputs: { found: false, reason: "none" } };
      // The row says kept; the control plane says whether it still exists.
      const probe = await ctx.deps.sessions.getSession(previous.sessionId);
      if (!probe.found) {
        return {
          kind: "ok",
          outputs: {
            found: false,
            reason: "gone",
            session_id: previous.sessionId,
            run_id: previous.runId,
          },
        };
      }
      return {
        kind: "ok",
        outputs: {
          found: true,
          session_id: previous.sessionId,
          web_url: sessionWebUrl(previous.sessionId),
          run_id: previous.runId,
        },
      };
    },
  });
}
