import type { ProviderSpec } from "../provider.ts";
import { vantaEventSource } from "./events.ts";

export const VANTA: ProviderSpec = { provider: "vanta", events: vantaEventSource };
