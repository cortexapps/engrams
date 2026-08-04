/**
 * OpenRouter Mode B client (ADR 0106): the coordinator unseals the org secret
 * `openrouter.api_key` (declared in connectors/openrouter.json) and this
 * adapter maps it onto the Vercel AI SDK provider. clients.ts caches the
 * instance and picks up a key rotation without a restart.
 */

import { createOpenRouter, type OpenRouterProvider } from "@openrouter/ai-sdk-provider";

import { asBearer, getIntegrationClient, registerIntegrationClient } from "./clients.ts";

registerIntegrationClient("openrouter", (cred) => createOpenRouter({ apiKey: asBearer(cred) }));

/** A ready OpenRouter AI-SDK provider, authenticated with the org key. */
export function getOpenRouterClient(): Promise<OpenRouterProvider> {
  return getIntegrationClient<OpenRouterProvider>("openrouter");
}
