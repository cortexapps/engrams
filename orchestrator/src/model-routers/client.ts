/** Model calls the orchestrator makes itself, through a connected router.
 *
 * The coordinator unseals the router's key; `integrations/clients.ts` caches
 * the client built from it and picks up a key rotation or a host change
 * without a restart.
 */

import { Code, ConnectError } from "@connectrpc/connect";
import type { LanguageModel } from "ai";

import { asBearer, getIntegrationClient, registerIntegrationClient } from "../integrations/clients.ts";
import { decideByChat } from "./chat-decisions.ts";
import { connectedRoutersOrNone } from "./connected.ts";
import {
  DecisionsApiError,
  type DecisionsClient,
  type DecisionsRequest,
  type DecisionsResponse,
} from "./decisions.ts";
import { routerModels } from "./models.ts";
import { listModelRouterDefinitions, type ModelRouterDefinition, type RouterRole } from "./registry.ts";

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

/** The first connected router that has a model for `role`, with that model. */
async function routerFor(
  role: RouterRole,
): Promise<{ router: ModelRouterDefinition; model: string } | null> {
  for (const router of await connectedRoutersOrNone()) {
    const model = (await routerModels(router))[role];
    if (model) return { router, model };
  }
  return null;
}

/** The model that titles a task; null when no connected router has one. */
export async function titleModel(): Promise<LanguageModel | null> {
  const chosen = await routerFor("title");
  if (!chosen) return null;
  return (await getIntegrationClient<RouterClient>(chosen.router.connector)).chat(chosen.model);
}

/** Whether a connected router has a decision model. */
export async function canDecide(): Promise<boolean> {
  return (await routerFor("decision")) !== null;
}

/** Control-plane codes a key lookup can recover from on a retry. */
const TRANSIENT_LOOKUP_CODES = new Set([
  Code.Unavailable,
  Code.DeadlineExceeded,
  Code.ResourceExhausted,
  Code.Aborted,
  Code.Internal,
]);

/** Answer a decisions request; null when no connected router can. A request
 * that names its `model` goes to the first connected router; any other goes
 * to the first one that has a decision model. A transient failure to resolve
 * the key is a retryable DecisionsApiError; a missing or rejected key stays
 * permanent. */
export async function decide(request: DecisionsRequest): Promise<DecisionsResponse | null> {
  let chosen: { router: ModelRouterDefinition; model: string } | null;
  if (request.model) {
    const [router] = await connectedRoutersOrNone();
    chosen = router ? { router, model: request.model } : null;
  } else {
    try {
      chosen = await routerFor("decision");
    } catch (error) {
      throw new DecisionsApiError(
        `read the decision model: ${error instanceof Error ? error.message : String(error)}`,
        null,
        true,
      );
    }
  }
  if (!chosen) return null;
  let client: RouterClient;
  try {
    client = await getIntegrationClient<RouterClient>(chosen.router.connector);
  } catch (error) {
    if (error instanceof ConnectError && TRANSIENT_LOOKUP_CODES.has(error.code)) {
      throw new DecisionsApiError(`resolve the ${chosen.router.label} key: ${error.rawMessage}`, null, true);
    }
    throw error;
  }
  const resolved = { ...request, model: chosen.model };
  return client.decisions
    ? client.decisions.decide(resolved)
    : decideByChat(client.chat(resolved.model), resolved);
}
