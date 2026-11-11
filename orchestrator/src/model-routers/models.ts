/** The model a router uses for each role: an admin's choice, else the
 * router's built-in model, else none. */

import { makeModelRouterStore, type ModelRouterStore } from "../db/model-routers.ts";
import type { ModelRouterDefinition, RouterRole } from "./registry.ts";

export type RouterModels = Record<RouterRole, string | null>;

export async function routerModels(
  router: ModelRouterDefinition,
  store: Pick<ModelRouterStore, "getSettings"> = makeModelRouterStore(),
): Promise<RouterModels> {
  const chosen = await store.getSettings(router.id);
  return {
    default: chosen?.defaultModel ?? router.models.default ?? null,
    title: chosen?.titleModel ?? router.models.title ?? null,
    decision: chosen?.decisionModel ?? router.models.decision ?? null,
  };
}
