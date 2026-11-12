/** Slack's built-in automation actions. */

import type { KnownBlock } from "@slack/types";

import type { BuiltinActionTable } from "../../automations/actions/builtin.ts";
import { optionalString, requireString } from "../../automations/actions/params.ts";
import { buildMessageBlocks } from "./blocks.ts";
import { replyText, type SlackReply } from "./message-text.ts";

/** The narrow Slack surface the builtins use (structural, fake-friendly;
 * WebClient satisfies it because these argument shapes inhabit its unions). */
export interface SlackChatClient {
  chat: {
    postMessage(args: {
      channel: string;
      text: string;
      thread_ts?: string;
    }): Promise<{ ts?: string; channel?: string }>;
    update(args: {
      channel: string;
      ts: string;
      text: string;
      blocks: KnownBlock[];
    }): Promise<{ ts?: string; channel?: string }>;
  };
  conversations: {
    join(args: { channel: string }): Promise<{ channel?: { id?: string } }>;
    replies(args: {
      channel: string;
      ts: string;
      oldest?: string;
      limit?: number;
    }): Promise<{ messages?: SlackReply[] }>;
  };
}

export const SLACK_ACTIONS: BuiltinActionTable = {
  "slack.post_message": async (params, _ctx, deps) => {
    const client = await deps.slackClient();
    const threadTs = optionalString(params, "threadTs");
    const response = await client.chat.postMessage({
      channel: requireString(params, "channel"),
      text: requireString(params, "text"),
      ...(threadTs !== undefined ? { thread_ts: threadTs } : {}),
    });
    return { ts: response.ts ?? null, channel: response.channel ?? null };
  },

  "slack.join_channel": async (params, _ctx, deps) => {
    // Rung 2 companion: message events only flow where the bot is a channel
    // MEMBER, so a flow that claims `slack:<channel>` joins first.
    // conversations.join is idempotent for public channels (already-in →
    // ok with a warning); private channels need a human /invite and fail
    // here with Slack's typed error.
    const client = await deps.slackClient();
    const response = await client.conversations.join({
      channel: requireString(params, "channel"),
    });
    return { channel: response.channel?.id ?? null };
  },

  "slack.list_replies": async (params, _ctx, deps) => {
    // One page of a thread, every content surface of each message rendered
    // to text (text, blocks, attachments, files — the legacy `replyText`).
    // The caller folds it into a prompt; this action only reads.
    const client = await deps.slackClient();
    const oldest = optionalString(params, "oldest");
    const response = await client.conversations.replies({
      channel: requireString(params, "channel"),
      ts: requireString(params, "threadTs"),
      ...(oldest !== undefined ? { oldest } : {}),
      limit: 500,
    });
    return {
      // `subtype` is on every Slack message the API returns but not on the
      // SDK's reply type; widen the one field honestly.
      messages: (response.messages ?? []).map((m: SlackReply & { subtype?: string }) => ({
        ts: m.ts ?? "",
        user: m.user ?? "",
        bot_id: m.bot_id ?? "",
        subtype: m.subtype ?? "",
        text: replyText(m),
      })),
    };
  },

  "slack.update_message": async (params, _ctx, deps) => {
    const client = await deps.slackClient();
    // The message becomes exactly this text: its blocks are replaced too, so
    // updating an interactive card (a choice) removes its buttons.
    const text = requireString(params, "text");
    const response = await client.chat.update({
      channel: requireString(params, "channel"),
      ts: requireString(params, "ts"),
      text,
      blocks: buildMessageBlocks(text),
    });
    return { ts: response.ts ?? null, channel: response.channel ?? null };
  },
};
