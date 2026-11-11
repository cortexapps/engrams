/** `relay_close` — the closing message of a relayed session, for a
 * `settings.onFinalize` hook (ADR 0119). Renders the run's terminal state
 * into the relayed place through the chat provider's policy, then removes
 * the session's relay (a later run re-installs it). A failed post keeps the
 * relay until the session ends:
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

import { registerBlock } from "./registry.ts";
import { sessionRefSchema } from "../definition.ts";
import type { RunContext } from "../context.ts";
import { ownPath } from "../../paths.ts";
import { closeRelay, SESSION_FAILED_MSG, type RelayClosing } from "../../session-relay.ts";

export const RELAY_CLOSE_TYPE = "relay_close";

export const relayCloseConfigSchema = z.object({
  /** The relayed session. Read without adoption: a finalize hook posts
   * about a session, it never takes routing over. */
  session: sessionRefSchema,
  /** The run's terminal status, templated from `${{ run.status }}`. */
  status: z.string().min(1),
  /** Optional text for the failure/neutral message; defaults to the run's
   * error or a fixed sentence. Tunable on the built-in. */
  message: z.string().optional(),
});
export type RelayCloseConfig = z.infer<typeof relayCloseConfigSchema>;

/** The session id a hook names, without adoption (see the config doc). A
 * ref that does not resolve (the run was filtered before any session) is
 * no session: the hook has nothing to post to. */
async function sessionIdOf(config: RelayCloseConfig, ctx: RunContext): Promise<string | null> {
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
  registerBlock<RelayCloseConfig>({
    type: RELAY_CLOSE_TYPE,
    refusesDryRun: true,
    outputs: ["posted", "rendered_as"],
    configSchema: relayCloseConfigSchema,
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
      const runError = ctx.terminal?.error;
      let closing: RelayClosing;
      switch (config.status) {
        case "completed":
          closing = { kind: "complete" };
          break;
        case "failed":
        case "deadline":
          closing = { kind: "fail", message: config.message ?? runError ?? SESSION_FAILED_MSG };
          break;
        default:
          closing = {
            kind: "neutral",
            message: config.message ?? `This thread's run ended (${config.status}). The session is kept.`,
          };
      }
      await closeRelay(row, closing, { store });
      return { kind: "ok", outputs: { posted: true, rendered_as: closing.kind } };
    },
  });
}
