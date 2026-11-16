import type { ProviderSpec } from "../provider.ts";
import { LITELLM_ROUTER } from "./router.ts";

export const LITELLM: ProviderSpec = { provider: "litellm", router: LITELLM_ROUTER };
