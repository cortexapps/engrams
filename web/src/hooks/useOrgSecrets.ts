/**
 * Org-secret hooks (ADR 0057).
 *
 * Org secrets live on the orchestrator's native Connect `OrgSecretService`
 * (admin-only; values sealed coordinator-side and never returned). `useOrgSecretNames`
 * feeds the profile editor's secret-ref picker — names only, never values.
 */

import { useQuery } from "@connectrpc/connect-query";
import { listSecrets } from "../gen/engram/app/v1/org_secret-OrgSecretService_connectquery";

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
