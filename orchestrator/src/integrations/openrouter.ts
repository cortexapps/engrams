/**
 * OpenRouter Mode B client (ADR 0106): the coordinator unseals the org secret
 * `openrouter.api_key` (declared in connectors/openrouter.json) and this
 * adapter maps it onto the Vercel AI SDK provider (chat models) and the
 * Decisions API client (System One models). clients.ts caches both and picks
 * up a key rotation without a restart.
 */

import { Code, ConnectError } from "@connectrpc/connect";
import { createOpenRouter, type OpenRouterProvider } from "@openrouter/ai-sdk-provider";

import { asBearer, getIntegrationClient, registerIntegrationClient } from "./clients.ts";
import { DecisionsApiError, makeDecisionsClient, type DecisionsClient } from "./openrouter-decisions.ts";

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

/** Control-plane codes a credential lookup can recover from on a retry. */
const TRANSIENT_LOOKUP_CODES = new Set([
  Code.Unavailable,
  Code.DeadlineExceeded,
  Code.ResourceExhausted,
  Code.Aborted,
  Code.Internal,
]);

/** A ready Decisions API client, authenticated with the org key. A
 * transient failure to resolve the key (the control plane briefly
 * unreachable) is a retryable DecisionsApiError, so the decide block's
 * retry applies; a missing or rejected key stays permanent. */
export async function getOpenRouterDecisions(): Promise<DecisionsClient> {
  try {
    return (await getIntegrationClient<OpenRouterClients>("openrouter")).decisions;
  } catch (error) {
    if (error instanceof ConnectError && TRANSIENT_LOOKUP_CODES.has(error.code)) {
      throw new DecisionsApiError(`resolve the OpenRouter key: ${error.rawMessage}`, null, true);
    }
    throw error;
  }
}
