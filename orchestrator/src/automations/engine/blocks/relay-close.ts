/** `relay_close` — the closing message of a relayed session, for a
 * `settings.onFinalize` hook (ADR 0119 phase 4.6; a catalog block since
 * 2026-09). Renders the run's terminal state into the relayed place exactly
 * the way the legacy Slack loop's terminal arms did, through the same policy:
 *
 *   completed          → onComplete (✅, last message, asset recap)
 *   failed | deadline  → onFail (❌ + the run's error)
 *   halted | superseded | filtered → onNeutralClose (informational)
 *
 * Runs inside the finalize sequence (contract 2 hooks), so a throw here is
 * recorded on its own step and never changes the run's status. With no relay
 * installed (the run ended before the relay block) there is no thread route
 * to post to; the block reports `posted: false` rather than guessing.
 */

import { z } from "zod";

import { registerBlock } from "./registry.ts";
import { slackRelayFinalFacts, slackRelayPolicy } from "./relay.ts";

export const RELAY_CLOSE_TYPE = "relay_close";

export const slackRecapConfigSchema = z.object({
  /** The run's terminal status, templated from `${{ run.status }}`. */
  status: z.string().min(1),
  /** Optional text for the failure/neutral message; defaults to the run's
   * error or a fixed sentence. Tunable on the built-in. */
  message: z.string().optional(),
});
export type SlackRecapConfig = z.infer<typeof slackRecapConfigSchema>;

export interface SlackRecapDeps {
  facts: typeof slackRelayFinalFacts;
  policy: typeof slackRelayPolicy;
}

let runtimeDeps: SlackRecapDeps | null = null;

/** Test seam; production uses the relay's state + policy. */
export function setSlackRecapDeps(deps: SlackRecapDeps | null): void {
  runtimeDeps = deps;
}

function deps(): SlackRecapDeps {
  return runtimeDeps ?? { facts: slackRelayFinalFacts, policy: slackRelayPolicy };
}

export function registerRelayCloseBlock(): void {
  registerBlock<SlackRecapConfig>({
    type: RELAY_CLOSE_TYPE,
    refusesDryRun: true,
    outputs: ["posted", "rendered_as"],
    configSchema: slackRecapConfigSchema,
    async execute(config, ctx) {
      const facts = deps().facts(ctx);
      if (!facts) {
        return { kind: "ok", outputs: { posted: false, rendered_as: "none" } };
      }
      const policy = deps().policy(ctx.runId);
      const runError = ctx.terminal?.error;
      switch (config.status) {
        case "completed": {
          await policy.onComplete(facts.mention, facts.session, facts.summary);
          return { kind: "ok", outputs: { posted: true, rendered_as: "complete" } };
        }
        case "failed":
        case "deadline": {
          await policy.onFail(
            facts.mention,
            config.message ?? runError ?? "The session hit an error and the thread cannot continue.",
          );
          return { kind: "ok", outputs: { posted: true, rendered_as: "fail" } };
        }
        default: {
          await policy.onNeutralClose(
            facts.mention,
            config.message ?? `This thread's run ended (${config.status}). The session is kept.`,
          );
          return { kind: "ok", outputs: { posted: true, rendered_as: "neutral" } };
        }
      }
    },
  });
}
