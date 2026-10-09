import { useModelRouters } from "./useModelRouters";

/** The ids of the model routers that have a key. Router-backed features
 * (the Decide block, Slack smart routing) are offered only for these. */
export function useConnectedRouters(): ReadonlySet<string> {
  const { data } = useModelRouters();
  return new Set((data?.routers ?? []).filter((r) => r.credentialConfigured).map((r) => r.id));
}
