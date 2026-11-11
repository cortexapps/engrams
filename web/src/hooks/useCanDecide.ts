import { useModelRouters } from "./useModelRouters";

/** Whether a model router can answer a decision that names no model: it has
 * a key and a decision model. The Decide block and smart routing are offered
 * only then. `undefined` while the routers load; false when they cannot be
 * read. */
export function useCanDecide(): boolean | undefined {
  const { data, isError } = useModelRouters();
  if (isError) return false;
  return data?.routers.some((r) => r.credentialConfigured && r.decisionModel !== "");
}

/** Whether a model router has a key: what a decision that names its own
 * model needs. `undefined` while the routers load. */
export function useHasRouterKey(): boolean | undefined {
  const { data, isError } = useModelRouters();
  if (isError) return false;
  return data?.routers.some((r) => r.credentialConfigured);
}
