import { useMutation, useQuery, createConnectQueryKey } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";
import {
  listRegistries,
  addRegistry,
  deleteRegistry,
} from "../gen/engram/app/v1/image-ImageService_connectquery";
import type { RegistryCredentialSummary, RegistryAuthKind } from "../lib/types";
import type { RegistryCredentialSummary as ProtoRegistryCredentialSummary } from "../gen/engram/app/v1/image_pb";

function protoRegistryToLegacy(r: ProtoRegistryCredentialSummary): RegistryCredentialSummary {
  return {
    id: r.id,
    registry_host: r.registryHost,
    auth_kind: r.authKind as RegistryAuthKind,
    auth_principal: r.authPrincipal ?? null,
    created_at: r.createdAt,
    updated_at: r.updatedAt ?? null,
  };
}

/** List of registered registry credentials. The coordinator never
 * returns secret material here — only `(host, auth_kind, principal)`
 * tuples — so this hook is safe to render verbatim. */
export function useRegistries() {
  return useQuery(
    listRegistries,
    {},
    {
      select: (data) => data.registries.map(protoRegistryToLegacy),
      refetchOnWindowFocus: true,
      staleTime: 10_000,
    },
  );
}

/** Mutation: add a new registry. Invalidates the list on success so
 * the panel re-renders without the consumer having to thread state.
 * Errors are surfaced via the mutation's `error` field — the panel
 * renders them inline. */
export function useAddRegistry() {
  const qc = useQueryClient();
  return useMutation(addRegistry, {
    onSuccess: () => {
      qc.invalidateQueries({
        queryKey: createConnectQueryKey({
          schema: listRegistries,
          input: {},
          cardinality: "finite",
        }),
      });
    },
  });
}

export function useDeleteRegistry() {
  const qc = useQueryClient();
  return useMutation(deleteRegistry, {
    onSuccess: () => {
      qc.invalidateQueries({
        queryKey: createConnectQueryKey({
          schema: listRegistries,
          input: {},
          cardinality: "finite",
        }),
      });
    },
  });
}
