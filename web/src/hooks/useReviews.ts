import { useQuery } from "@connectrpc/connect-query";

import { getReview, listReviews } from "../gen/engram/app/v1/review-ReviewService_connectquery";

export function useReviews(repo?: string) {
  return useQuery(listReviews, repo ? { repo } : {}, { staleTime: 10_000 });
}

/** Full detail for one review — findings + verdicts — loaded lazily when a row
 *  is expanded. Kept fresh while a review is still running. */
export function useReview(id: string | undefined, enabled = true) {
  return useQuery(getReview, { id: id ?? "" }, { enabled: enabled && !!id, staleTime: 5_000 });
}
