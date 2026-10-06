import { createConnectQueryKey, useMutation, useQuery } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";

import {
  getRetentionPolicy,
  setRetentionPolicy,
} from "../gen/engram/app/v1/retention-RetentionService_connectquery";

/** The org's retention policy (Settings → Retention; admin-only). */
export function useRetentionPolicy() {
  return useQuery(getRetentionPolicy, {}, { staleTime: 10_000 });
}

export function useSetRetentionPolicy() {
  const queryClient = useQueryClient();
  return useMutation(setRetentionPolicy, {
    onSuccess: () => {
      void queryClient.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: getRetentionPolicy, cardinality: "finite" }),
      });
    },
  });
}
