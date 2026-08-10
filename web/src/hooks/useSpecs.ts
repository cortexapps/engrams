import { useQuery } from "@connectrpc/connect-query";

import { listSpecs } from "../gen/engram/app/v1/spec-SpecService_connectquery";

export type SpecLifecycleFilter = "all" | "draft" | "published";

export function useSpecs(lifecycle: SpecLifecycleFilter) {
  return useQuery(listSpecs, { lifecycle, page: 1, pageSize: 0 }, { staleTime: 10_000 });
}
