/**
 * OpenRouter Mode B client (ADR 0106): the coordinator unseals the org secret
 * `openrouter.api_key` (declared in connectors/openrouter.json) and this
 * adapter maps it onto the Vercel AI SDK provider (chat models) and the
 * Decisions API client (System One models). clients.ts caches both and picks
 * up a key rotation without a restart.
 */

import { createOpenRouter, type OpenRouterProvider } from "@openrouter/ai-sdk-provider";

import { asBearer, getIntegrationClient, registerIntegrationClient } from "./clients.ts";
import { makeDecisionsClient, type DecisionsClient } from "./openrouter-decisions.ts";

interface OpenRouterClients {
  sdk: OpenRouterProvider;
  decisions: DecisionsClient;
}

registerIntegrationClient("openrouter", (cred): OpenRouterClients => {
  const apiKey = asBearer(cred);
  return { sdk: createOpenRouter({ apiKey }), decisions: makeDecisionsClient({ apiKey }) };
});

/** A ready OpenRouter AI-SDK provider, authenticated with the org key. */
export async function getOpenRouterClient(): Promise<OpenRouterProvider> {
  return (await getIntegrationClient<OpenRouterClients>("openrouter")).sdk;
}

/** A ready Decisions API client, authenticated with the org key. */
export async function getOpenRouterDecisions(): Promise<DecisionsClient> {
  return (await getIntegrationClient<OpenRouterClients>("openrouter")).decisions;
}
