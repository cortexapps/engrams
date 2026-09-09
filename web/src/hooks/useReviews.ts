import {
  createConnectQueryKey,
  useInfiniteQuery,
  useMutation,
  useQuery,
} from "@connectrpc/connect-query";
import { keepPreviousData, useQueryClient } from "@tanstack/react-query";

import {
  getReview,
  listReviews,
  retryReview,
} from "../gen/engram/app/v1/review-ReviewService_connectquery";
import type { Review, ReviewFacets } from "../gen/engram/app/v1/review_pb";

/** The list filters. Every one reads the NEWEST pass of each pull request. */
export interface ReviewListParams {
  repos?: string[];
  search?: string;
  authors?: string[];
  prStates?: string[];
  statuses?: string[];
  severities?: string[];
}

/** The values the filters can take, over every reviewed pull request. */
export type ReviewFacetValues = Pick<ReviewFacets, "repos" | "authors" | "prStates" | "statuses">;

const NO_FACETS: ReviewFacetValues = { repos: [], authors: [], prStates: [], statuses: [] };

/** How many pull requests a page holds. The list pages by PULL REQUEST and
 *  carries every pass of each, so the rows are not the unit. The coordinate
 *  fallback mirrors `groupByPr`. */
function pullRequestsOn(reviews: readonly Review[]): number {
  return new Set(reviews.map((r) => r.targetId || `${r.repo}#${r.prNumber}`)).size;
}

export interface ReviewsInfiniteResult {
  /** Every pass loaded so far, across every page, each once. */
  reviews: Review[];
  /** Pull requests matching the filters, before pagination. */
  totalCount: number | undefined;
  facets: ReviewFacetValues;
  hasNextPage: boolean;
  isFetchingNextPage: boolean;
  fetchNextPage: () => void;
  isPending: boolean;
  error: unknown;
}

/**
 * The reviews list, a page of pull requests at a time. The ledger and the rail
 * both read through here, so they share React Query's cache and page in step.
 */
export function useReviewsInfinite(
  params: ReviewListParams,
  pageSize: number,
): ReviewsInfiniteResult {
  const query = useInfiniteQuery(
    listReviews,
    { ...params, pageSize, page: 1 },
    {
      pageParamKey: "page",
      getNextPageParam: (lastPage, allPages) => {
        const soFar = allPages.reduce((n, page) => n + pullRequestsOn(page.reviews), 0);
        return soFar < lastPage.totalCount ? allPages.length + 1 : undefined;
      },
      // Poll modestly so the stage and the "watch live" link advance on their
      // own while a review runs, without hammering an idle dashboard. A refetch
      // re-requests EVERY loaded page, so the interval scales with the depth
      // read to keep request volume constant (the rule the tasks list follows).
      staleTime: 10_000,
      refetchInterval: (current) => 10_000 * Math.max(1, current.state.data?.pages.length ?? 1),
      // A filter change keeps the rows in hand until the new page lands, so
      // the ledger does not drop to a skeleton on every keystroke.
      placeholderData: keepPreviousData,
    },
  );

  const pages = query.data?.pages ?? [];
  // A pass can move across a page boundary between fetches (a new pass over
  // an old pull request lifts it to page one); keep the first occurrence.
  const seen = new Set<string>();
  const reviews = pages
    .flatMap((page) => page.reviews)
    .filter((review) => {
      if (seen.has(review.id)) return false;
      seen.add(review.id);
      return true;
    });
  const lastPage = pages[pages.length - 1];

  return {
    reviews,
    totalCount: lastPage?.totalCount,
    facets: pages[0]?.facets ?? NO_FACETS,
    hasNextPage: query.hasNextPage,
    isFetchingNextPage: query.isFetchingNextPage,
    fetchNextPage: query.fetchNextPage,
    isPending: query.isPending,
    error: query.error,
  };
}

/** Full detail for one review — findings + verdicts + the activity log + every
 *  pass over the same pull request. While the review is still running it polls
 *  quickly so the log fills in step by step; the response says whether it is. */
export function useReview(id: string | undefined, opts: { enabled?: boolean } = {}) {
  const enabled = (opts.enabled ?? true) && !!id;
  return useQuery(
    getReview,
    { id: id ?? "" },
    {
      enabled,
      staleTime: 5_000,
      refetchInterval: (current) => (current.state.data?.review?.active ? 2_500 : false),
    },
  );
}

/** Re-run a terminal review from scratch. Dispatches a fresh pass (a new review
 *  row + `review:<reviewId>` workflow) and refreshes the list so it appears. */
export function useRetryReview() {
  const qc = useQueryClient();
  return useMutation(retryReview, {
    onSuccess: () => {
      qc.invalidateQueries({
        // cardinality undefined → the whole ListReviews key family, every
        // page of every filter.
        queryKey: createConnectQueryKey({ schema: listReviews, cardinality: undefined }),
      });
    },
  });
}
