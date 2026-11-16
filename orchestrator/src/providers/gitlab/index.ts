import type { ProviderSpec } from "../provider.ts";
import { gitlabEventSource } from "./events.ts";
import { GITLAB_FORGE } from "./forge.ts";

export const GITLAB: ProviderSpec = {
  provider: "gitlab",
  forge: GITLAB_FORGE,
  events: gitlabEventSource,
};
