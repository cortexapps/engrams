/** `relay_close` — the closing message of a relayed session, for a
 * `settings.onFinalize` hook (ADR 0119 phase 4.6; a catalog block since
 * 2026-09). Renders the run's terminal state into the relayed place exactly
 * the way the legacy Slack loop's terminal arms did, through the same policy,
 * then removes the session's relay (a later run re-installs it):
 *
 *   completed          → onComplete (✅, last message, asset recap)
 *   failed | deadline  → onFail (❌ + the run's error)
 *   halted | superseded | filtered → onNeutralClose (informational)
 *
 * Runs inside the finalize sequence (contract 2 hooks), so a throw here is
 * recorded on its own step and never changes the run's status. With no relay
 * on the session (the run ended before installing one) there is no thread
 * route to post to; the block reports `posted: false` rather than guessing.
 */

import { z } from "zod";

import { makeSlackPolicy } from "../../../integrations/slack-policy.ts";
import { sessionWebUrl } from "../../../links.ts";
import { registerBlock } from "./registry.ts";
import { sessionRefSchema } from "../definition.ts";
import type { RunContext } from "../context.ts";
import { ownPath } from "../../paths.ts";
import { relayFactsOf } from "../../session-relay.ts";

export const RELAY_CLOSE_TYPE = "relay_close";

export const slackRecapConfigSchema = z.object({
  /** The relayed session. Read without adoption: a finalize hook posts
   * about a session, it never takes routing over. */
  session: sessionRefSchema,
  /** The run's terminal status, templated from `${{ run.status }}`. */
  status: z.string().min(1),
  /** Optional text for the failure/neutral message; defaults to the run's
   * error or a fixed sentence. Tunable on the built-in. */
  message: z.string().optional(),
});
export type SlackRecapConfig = z.infer<typeof slackRecapConfigSchema>;

/** The session id a hook names, without adoption (see the config doc). A
 * ref that does not resolve (the run was filtered before any session) is
 * no session: the hook has nothing to post to. */
async function sessionIdOf(config: SlackRecapConfig, ctx: RunContext): Promise<string | null> {
  if ("blockId" in config.session) {
    const id = ownPath(ctx.steps, `${config.session.blockId}.session_id`);
    return typeof id === "string" && id !== "" ? id : null;
  }
  try {
    const rendered = await ctx.render(config.session.template);
    return rendered === "" ? null : rendered;
  } catch {
    return null;
  }
}

export function registerRelayCloseBlock(): void {
  registerBlock<SlackRecapConfig>({
    type: RELAY_CLOSE_TYPE,
    refusesDryRun: true,
    outputs: ["posted", "rendered_as"],
    configSchema: slackRecapConfigSchema,
    async execute(config, ctx) {
      const store = ctx.deps.relays;
      if (!store) {
        return { kind: "error", code: "relay_store_unavailable", message: "the session relay store is not installed", retryable: false };
      }
      const sessionId = await sessionIdOf(config, ctx);
      const row = sessionId === null ? null : await store.get(sessionId);
      if (row === null) {
        return { kind: "ok", outputs: { posted: false, rendered_as: "none" } };
      }
      const facts = relayFactsOf(row, { sessionWebUrl });
      const policy = makeSlackPolicy({ routeExtras: { sessionId: row.sessionId } });
      const runError = ctx.terminal?.error;
      let renderedAs: "complete" | "fail" | "neutral";
      switch (config.status) {
        case "completed":
          await policy.onComplete(facts.mention, facts.session, facts.summary);
          renderedAs = "complete";
          break;
        case "failed":
        case "deadline":
          await policy.onFail(
            facts.mention,
            config.message ?? runError ?? "The session hit an error and the thread cannot continue.",
          );
          renderedAs = "fail";
          break;
        default:
          await policy.onNeutralClose(
            facts.mention,
            config.message ?? `This thread's run ended (${config.status}). The session is kept.`,
          );
          renderedAs = "neutral";
      }
      await store.remove(row.sessionId);
      return { kind: "ok", outputs: { posted: true, rendered_as: renderedAs } };
    },
  });
}
