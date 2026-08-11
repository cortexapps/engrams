import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createClient, type Transport } from "@connectrpc/connect";
import { useTransport } from "@connectrpc/connect-query";
import { subscribeSession, type SseHandlers } from "../sse";
import { SessionService } from "../gen/engram/app/v1/session_pb";
import {
  decodePage,
  loadTranscript,
  loadWindow,
  mergeIndexed,
  type ListEventsPage,
} from "../lib/sessionWindow";
import type { IndexedEvent } from "../lib/types";

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
  /** Transcript exists below the oldest loaded window — `loadOlder` reads it. */
  hasMore: boolean;
  /** A backfill page is in flight. */
  loadingOlder: boolean;
  /** Prepend the next window of older transcript. A no-op while one is in
   *  flight, or when the start of the log is already held. */
  loadOlder: () => void;
  /** The oldest idx held in full fidelity. It changes ONLY on a prepend, so
   *  the viewport uses it as the scroll-anchor key. */
  oldestIdx: number | null;
}

/**
 * Loads a session's transcript and subscribes to its live tail.
 *
 * The open sequence (lib/sessionWindow.ts carries the why):
 *   1. read the SPINE (cheap kinds from idx 0) and the TAIL WINDOW (the
 *      newest ~200 events, unfiltered) in parallel;
 *   2. snap the window's lower edge DOWN to the `run_started` at or before
 *      it, so the transcript never opens on half a turn;
 *   3. merge both into ONE idx-ordered, idx-deduped array (`buildMessages`
 *      takes a single input);
 *   4. subscribe SSE with `since = <highest idx held>`. The coordinator
 *      subscribes to the live bus BEFORE it reads the backlog, so no event
 *      falls between the unary read and the stream.
 * `loadOlder` repeats steps 2-3 downward from the current floor.
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
export function useSessionEvents(sessionId: string | undefined): SessionEventsState {
  const transport = useTransport();
  const [events, setEvents] = useState<IndexedEvent[]>([]);
  const [streamingText, setStreamingText] = useState("");
  const [hasMore, setHasMore] = useState(false);
  const [loadingOlder, setLoadingOlder] = useState(false);
  const [oldestIdx, setOldestIdx] = useState<number | null>(null);

  // The highest idx observed — used by the dedup pass below.
  const highWater = useRef<number>(-1);
  // Phase 1c overlay: message_id → accumulated live text, insertion-ordered
  // (a Map preserves insertion order). Mutated in place; `streamingText` is
  // the derived, concatenated view.
  const overlay = useRef<Map<string, string>>(new Map());
  // message_ids whose durable `agent_message` already landed — used to drop
  // a late / out-of-order straggler chunk so the durable text always wins.
  const finalized = useRef<Set<string>>(new Set());

  // Windowing cursors. `floor` is the oldest idx held in full fidelity,
  // `logStart` the lowest idx the log holds, `runStarts` the turn boundaries
  // (from the spine, which reads every `run_started` from idx 0).
  const floor = useRef<number | null>(null);
  const logStart = useRef<number>(0);
  const runStarts = useRef<number[]>([]);
  const backfilling = useRef(false);
  // `hasMore` is read inside async callbacks; the ref is the value they see.
  const hasMoreRef = useRef(false);
  // Bumped on every teardown, so an in-flight page from a previous session
  // (or a previous mount) can never write into the current one.
  const generation = useRef(0);
  const abort = useRef<AbortController | null>(null);

  const setHasMoreTracked = useCallback((value: boolean) => {
    hasMoreRef.current = value;
    setHasMore(value);
  }, []);

  const list = useMemo<ListEventsPage>(
    () => eventPageLister(transport, sessionId ?? "", () => abort.current?.signal),
    [transport, sessionId],
  );

  const loadOlder = useCallback(() => {
    const from = floor.current;
    if (backfilling.current || !hasMoreRef.current || from == null) return;
    backfilling.current = true;
    setLoadingOlder(true);
    const gen = generation.current;
    void (async () => {
      try {
        const win = await loadWindow(list, BigInt(from), runStarts.current);
        if (gen !== generation.current) return;
        // A window with no floor read nothing older — hold the cursor still.
        const next = win.floor ?? from;
        floor.current = next;
        setOldestIdx(next);
        setHasMoreTracked(!win.exhausted && next > logStart.current);
        setEvents((prev) => mergeIndexed(prev, win.events));
      } catch (err) {
        console.warn("loading older transcript failed", err);
      } finally {
        if (gen === generation.current) {
          backfilling.current = false;
          setLoadingOlder(false);
        }
      }
    })();
  }, [list, setHasMoreTracked]);

  useEffect(() => {
    if (!sessionId) return;

    const gen = generation.current;
    const controller = new AbortController();
    abort.current = controller;

    setEvents([]);
    setStreamingText("");
    setHasMoreTracked(false);
    setLoadingOlder(false);
    setOldestIdx(null);
    highWater.current = -1;
    overlay.current = new Map();
    finalized.current = new Set();
    floor.current = null;
    logStart.current = 0;
    runStarts.current = [];
    backfilling.current = false;

    const tail = () => [...overlay.current.values()].join("");

    const handlers: SseHandlers = {
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

        // A live `run_started` is one more turn boundary. It is always ABOVE
        // the floor, so it can never change a snap — but keeping the list
        // whole holds one source of truth for boundaries.
        if (ev.type === "run_started") runStarts.current = [...runStarts.current, e.idx];

        setEvents((prev) => [...prev, e]);
      },
      onDelta: ({ messageId, chunk }) => {
        // The durable message already won for this id → ignore the straggler.
        if (finalized.current.has(messageId)) return;
        overlay.current.set(messageId, (overlay.current.get(messageId) ?? "") + chunk);
        setStreamingText(tail());
      },
    };

    let unsubscribe: (() => void) | undefined;

    void (async () => {
      let since = -1;
      try {
        const loaded = await loadTranscript(list);
        if (gen !== generation.current) return;
        runStarts.current = loaded.runStarts;
        logStart.current = loaded.logStart;
        floor.current = loaded.floor;
        setOldestIdx(loaded.floor);
        setHasMoreTracked(loaded.hasMore);
        setEvents(loaded.events);
        since = loaded.since;
        highWater.current = since;
      } catch (err) {
        if (gen !== generation.current) return;
        // The windowed read failed (offline, a passthrough hiccup, an aborted
        // page). Fall back to the full SSE replay: slow on a long session, but
        // the reader never faces an empty transcript because a page failed.
        console.warn("windowed transcript read failed; replaying the whole log", err);
        since = -1;
        highWater.current = -1;
      }
      if (gen !== generation.current) return;
      unsubscribe = subscribeSession(sessionId, handlers, since);
    })();

    return () => {
      generation.current += 1;
      controller.abort();
      abort.current = null;
      unsubscribe?.();
    };
  }, [sessionId, list, setHasMoreTracked]);

  return { events, streamingText, hasMore, loadingOlder, loadOlder, oldestIdx };
}

/**
 * Bind {@link ListEventsPage} to the Connect passthrough. The signal getter is
 * read per call, so a teardown also cancels the pages already in flight.
 */
export function eventPageLister(
  transport: Transport,
  sessionId: string,
  signal: () => AbortSignal | undefined,
): ListEventsPage {
  const client = createClient(SessionService, transport);
  return async (req) => {
    const resp = await client.listSessionEvents(
      {
        sessionId,
        ...(req.afterIdx !== undefined ? { afterIdx: BigInt(req.afterIdx) } : {}),
        ...(req.beforeIdx !== undefined ? { beforeIdx: req.beforeIdx } : {}),
        limit: BigInt(req.limit),
        kinds: req.kinds ? [...req.kinds] : [],
        toolNames: req.toolNames ? [...req.toolNames] : [],
      },
      { signal: signal() },
    );
    return {
      events: decodePage(resp.events),
      count: resp.events.length,
      next: Number(resp.nextAfterIdx),
    };
  };
}
