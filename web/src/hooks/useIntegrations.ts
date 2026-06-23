/**
 * Integration hooks (ADR 0057 C3/C4).
 *
 * `useConnectors` + the connector mutations back the Plane-B catalog editor;
 * `useMintKinds` feeds the data-driven Plane-A mint form. Connectors live in the
 * orchestrator DB (built-ins are read-only seeds); mint kinds are the
 * coordinator's static registry, proxied. Both admin-only.
 */

import { useMutation, useQuery, createConnectQueryKey } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";
import {
  listConnectors,
  upsertConnector,
  deleteConnector,
} from "../gen/engram/app/v1/integration-IntegrationService_connectquery";
import { listMintKinds } from "../gen/engram/app/v1/mint-MintService_connectquery";

/** Built-in seeds (read-only) + admin-authored connectors. */
export function useConnectors() {
  return useQuery(listConnectors, {}, { staleTime: 10_000 });
}

/** The coordinator's mint-kind registry (Plane-A form metadata). */
export function useMintKinds() {
  return useQuery(listMintKinds, {}, { staleTime: 60_000 });
}

function useInvalidateConnectors() {
  const qc = useQueryClient();
  return () =>
    qc.invalidateQueries({
      queryKey: createConnectQueryKey({ schema: listConnectors, input: {}, cardinality: "finite" }),
    });
}

/** Create or replace a custom connector (validated coordinator-side at load + server-side here). */
export function useUpsertConnector() {
  const invalidate = useInvalidateConnectors();
  return useMutation(upsertConnector, { onSuccess: invalidate });
}

/** Delete a custom connector (idempotent; built-ins are rejected server-side). */
export function useDeleteConnector() {
  const invalidate = useInvalidateConnectors();
  return useMutation(deleteConnector, { onSuccess: invalidate });
}
