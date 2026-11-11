/** Slack as a chat provider. */

import type { ChatProviderSpec } from "../../chat/chat.ts";
import { describeSlackChannels, listSlackChannelOptions } from "./channels.ts";
import { getSlackClient } from "./client.ts";
import { makeSlackInteractivityRoute } from "./interactivity.ts";
import { makeSlackPolicy } from "./policy.ts";
import { SLACK_THREADS } from "./threads-variant.ts";

const PROVIDER = "slack";

export const SLACK_CHAT: ChatProviderSpec = {
  provider: PROVIDER,
  legacyDefault: true,
  make: () => ({
    provider: PROVIDER,
    policy: (route) => makeSlackPolicy({ routeExtras: route }),
    /** Needs the `users:read.email` scope. */
    async userEmail(externalUserId) {
      const response = await (await getSlackClient()).users.info({ user: externalUserId });
      return response.user?.profile?.email || null;
    },
    listChannels: async () => listSlackChannelOptions(await getSlackClient()),
    describeChannels: async (keys) => describeSlackChannels(await getSlackClient(), keys),
    threadHandle: (place) => `${PROVIDER}:${place.channel}:${place.threadRoot}`,
  }),
  interactivity: makeSlackInteractivityRoute,
  threads: SLACK_THREADS,
};
