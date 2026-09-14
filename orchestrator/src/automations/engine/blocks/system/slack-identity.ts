/** `system.slack_resolve_user` — the Slack thread brain's identity gate
 * (ADR 0119 phase 4.6, legacy `slack-thread.ts` resolveUser).
 *
 * The legacy workflow refused to start a session for a Slack user who has
 * no engrams account: it resolved the Slack user to an engrams user by
 * email (ADR 0060 D4) and posted "log in first" otherwise. That gate is
 * what makes a thread's session OWNED by the person who asked — it runs
 * with their credentials and git attribution, not with the harness's
 * programmatic org credential — so a stranger in a flagged channel cannot
 * drive a session on the org's behalf.
 *
 * Linked → outputs `user_id`; `create_session` passes it through as the
 * session's `ownerUserId`. Unlinked → the block posts the legacy message to
 * the thread through the same Slack policy and ENDS the run `filtered` (no
 * session, no relay — the recap has nothing to post, which is correct).
 */

import { z } from "zod";

import { NO_USER_MSG, resolveEngramsUser } from "../../../../integrations/slack-identity.ts";
import type { SourceMention } from "../../../../workflows/thread-inbox.ts";
import { registerBlock } from "../registry.ts";
import { slackRelayPolicy } from "./slack-relay.ts";

export const SLACK_IDENTITY_TYPE = "system.slack_resolve_user";

export const slackIdentityConfigSchema = z.object({
  /** The Slack user id of the mention's author. */
  userId: z.string(),
  team: z.string().min(1),
  channel: z.string().min(1),
  threadTs: z.string().min(1),
  mentionTs: z.string().min(1).optional(),
  eventId: z.string().min(1).optional(),
});
export type SlackIdentityConfig = z.infer<typeof slackIdentityConfigSchema>;

export interface SlackIdentityDeps {
  resolveUser(provider: string, externalUserId: string): Promise<string | null>;
  policy: typeof slackRelayPolicy;
}

let runtimeDeps: SlackIdentityDeps | null = null;

/** Test seam; production resolves by Slack profile email. */
export function setSlackIdentityDeps(deps: SlackIdentityDeps | null): void {
  runtimeDeps = deps;
}

function deps(): SlackIdentityDeps {
  return (
    runtimeDeps ?? {
      resolveUser: (provider, externalUserId) => resolveEngramsUser(provider, externalUserId),
      policy: slackRelayPolicy,
    }
  );
}

export function registerSlackIdentityBlock(): void {
  registerBlock<SlackIdentityConfig>({
    type: SLACK_IDENTITY_TYPE,
    system: true,
    refusesDryRun: true,
    outputs: ["user_id", "linked"],
    configSchema: slackIdentityConfigSchema,
    async execute(config, ctx) {
      const userId =
        config.userId === "" ? null : await deps().resolveUser("slack", config.userId);
      if (userId !== null) {
        return { kind: "ok", outputs: { user_id: userId, linked: true } };
      }
      const mention: SourceMention = {
        team: config.team,
        channel: config.channel,
        threadRoot: config.threadTs,
        user: config.userId,
        ts: config.mentionTs ?? config.threadTs,
        eventId: config.eventId ?? `${config.channel}:${config.threadTs}`,
      };
      await deps().policy(ctx.runId).onFail(mention, NO_USER_MSG);
      return { kind: "end_run", status: "filtered", reason: "slack user is not linked to an engrams user" };
    },
  });
}
