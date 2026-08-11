import { createContext, useContext } from "react";

// The windowed transcript's backfill controls, carried into the Thread subtree.
// The transcript opens on a tail window (lib/sessionWindow.ts) and reads older
// pages as the reader scrolls up, so the viewport — which owns the scroll
// position — needs three things: whether older transcript exists, whether a
// page is already in flight, and the oldest idx held (which changes EXACTLY
// when a page is prepended, so it is the scroll-anchor key).

export interface TranscriptWindow {
  hasMore: boolean;
  loadingOlder: boolean;
  loadOlder: () => void;
  oldestIdx: number | null;
}

/** A transcript that holds its whole log: nothing to backfill. Surfaces that
 *  do not window (tests, panes that mount a short session) get this. */
export const NO_TRANSCRIPT_BACKFILL: TranscriptWindow = {
  hasMore: false,
  loadingOlder: false,
  loadOlder: () => {},
  oldestIdx: null,
};

export const TranscriptWindowContext = createContext<TranscriptWindow>(NO_TRANSCRIPT_BACKFILL);

export const useTranscriptWindow = (): TranscriptWindow => useContext(TranscriptWindowContext);
