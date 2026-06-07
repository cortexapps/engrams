import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { addRegistry, deleteRegistry, fetchRegistries } from "../api";
import type { AddRegistryRequest } from "../types";

const KEY = ["registries"] as const;

/** List of registered registry credentials. The coordinator never
 * returns secret material here — only `(host, auth_kind, principal)`
 * tuples — so this hook is safe to render verbatim. */
export function useRegistries() {
  return useQuery({
    queryKey: KEY,
    queryFn: fetchRegistries,
    refetchOnWindowFocus: true,
    staleTime: 10_000,
  });
}

/** Mutation: add a new registry. Invalidates the list on success so
 * the panel re-renders without the consumer having to thread state.
 * Errors are surfaced via the mutation's `error` field — the panel
 * renders them inline. */
export function useAddRegistry() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: AddRegistryRequest) => addRegistry(req),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: KEY });
    },
  });
}

export function useDeleteRegistry() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (host: string) => deleteRegistry(host),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: KEY });
    },
  });
}
