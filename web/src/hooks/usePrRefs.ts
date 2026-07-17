import { useQuery } from "@connectrpc/connect-query";

import { listPrRefs } from "../gen/engram/app/v1/pr_ref-PrRefService_connectquery";

export function usePrRefs(taskId: string | null, sessionId: string) {
  return useQuery(listPrRefs, taskId ? { taskId } : { sessionId }, { staleTime: 10_000 });
}
