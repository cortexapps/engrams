/** `relay_session` — mirror a session into a place (a Slack thread).
 *
 * Since contract 5 the relay is bound to the SESSION (`automations/
 * session-relay.ts`): this block installs it, or re-points an installed one
 * at the mention that starts a new turn, inside one checkpointed step, and
 * the run goes on (or ends). The automation consumer renders the session's
 * events into the thread from then on, with no run live. Slack is the only
 * provider so far; the config's `provider` enum is where the next one lands.
 */

import { z } from "zod";

import { makeSlackPolicy } from "../../../integrations/slack-policy.ts";
import type { CommunicationPolicy, SourceMention } from "../../../workflows/communication-policy.ts";
import { canonicalHandle } from "../../handles.ts";
import { installSessionRelay } from "../../session-relay.ts";
import { sessionRefSchema } from "../definition.ts";
import type { RunContext } from "../context.ts";
import { registerBlock, type BlockOutcome } from "./registry.ts";

export { CHOICE_ID_PREFIX, SLACK_ANSWER_SIGNAL } from "../../session-relay.ts";

export const RELAY_SESSION_TYPE = "relay_session";

export const slackRelayConfigSchema = z.object({
  session: sessionRefSchema,
  /** The place the session is mirrored into. Slack is the only provider so
   * far; the thread coordinates below are Slack's. */
  provider: z.enum(["slack"]).default("slack"),
  team: z.string().min(1),
  channel: z.string().min(1),
  /** The thread root ts (the mention that opened the thread). */
  threadTs: z.string().min(1),
  /** The mention ts the current turn reacts on; defaults to threadTs. */
  mentionTs: z.string().min(1).optional(),
  userId: z.string().min(1).optional(),
  eventId: z.string().min(1).optional(),
});
export type SlackRelayConfig = z.infer<typeof slackRelayConfigSchema>;

export function mentionFromRelayConfig(config: Omit<SlackRelayConfig, "session">): SourceMention {
  return {
    team: config.team,
    channel: config.channel,
    threadRoot: config.threadTs,
    user: config.userId ?? "",
    ts: config.mentionTs ?? config.threadTs,
    eventId: config.eventId ?? `${config.channel}:${config.threadTs}`,
  };
}

/** Install or re-point the relay for a resolved session. Shared with
 * `run_agent`. */
export async function installRelayForSession(
  sessionId: string,
  config: Omit<SlackRelayConfig, "session">,
  ctx: RunContext,
): Promise<BlockOutcome> {
  const mention = mentionFromRelayConfig(config);
  if (ctx.dryRun) {
    return {
      kind: "ok",
      outputs: { session_id: sessionId, installed: false, mention_ts: mention.ts, dry_run: true, would_execute: { target: mention } },
    };
  }
  const store = ctx.deps.relays;
  if (!store) {
    return { kind: "error", code: "relay_store_unavailable", message: "the session relay store is not installed", retryable: false };
  }
  const { created, previous } = await installSessionRelay(
    { sessionId, automationId: ctx.automationId, target: mention },
    { store },
  );
  // The place the session talks into is the workstream's: the handle
  // ledger routes later events for the thread to it and the UI names the
  // workstream by it (what the retired "Started a session" post recorded
  // through the action runtime). Idempotent (`already_ours` on a replay);
  // a conflict (another workstream holds the thread) is reported, never a
  // rebind and never a failed turn.
  let handle: string | undefined;
  if (ctx.instanceId !== "" && ctx.deps.instances?.recordInstanceHandle) {
    handle = canonicalHandle("slack", `slack:${mention.channel}:${mention.threadRoot}`);
    const result = await ctx.deps.instances.recordInstanceHandle({
      automationId: ctx.automationId,
      handle,
      instanceId: ctx.instanceId,
      writtenBy: `${ctx.runId}:${ctx.currentPath ?? ctx.currentBlockId ?? ""}`,
    });
    if (result.kind === "conflict") handle = undefined;
  }
  return {
    kind: "ok",
    outputs: {
      session_id: sessionId,
      installed: true,
      created,
      mention_ts: mention.ts,
      ...(handle !== undefined ? { handle } : {}),
      // The target a re-point replaced; the agent step restores it when
      // the turn fails to reach the session.
      ...(previous !== undefined ? { previous_target: previous } : {}),
    },
  };
}

/** A run-addressed Slack policy for the `slack_choice` card: its click is
 * the RUN's answer (the choice block waits for it), so `runId` rides the
 * route instead of a session id. */
export function slackRelayPolicy(runId: string): CommunicationPolicy {
  return makeSlackPolicy({ routeExtras: { runId } });
}

export function registerSlackRelayBlock(): void {
  registerBlock<SlackRelayConfig>({
    type: RELAY_SESSION_TYPE,
    outputs: ["session_id", "installed", "created", "mention_ts"],
    configSchema: slackRelayConfigSchema,
    async execute(config, ctx) {
      const sessionId = await ctx.resolveSession(config.session);
      return installRelayForSession(sessionId, config, ctx);
    },
  });
}
