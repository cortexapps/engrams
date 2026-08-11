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
  // A page has landed and the height it adds has not been paid for yet.
  //
  // The prepend and the growth do NOT share a commit. `oldestIdx` moves when
  // the events land, but the taller DOM arrives when assistant-ui re-renders
  // its message list from its own store — a later commit that does not
  // re-render this component at all. So a correction measured where `oldestIdx`
  // moves measures ZERO, and the reader was thrown a whole turn (~14 000 px on
  // the 10 k-event rig) down the transcript. The prepend ARMS the correction;
  // the layout that follows pays it, seen through a ResizeObserver because
  // React cannot report a commit it never told us about.
  const owedGrowth = useRef(false);
  const disarm = useRef<ReturnType<typeof setTimeout> | null>(null);

  useLayoutEffect(() => {
    if (!node) return;
    if (lastOldest.current != null && oldestIdx != null && oldestIdx < lastOldest.current) {
      owedGrowth.current = true;
      // The page renders within a frame or two. Bounding the armed window keeps
      // an unrelated later growth — a live run streaming into the tail — from
      // being mistaken for the backfill's.
      if (disarm.current) clearTimeout(disarm.current);
      disarm.current = setTimeout(() => {
        owedGrowth.current = false;
      }, 1000);
    }
    lastOldest.current = oldestIdx;
  }, [node, oldestIdx]);

  // Older messages are added ABOVE the current offset, so everything the reader
  // is looking at moves DOWN by exactly the height that was added: scroll down
  // by the same amount, before the browser paints, and the viewport does not
  // move. A ResizeObserver runs at exactly that point in the frame.
  useEffect(() => {
    if (!node) return;
    const content = node.firstElementChild;
    if (!content) return;
    lastHeight.current = node.scrollHeight;
    const observer = new ResizeObserver(() => {
      const height = node.scrollHeight;
      const grew = height - lastHeight.current;
      lastHeight.current = height;
      if (!owedGrowth.current || grew <= 0) return;
      // The viewport carries `scroll-smooth`, which would ANIMATE this
      // correction — that is the jump we are removing. Suppress the animation
      // for this one assignment only, and restore whatever was set before.
      const behavior = node.style.scrollBehavior;
      node.style.scrollBehavior = "auto";
      node.scrollTop += grew;
      node.style.scrollBehavior = behavior;
    });
    observer.observe(content);
    return () => observer.disconnect();
  }, [node]);

  useEffect(
    () => () => {
      if (disarm.current) clearTimeout(disarm.current);
    },
    [],
  );

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
  // the top, and no scroll event follows. Re-check whenever a page settles —
  // but ONLY in that case. When the content does overflow, the scroll listener
  // covers it: at mount `scrollTop` is still 0 because the stick-to-bottom
  // scroll has not run yet, so an unconditional re-check reads "the reader is
  // at the top", and the transcript opens with a second page nobody asked for
  // (measured: 28 276 px on open, of which ~14 000 was that extra page).
  useEffect(() => {
    if (!node || node.scrollHeight > node.clientHeight) return;
    maybeLoadOlder();
  }, [node, maybeLoadOlder]);

  return { viewportRef };
}
