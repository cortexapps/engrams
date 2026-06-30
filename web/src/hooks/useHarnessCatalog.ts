import { useMutation, useQuery, createConnectQueryKey } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";
import {
  listHarnesses,
  registerHarness,
  deleteHarness,
} from "../gen/engram/app/v1/harness-HarnessCatalogService_connectquery";
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

/** Invalidate the harness list so register/delete reflect immediately. */
function useInvalidateHarnesses() {
  const qc = useQueryClient();
  return () =>
    qc.invalidateQueries({
      queryKey: createConnectQueryKey({ schema: listHarnesses, input: {}, cardinality: "finite" }),
    });
}

/** Register a custom harness by OCI ref (admin; the coordinator pulls + packs it). */
export function useRegisterHarness() {
  const invalidate = useInvalidateHarnesses();
  return useMutation(registerHarness, { onSuccess: invalidate });
}

/** Soft-delete a custom harness by name (admin; built-ins are rejected coord-side). */
export function useDeleteHarness() {
  const invalidate = useInvalidateHarnesses();
  return useMutation(deleteHarness, { onSuccess: invalidate });
}
