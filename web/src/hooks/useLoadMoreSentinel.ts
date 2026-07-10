import { useCallback, useEffect, useRef } from "react";

/** Observe an infinite-scroll sentinel against the viewport. The observer is
 * recreated when `hasMore` or `onLoadMore` changes, so callers must give
 * `onLoadMore` a new identity when their rendered row count changes. That
 * re-observation makes a sentinel that remains visible after growth fire again. */
export function useLoadMoreSentinel({
  hasMore,
  onLoadMore,
}: {
  hasMore: boolean;
  onLoadMore: () => void;
}): (node: HTMLElement | null) => void {
  const observerRef = useRef<IntersectionObserver | null>(null);

  const sentinelRef = useCallback(
    (node: HTMLElement | null) => {
      observerRef.current?.disconnect();
      observerRef.current = null;

      if (!node || !hasMore) return;

      const observer = new IntersectionObserver(
        (entries) => {
          if (entries.some((entry) => entry.isIntersecting)) onLoadMore();
        },
        { root: null },
      );
      observer.observe(node);
      observerRef.current = observer;
    },
    [hasMore, onLoadMore],
  );

  useEffect(() => () => observerRef.current?.disconnect(), []);

  return sentinelRef;
}
