import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { TranscriptWindow } from "../components/session-thread/transcript-window";

// Scroll behaviour for the windowed transcript: keep the reader's position
// across a PREPEND, and read the next page before the reader reaches the top.
//
// Scope note: this hook only reads `scrollTop`/`scrollHeight` and corrects for
// content added ABOVE the reader. The viewport's auto-scroll (assistant-ui's
// stick-to-bottom) is deliberately untouched.

/** Distance from the top (px) that starts the next page. About one viewport of
 *  lead time, so the page usually lands before the reader gets to the edge. */
const TRIGGER_PX = 400;

export interface TranscriptBackfill {
  /** Attach to the scrolling viewport element. */
  viewportRef: (node: HTMLDivElement | null) => void;
}

export function useTranscriptBackfill({
  hasMore,
  loadingOlder,
  loadOlder,
  oldestIdx,
}: TranscriptWindow): TranscriptBackfill {
  // The node lives in state, not a ref, so the listener effect re-runs when the
  // viewport mounts (a ref object mutates without telling the effect).
  const [node, setNode] = useState<HTMLDivElement | null>(null);
  const viewportRef = useCallback((el: HTMLDivElement | null) => setNode(el), []);

  const lastHeight = useRef(0);
  const lastOldest = useRef<number | null>(null);

  // Older messages are added ABOVE the current offset, so everything the reader
  // is looking at moves DOWN by exactly the height that was added: scroll down
  // by the same amount in the SAME layout pass and the viewport does not jump.
  // No dependency array — every render must record the height, because the
  // height recorded by the PREVIOUS render is the "before" measurement.
  useLayoutEffect(() => {
    if (!node) return;
    const height = node.scrollHeight;
    const prepended =
      lastOldest.current != null && oldestIdx != null && oldestIdx < lastOldest.current;
    if (prepended && lastHeight.current > 0) {
      // The viewport carries `scroll-smooth`, which would ANIMATE this
      // correction — that is the jump we are removing. Suppress the animation
      // for this one assignment only, and restore whatever was set before.
      const behavior = node.style.scrollBehavior;
      node.style.scrollBehavior = "auto";
      node.scrollTop += height - lastHeight.current;
      node.style.scrollBehavior = behavior;
    }
    lastOldest.current = oldestIdx;
    lastHeight.current = height;
  });

  const maybeLoadOlder = useCallback(() => {
    if (!node || !hasMore || loadingOlder) return;
    if (node.scrollTop <= TRIGGER_PX) loadOlder();
  }, [node, hasMore, loadingOlder, loadOlder]);

  useEffect(() => {
    if (!node) return;
    node.addEventListener("scroll", maybeLoadOlder, { passive: true });
    return () => node.removeEventListener("scroll", maybeLoadOlder);
  }, [node, maybeLoadOlder]);

  // A page that lands without filling the viewport leaves the reader still at
  // the top, and no scroll event follows. Re-check whenever a page settles.
  useEffect(() => {
    maybeLoadOlder();
  }, [maybeLoadOlder]);

  return { viewportRef };
}
