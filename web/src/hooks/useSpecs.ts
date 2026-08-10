import { useQuery } from "@connectrpc/connect-query";

import { listSpecs } from "../gen/engram/app/v1/spec-SpecService_connectquery";

export type SpecLifecycleFilter = "all" | "draft" | "published";

export const SPEC_LIST_QUERY_OPTIONS = {
  staleTime: 10_000,
  refetchInterval: 15_000,
  refetchIntervalInBackground: false,
} as const;

export function useSpecs(lifecycle: SpecLifecycleFilter, page = 1, pageSize = 50) {
  return useQuery(listSpecs, { lifecycle, page, pageSize }, SPEC_LIST_QUERY_OPTIONS);
}
