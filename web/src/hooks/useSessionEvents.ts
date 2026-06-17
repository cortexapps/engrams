import { useEffect, useRef, useState } from "react";
import { subscribeSession } from "../sse";
import type { IndexedEvent } from "../lib/types";

export interface UseSessionEventsOptions {
  /** Cap on retained events (most-recent first). Defaults to unlimited. */
  cap?: number;
}

export interface SessionEventsState {
  /** The durable, persisted, idx-ordered event log (token chunks excluded). */
  events: IndexedEvent[];
  /**
   * Phase 1c: the live token tail — the concatenation of all not-yet-durable
   * `agent_message_chunk` deltas (the in-flight assistant message being
   * typed). EPHEMERAL by design: it is emptied the instant the terminal
   * `agent_message` lands (which supersedes it), cleared on a run terminal,
   * and reset on reconnect / session change. It is kept STRICTLY SEPARATE
   * from `events` so a chunk can never corrupt or double the durable
   * transcript — see the merge-safety notes below.
   */
  streamingText: string;
}

/**
 * Subscribes to a session's SSE feed. Durable events arrive in monotonic
 * `idx` order; we de-dup on idx in case a reconnect replays a frame the bus
 * also delivered.
 *
 * Phase 1c (ADR 0052) — live token streaming, two tracks kept separate so
 * the durable record ALWAYS wins:
 *   - Durable: persisted `agent_message`/`run_*` events → `events`.
 *   - Ephemeral: `agent_message_chunk` deltas → a live overlay (`overlay`,
 *     keyed by `message_id`), surfaced as `streamingText`. Never persisted,
 *     never replayed.
 *
 * The invariant: a delta is dropped the moment its message is `finalized`
 * (its durable `agent_message` arrived); a run terminal clears the overlay;
 * a reconnect/session-switch resets everything. So under a pod cycle,
 * reconnect, or out-of-order delivery the worst case is lost animation — the
 * durable terminal message always makes the transcript whole.
 */
export function useSessionEvents(
  sessionId: string | undefined,
  opts: UseSessionEventsOptions = {},
): SessionEventsState {
  const [events, setEvents] = useState<IndexedEvent[]>([]);
  const [streamingText, setStreamingText] = useState("");
  // The highest idx observed — used by the dedup pass below.
  const highWater = useRef<number>(-1);
  // Phase 1c overlay: message_id → accumulated live text, insertion-ordered
  // (a Map preserves insertion order). Mutated in place; `streamingText` is
  // the derived, concatenated view.
  const overlay = useRef<Map<string, string>>(new Map());
  // message_ids whose durable `agent_message` already landed — used to drop
  // a late / out-of-order straggler chunk so the durable text always wins.
  const finalized = useRef<Set<string>>(new Set());

  useEffect(() => {
    if (!sessionId) return;

    setEvents([]);
    setStreamingText("");
    highWater.current = -1;
    overlay.current = new Map();
    finalized.current = new Set();

    const tail = () => [...overlay.current.values()].join("");

    const unsubscribe = subscribeSession(sessionId, {
      onEvent: (e) => {
        if (e.idx <= highWater.current) return;
        highWater.current = e.idx;

        // Merge-safety: the durable record supersedes the live overlay.
        // When the terminal assistant `agent_message` lands, drop its
        // overlay entry (its full text is now in the durable stream) and
        // mark it finalized so a late straggler chunk can't re-add text.
        // A run terminal clears the whole overlay — this also collects a
        // crashed turn that streamed partials but produced no final message.
        // Pruning is FLUSHED synchronously (in the same update batch as the
        // event append) so there's never a frame where both the durable
        // message and its stale tail render together.
        const ev = e.event;
        if (ev.type === "agent_message" && ev.role === "assistant") {
          finalized.current.add(ev.message_id);
          if (overlay.current.delete(ev.message_id)) setStreamingText(tail());
        } else if (ev.type === "run_completed" || ev.type === "run_interrupted") {
          if (overlay.current.size > 0) {
            overlay.current.clear();
            setStreamingText("");
          }
        }

        setEvents((prev) => {
          const next = [...prev, e];
          if (opts.cap && next.length > opts.cap) {
            return next.slice(next.length - opts.cap);
          }
          return next;
        });
      },
      onDelta: ({ messageId, chunk }) => {
        // The durable message already won for this id → ignore the straggler.
        if (finalized.current.has(messageId)) return;
        overlay.current.set(messageId, (overlay.current.get(messageId) ?? "") + chunk);
        setStreamingText(tail());
      },
    });

    return unsubscribe;
  }, [sessionId, opts.cap]);

  return { events, streamingText };
}
