import { useEffect, useRef, useState } from "react";

import { mergeIndexed } from "@/lib/sessionWindow";
import type { IndexedEvent } from "@/lib/types";
import { subscribeSpec } from "@/sse";

export interface SpecEventsState {
  events: IndexedEvent[];
  streamingText: string;
  error: Event | null;
  missed: number;
}

/** Read and follow the member-gated event feed for one shared spec thread. */
export function useSpecEvents(specId: string | undefined): SpecEventsState {
  const [events, setEvents] = useState<IndexedEvent[]>([]);
  const [streamingText, setStreamingText] = useState("");
  const [error, setError] = useState<Event | null>(null);
  const [missed, setMissed] = useState(0);
  const overlay = useRef<Map<string, string>>(new Map());
  const finalized = useRef<Set<string>>(new Set());

  useEffect(() => {
    setEvents([]);
    setStreamingText("");
    setError(null);
    setMissed(0);
    overlay.current = new Map();
    finalized.current = new Set();
    if (!specId) return;
    const tail = () => [...overlay.current.values()].join("");
    return subscribeSpec(specId, {
      onEvent: (event) => {
        const value = event.event;
        if (value.type === "agent_message" && value.role === "assistant") {
          finalized.current.add(value.message_id);
          if (overlay.current.delete(value.message_id)) setStreamingText(tail());
        } else if (value.type === "run_completed" || value.type === "run_interrupted") {
          if (overlay.current.size > 0) {
            overlay.current.clear();
            setStreamingText("");
          }
        }
        setEvents((current) => mergeIndexed(current, [event]));
      },
      onDelta: ({ messageId, chunk }) => {
        if (finalized.current.has(messageId)) return;
        overlay.current.set(messageId, (overlay.current.get(messageId) ?? "") + chunk);
        setStreamingText(tail());
      },
      onError: setError,
      onLagged: (count) => setMissed((current) => current + count),
    });
  }, [specId]);

  return { events, streamingText, error, missed };
}
