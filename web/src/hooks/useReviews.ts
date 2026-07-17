import { useQuery } from "@connectrpc/connect-query";

import { listReviews } from "../gen/engram/app/v1/review-ReviewService_connectquery";

export function useReviews(repo?: string) {
  return useQuery(listReviews, repo ? { repo } : {}, { staleTime: 10_000 });
}
