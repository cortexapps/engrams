/** Model calls the orchestrator makes itself, through a connected router.
 *
 * The coordinator unseals the router's key; `integrations/clients.ts` caches
 * the client built from it and picks up a key rotation or a host change
 * without a restart.
 */

import { Code, ConnectError } from "@connectrpc/connect";
import type { LanguageModel } from "ai";

import { asBearer, getIntegrationClient, registerIntegrationClient } from "../integrations/clients.ts";
import type { RunOpDeps } from "../integrations/run-op.ts";
import { decideByChat } from "./chat-decisions.ts";
import { connectedRoutersOrNone } from "./connected.ts";
import {
  DecisionsApiError,
  type DecisionsClient,
  type DecisionsRequest,
  type DecisionsResponse,
} from "./decisions.ts";
import { makeModelRouterStore } from "../db/model-routers.ts";
import { routerModels } from "./models.ts";
import type { ModelRouterDefinition, RouterRole } from "./registry.ts";

export interface RouterClient {
  chat(modelId: string): LanguageModel;
  decisions?: DecisionsClient;
}

const registered = new WeakSet<ModelRouterDefinition>();

/** The router's client. `deps` is a test seam for the key lookup. */
export function routerClient(
  router: ModelRouterDefinition,
  deps?: RunOpDeps,
): Promise<RouterClient> {
  if (!registered.has(router)) {
    registerIntegrationClient(router.connector, (cred, host): RouterClient => {
      const apiKey = asBearer(cred);
      const decisions = router.decisions?.(apiKey, host);
      return { chat: router.chat(apiKey, host), ...(decisions ? { decisions } : {}) };
    });
    registered.add(router);
  }
  return getIntegrationClient<RouterClient>(router.connector, deps);
}

interface RoutedModel {
  router: ModelRouterDefinition;
  model: string;
}

/** The connected router for `role`, with its model: the first one whose
 * model an admin chose, else the first one that has a built-in model. */
export async function routerFor(
  role: RouterRole,
  deps: { routers: typeof connectedRoutersOrNone; models: typeof routerModels } = {
    routers: connectedRoutersOrNone,
    models: routerModels,
  },
): Promise<RoutedModel | null> {
  let builtin: RoutedModel | null = null;
  for (const router of await deps.routers()) {
    const roleModel = (await deps.models(router))[role];
    if (roleModel?.chosen) return { router, model: roleModel.model };
    if (roleModel) builtin ??= { router, model: roleModel.model };
  }
  return builtin;
}

/** The connected router that serves `model` for a decision: one whose
 * decision model it is, else one whose catalog lists it, else the first. */
async function routerForDecisionModel(model: string): Promise<RoutedModel | null> {
  const routers = await connectedRoutersOrNone();
  for (const router of routers) {
    if ((await routerModels(router)).decision?.model === model) return { router, model };
  }
  const store = makeModelRouterStore();
  for (const router of routers) {
    if ((await store.getModel(router.id, model))?.available) return { router, model };
  }
  const [first] = routers;
  return first ? { router: first, model } : null;
}

/** The model that titles a task; null when no connected router has one. */
export async function titleModel(): Promise<LanguageModel | null> {
  const chosen = await routerFor("title");
  if (!chosen) return null;
  return (await routerClient(chosen.router)).chat(chosen.model);
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
 * that names its `model` goes to the router that serves the model; any other
 * goes to the router for the decision role. A transient failure to resolve
 * the key is a retryable DecisionsApiError; a missing or rejected key stays
 * permanent. */
export async function decide(request: DecisionsRequest): Promise<DecisionsResponse | null> {
  let chosen: RoutedModel | null;
  try {
    chosen = request.model
      ? await routerForDecisionModel(request.model)
      : await routerFor("decision");
  } catch (error) {
    throw new DecisionsApiError(
      `read the decision model: ${error instanceof Error ? error.message : String(error)}`,
      null,
      true,
    );
  }
  if (!chosen) return null;
  let client: RouterClient;
  try {
    client = await routerClient(chosen.router);
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
