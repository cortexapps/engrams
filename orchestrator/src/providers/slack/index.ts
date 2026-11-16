import type { ProviderSpec } from "../provider.ts";
import { SLACK_ACTIONS } from "./actions.ts";
import { SLACK_CHAT } from "./chat.ts";
import { SLACK_PROVIDER } from "./client.ts";
import { slackEventSource } from "./events.ts";

export const SLACK: ProviderSpec = {
  provider: SLACK_PROVIDER,
  chat: SLACK_CHAT,
  events: slackEventSource,
  actions: SLACK_ACTIONS,
};
