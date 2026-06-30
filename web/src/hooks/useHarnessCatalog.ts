import { useQuery } from "@connectrpc/connect-query";
import { listHarnesses } from "../gen/engram/app/v1/harness-HarnessCatalogService_connectquery";
import type { HarnessSummary } from "../gen/engram/app/v1/harness_pb";

/**
 * The registered harness catalog (ADR 0062/0063). Each entry's `descriptor`
 * carries the model + effort enums (and the auth env-var names) that drive the
 * profile-editor + session-create harness/model/effort pickers. Selection is per
 * session; a profile sets a default, overridable at create (B2).
 */
export function useHarnessCatalog(enabled = true) {
  return useQuery(
    listHarnesses,
    {},
    {
      enabled,
      select: (resp): HarnessSummary[] => resp.harnesses,
    },
  );
}
