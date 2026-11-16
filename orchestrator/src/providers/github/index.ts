import type { ProviderSpec } from "../provider.ts";
import { githubEventSource } from "./events.ts";
import { GITHUB_FORGE } from "./forge.ts";

export const GITHUB: ProviderSpec = {
  provider: "github",
  forge: GITHUB_FORGE,
  events: githubEventSource,
};
