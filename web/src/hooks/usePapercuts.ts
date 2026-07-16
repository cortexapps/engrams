import { createConnectQueryKey, useMutation, useQuery } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";
import {
  archivePapercut,
  listPapercuts,
  unarchivePapercut,
} from "../gen/engram/app/v1/papercut-PapercutService_connectquery";

export function usePapercuts(includeArchived = false) {
  return useQuery(listPapercuts, { includeArchived }, { staleTime: 10_000 });
}

function useInvalidatePapercuts() {
  const queryClient = useQueryClient();
  return () =>
    queryClient.invalidateQueries({
      queryKey: createConnectQueryKey({ schema: listPapercuts, cardinality: "finite" }),
    });
}

export function useArchivePapercut() {
  const invalidate = useInvalidatePapercuts();
  return useMutation(archivePapercut, { onSuccess: invalidate });
}

export function useUnarchivePapercut() {
  const invalidate = useInvalidatePapercuts();
  return useMutation(unarchivePapercut, { onSuccess: invalidate });
}
