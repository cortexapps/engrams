/** Inputs-tab hooks (ADR 0119 phase 3.6). Kept apart from the editor and
 * list hooks so the three surfaces evolve independently. */

import { createConnectQueryKey, useMutation, useQuery } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";

import {
  describeInputKeys,
  getAutomation,
  listAutomations,
  listInputKeyOptions,
  setInputs,
} from "@/gen/engram/app/v1/automation-AutomationService_connectquery";
import type { KeyNoun } from "@/lib/automation-inputs";

/** Noun-keyed options for a map input's key picker. Repository options are
 * ledger-observed (ADR 0119 3.1 divergence), so the editor also accepts a
 * free-typed key — this list is a convenience, not the universe. */
export function useInputKeyOptions(noun: KeyNoun | undefined, connectionId?: string) {
  return useQuery(
    listInputKeyOptions,
    { noun: noun ?? "", ...(connectionId ? { connectionId } : {}) },
    { enabled: !!noun, staleTime: 60_000 },
  );
}

/** Labels for keys a page already shows (the channels of its threads): a
 * lookup by id, never the provider's whole list. The key set is sorted and
 * deduplicated so the same page asks once. */
export function useInputKeyLabels(noun: KeyNoun, keys: readonly string[], connectionId?: string) {
  const sorted = [...new Set(keys)].sort();
  return useQuery(
    describeInputKeys,
    { noun, keys: sorted, ...(connectionId ? { connectionId } : {}) },
    { enabled: sorted.length > 0, staleTime: 10 * 60_000 },
  );
}

export function useSetInputs() {
  const queryClient = useQueryClient();
  return useMutation(setInputs, {
    onSuccess: () => {
      void queryClient.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: getAutomation, cardinality: "finite" }),
      });
      void queryClient.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: listAutomations, cardinality: "finite" }),
      });
    },
  });
}
