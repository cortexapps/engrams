import { useCallback, useEffect, useRef } from "react";

/** Observe an infinite-scroll sentinel against the viewport. The observer is
 * recreated when its inputs change. A still-visible sentinel naturally fires
 * again when `isFetching` flips back to false after the new page renders. */
export function useLoadMoreSentinel({
  hasMore,
  isFetching,
  onLoadMore,
}: {
  hasMore: boolean;
  isFetching: boolean;
  onLoadMore: () => void;
}): (node: HTMLElement | null) => void {
  const observerRef = useRef<IntersectionObserver | null>(null);

  const sentinelRef = useCallback(
    (node: HTMLElement | null) => {
      observerRef.current?.disconnect();
      observerRef.current = null;

      if (!node || !hasMore || isFetching) return;

      const observer = new IntersectionObserver(
        (entries) => {
          if (isFetching || !entries.some((entry) => entry.isIntersecting)) return;
          // Close the tiny gap before the query's isFetching update renders so
          // repeated observer deliveries cannot start the same page twice.
          observer.disconnect();
          observerRef.current = null;
          onLoadMore();
        },
        { root: null },
      );
      observer.observe(node);
      observerRef.current = observer;
    },
    [hasMore, isFetching, onLoadMore],
  );

  useEffect(() => () => observerRef.current?.disconnect(), []);

  return sentinelRef;
}
