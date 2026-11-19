/** The model a router uses for each role: an admin's choice, else the
 * router's built-in model, else none. */

import { makeModelRouterStore, type ModelRouterStore } from "../db/model-routers.ts";
import type { ModelRouterDefinition, RouterRole } from "./registry.ts";

export interface RoleModel {
  model: string;
  /** An admin chose the model. False for the router's built-in model. */
  chosen: boolean;
}

export type RouterModels = Record<RouterRole, RoleModel | null>;

function roleModel(chosen: string | null | undefined, builtin: string | undefined): RoleModel | null {
  if (chosen) return { model: chosen, chosen: true };
  return builtin ? { model: builtin, chosen: false } : null;
}

export async function routerModels(
  router: ModelRouterDefinition,
  store: Pick<ModelRouterStore, "getSettings"> = makeModelRouterStore(),
): Promise<RouterModels> {
  const chosen = await store.getSettings(router.id);
  return {
    default: roleModel(chosen?.defaultModel, router.models.default),
    title: roleModel(chosen?.titleModel, router.models.title),
    decision: roleModel(chosen?.decisionModel, router.models.decision),
  };
}
