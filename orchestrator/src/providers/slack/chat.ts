/** Slack as a chat provider. */

import type { WebClient } from "@slack/web-api";

import type { ChatProvider, ChatProviderSpec } from "../../chat/chat.ts";
import { describeSlackChannels, listSlackChannelOptions } from "./channels.ts";
import { getSlackClient, SLACK_PROVIDER } from "./client.ts";
import { makeSlackInteractivityRoute } from "./interactivity.ts";
import { makeSlackPolicy } from "./policy.ts";
import { SLACK_THREADS } from "./threads-variant.ts";

export const SLACK_CHAT: ChatProviderSpec = {
  provider: SLACK_PROVIDER,
  legacyDefault: true,
  make: () => makeSlackChat(),
  interactivity: makeSlackInteractivityRoute,
  threads: SLACK_THREADS,
};

export function makeSlackChat(client: () => Promise<WebClient> = getSlackClient): ChatProvider {
  return {
    provider: SLACK_PROVIDER,
    policy: (route) => makeSlackPolicy({ routeExtras: route }),
    /** Needs the `users:read.email` scope. */
    async userEmail(externalUserId) {
      const response = await (await client()).users.info({ user: externalUserId });
      return response.user?.profile?.email || null;
    },
    listChannels: async () => listSlackChannelOptions(await client()),
    describeChannels: async (keys) => describeSlackChannels(await client(), keys),
    threadHandle: (place) => `${SLACK_PROVIDER}:${place.channel}:${place.threadRoot}`,
  };
}
