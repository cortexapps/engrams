import { createConnectQueryKey, useMutation, useQuery } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";

import {
  getReview,
  listReviews,
  retryReview,
} from "../gen/engram/app/v1/review-ReviewService_connectquery";

export function useReviews(repo?: string) {
  // Poll modestly so the stage and the "watch live" link advance on their own
  // while a review runs, without hammering an idle dashboard.
  return useQuery(listReviews, repo ? { repo } : {}, {
    staleTime: 10_000,
    refetchInterval: 10_000,
  });
}

/** Full detail for one review — findings + verdicts + the activity log — loaded
 *  lazily when a row is expanded. While the review is still running (`active`)
 *  it polls quickly so the log fills in step by step. */
export function useReview(
  id: string | undefined,
  opts: { enabled?: boolean; active?: boolean } = {},
) {
  const enabled = (opts.enabled ?? true) && !!id;
  return useQuery(
    getReview,
    { id: id ?? "" },
    {
      enabled,
      staleTime: opts.active ? 0 : 5_000,
      ...(opts.active ? { refetchInterval: 2_500 } : {}),
    },
  );
}

/** Re-run a terminal review from scratch. Dispatches a fresh pass (a new review
 *  record + workflow epoch) and refreshes the list so the new row appears. */
export function useRetryReview() {
  const qc = useQueryClient();
  return useMutation(retryReview, {
    onSuccess: () => {
      qc.invalidateQueries({
        queryKey: createConnectQueryKey({
          schema: listReviews,
          cardinality: "finite",
        }),
      });
    },
  });
}
