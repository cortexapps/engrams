import { useModelRouters } from "./useModelRouters";

/** Whether a model router can answer a decision: it has a key and a decision
 * model. The Decide block and smart routing are offered only then.
 * `undefined` while the routers load. */
export function useCanDecide(): boolean | undefined {
  const { data } = useModelRouters();
  return data?.routers.some((r) => r.credentialConfigured && r.decisionModel !== "");
}
