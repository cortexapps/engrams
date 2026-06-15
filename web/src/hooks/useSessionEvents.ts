import { useEffect, useRef, useState } from "react";
import { subscribeSession } from "../sse";
import type { IndexedEvent } from "../lib/types";

export interface UseSessionEventsOptions {
  /** Cap on retained events (most-recent first). Defaults to unlimited. */
  cap?: number;
}

/**
 * Subscribes to a session's SSE feed. Events arrive in monotonic `idx`
 * order; we de-dup on idx in case a reconnect replays a frame the bus
 * also delivered.
 */
export function useSessionEvents(
  sessionId: string | undefined,
  opts: UseSessionEventsOptions = {},
): IndexedEvent[] {
  const [events, setEvents] = useState<IndexedEvent[]>([]);
  // The highest idx observed — used by the dedup pass below.
  const highWater = useRef<number>(-1);

  useEffect(() => {
    if (!sessionId) return;

    setEvents([]);
    highWater.current = -1;

    const unsubscribe = subscribeSession(sessionId, {
      onEvent: (e) => {
        if (e.idx <= highWater.current) return;
        highWater.current = e.idx;
        setEvents((prev) => {
          const next = [...prev, e];
          if (opts.cap && next.length > opts.cap) {
            return next.slice(next.length - opts.cap);
          }
          return next;
        });
      },
    });

    return unsubscribe;
  }, [sessionId, opts.cap]);

  return events;
}
