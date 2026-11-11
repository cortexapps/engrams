/** Model calls the orchestrator makes itself, through a connected router.
 *
 * The coordinator unseals the router's key; `integrations/clients.ts` caches
 * the client built from it and picks up a key rotation or a host change
 * without a restart.
 */

import { Code, ConnectError } from "@connectrpc/connect";
import type { LanguageModel } from "ai";

import { asBearer, getIntegrationClient, registerIntegrationClient } from "../integrations/clients.ts";
import { firstConnectedRouter } from "./connected.ts";
import {
  DecisionsApiError,
  type DecisionsClient,
  type DecisionsRequest,
  type DecisionsResponse,
} from "./decisions.ts";
import { listModelRouterDefinitions, type ModelRouterDefinition } from "./registry.ts";

interface RouterClient {
  chat(modelId: string): LanguageModel;
  decisions?: DecisionsClient;
}

for (const router of listModelRouterDefinitions()) {
  registerIntegrationClient(router.connector, (cred, host): RouterClient => {
    const apiKey = asBearer(cred);
    const decisions = router.decisions?.(apiKey, host);
    return { chat: router.chat(apiKey, host), ...(decisions ? { decisions } : {}) };
  });
}

function clientFor(router: ModelRouterDefinition): Promise<RouterClient> {
  return getIntegrationClient<RouterClient>(router.connector);
}

/** The model that titles a task, on the first connected router; null when no
 * router is connected. */
export async function titleModel(): Promise<LanguageModel | null> {
  const router = await firstConnectedRouter();
  return router ? (await clientFor(router)).chat(router.titleModel) : null;
}

/** The first connected router that can answer a decisions request. */
export function decisionsRouter(): Promise<ModelRouterDefinition | null> {
  return firstConnectedRouter((router) => router.decisions !== undefined);
}

/** Control-plane codes a key lookup can recover from on a retry. */
const TRANSIENT_LOOKUP_CODES = new Set([
  Code.Unavailable,
  Code.DeadlineExceeded,
  Code.ResourceExhausted,
  Code.Aborted,
  Code.Internal,
]);

/** Answer a decisions request on the first connected router that can; null
 * when there is none. A transient failure to resolve the key is a retryable
 * DecisionsApiError; a missing or rejected key stays permanent. */
export async function decide(request: DecisionsRequest): Promise<DecisionsResponse | null> {
  const router = await decisionsRouter();
  if (!router) return null;
  let client: RouterClient;
  try {
    client = await clientFor(router);
  } catch (error) {
    if (error instanceof ConnectError && TRANSIENT_LOOKUP_CODES.has(error.code)) {
      throw new DecisionsApiError(`resolve the ${router.label} key: ${error.rawMessage}`, null, true);
    }
    throw error;
  }
  if (!client.decisions) {
    throw new DecisionsApiError(`${router.label} has no decisions endpoint`, null, false);
  }
  return client.decisions.decide(request);
}
