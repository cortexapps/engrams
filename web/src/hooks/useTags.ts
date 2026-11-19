import { createConnectQueryKey, useMutation, useQuery } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";

import {
  getAutomation,
  listAutomations,
} from "@/gen/engram/app/v1/automation-AutomationService_connectquery";
import {
  createTag,
  deleteTag,
  listTags,
  updateTag,
} from "@/gen/engram/app/v1/tag-TagService_connectquery";

/** Every tag with its automation count (admin-only). */
export function useTags() {
  return useQuery(listTags, {}, { staleTime: 10_000 });
}

/** Refreshes every view that shows tags: the tag list and the automations,
 * which carry their tags' names and colors. */
export function useInvalidateTagViews() {
  const queryClient = useQueryClient();
  return async () => {
    await Promise.all([
      queryClient.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: listTags, cardinality: "finite" }),
      }),
      queryClient.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: listAutomations, cardinality: "finite" }),
      }),
      queryClient.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: getAutomation, cardinality: "finite" }),
      }),
    ]);
  };
}

export function useCreateTag() {
  const invalidate = useInvalidateTagViews();
  return useMutation(createTag, { onSuccess: invalidate });
}

export function useUpdateTag() {
  const invalidate = useInvalidateTagViews();
  return useMutation(updateTag, { onSuccess: invalidate });
}

export function useDeleteTag() {
  const invalidate = useInvalidateTagViews();
  return useMutation(deleteTag, { onSuccess: invalidate });
}
