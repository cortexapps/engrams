import type { ProviderSpec } from "../provider.ts";
import { LINEAR_ACTIONS } from "./actions.ts";
import { linearEventSource } from "./events.ts";

export const LINEAR: ProviderSpec = {
  provider: "linear",
  events: linearEventSource,
  actions: LINEAR_ACTIONS,
};
