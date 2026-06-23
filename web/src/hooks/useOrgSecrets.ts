/**
 * Org-secret hooks (ADR 0057).
 *
 * Org secrets live on the orchestrator's native Connect `OrgSecretService`
 * (admin-only; values sealed coordinator-side and never returned). `useOrgSecretNames`
 * feeds the profile editor's secret-ref picker — names only, never values.
 * `useOrgSecrets` + the put/delete mutations back the admin management panel
 * (`/settings/secrets`, the C0 phase before the integration catalog).
 */

import { useMutation, useQuery, createConnectQueryKey } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";
import {
  listSecrets,
  putSecret,
  deleteSecret,
} from "../gen/engram/app/v1/org_secret-OrgSecretService_connectquery";

/** Existing org-secret names (metadata only) for the profile secret-ref picker. */
export function useOrgSecretNames() {
  return useQuery(
    listSecrets,
    {},
    {
      select: (data): string[] => data.secrets.map((s) => s.name),
      staleTime: 10_000,
    },
  );
}

/** Full org-secret metadata (name + key id + timestamps) for the management panel.
 * Values are NEVER returned by the API — this is "set / not set" surface only. */
export function useOrgSecrets() {
  return useQuery(listSecrets, {}, { staleTime: 10_000 });
}

/** Invalidate every org-secret list consumer (panel + ref picker) after a write. */
function useInvalidateOrgSecrets() {
  const qc = useQueryClient();
  return () =>
    qc.invalidateQueries({
      queryKey: createConnectQueryKey({ schema: listSecrets, input: {}, cardinality: "finite" }),
    });
}

/** Upsert (seal + store) an org secret. Plaintext sealed coordinator-side; re-put overwrites. */
export function usePutOrgSecret() {
  const invalidate = useInvalidateOrgSecrets();
  return useMutation(putSecret, { onSuccess: invalidate });
}

/** Delete an org secret by name (idempotent). */
export function useDeleteOrgSecret() {
  const invalidate = useInvalidateOrgSecrets();
  return useMutation(deleteSecret, { onSuccess: invalidate });
}
