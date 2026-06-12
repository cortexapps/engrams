import { API_BASE } from "./api";
import { parseOrchestratorFrame } from "./events";
import type { IndexedEvent, SessionEventKind } from "./types";

// The native EventSource does not let us pass a Last-Event-ID header
// directly — but it sends one automatically on auto-reconnect, and we
// can also pass `?since=N` on first connect to replay from a known
// point. The orchestrator (and coordinator) takes the max of `?since=`
// and the `Last-Event-ID` header, so an explicit `?since` never goes
// backward across a reconnect.

export interface SseHandlers {
  onEvent: (e: IndexedEvent) => void;
  onError?: (err: Event) => void;
  onLagged?: (missed: number) => void;
}

/**
 * Subscribe to `GET /api/v1/sessions/:id/events`. Returns a `close()` thunk.
 * `since` defaults to -1 (replay everything from the start of the log).
 *
 * The EventSource points at the orchestrator-proxied route (Task 26);
 * the vite proxy regex rule routes /api/v1/sessions/:id/events → 8787
 * BEFORE the coordinator /api catch-all.
 *
 * Wire format: orchestrator SSE envelope
 *   event: <kind>
 *   id:    <idx>          (omitted for lagged — no SSE id disturbed)
 *   data:  {"idx":<n|null>,"kind":"<kind>","payload_json":"..."}
 *
 * `payload_json`, when JSON-parsed, yields the SessionEvent fields PLUS
 * the ADR 0028 rewind metadata (_rewound, _recovery_epoch) folded in by
 * crates/engram-coordinator/src/api/events.rs :: with_rewind_meta.
 * parseOrchestratorFrame (web/src/events.ts) handles the two-level parse
 * and metadata lift, and is exported for the contract test.
 */
export function subscribeSession(sessionId: string, handlers: SseHandlers, since = -1): () => void {
  const url = `${API_BASE}/sessions/${sessionId}/events?since=${since}`;
  const es = new EventSource(url);

  // Wire one listener per discriminant so EventSource doesn't deliver
  // them all through `onmessage` (which only catches frames with no
  // explicit `event:` field — namely keep-alives).
  const kinds: SessionEventKind[] = [
    "status_changed",
    "exec_started",
    "exec_completed",
    "stdout",
    "stderr",
    "snapshot_taken",
    "evicted",
    "resumed",
    "run_started",
    "agent_message",
    "tool_call_started",
    "tool_call_completed",
    "run_completed",
    "run_interrupted",
    "harness_idle",
    "pull_request_opened",
    "file_shared",
    "recovered_from_checkpoint",
  ];

  for (const kind of kinds) {
    es.addEventListener(kind, (ev) => {
      const indexed = parseOrchestratorFrame((ev as MessageEvent).data, kind);
      if (indexed) handlers.onEvent(indexed);
    });
  }

  // The orchestrator surfaces broadcast lag as an `event: lagged` frame
  // with no SSE `id:` line (so it never disturbs Last-Event-ID / reconnect
  // cursor). parseOrchestratorFrame returns null for lagged — handle it here.
  es.addEventListener("lagged", (ev) => {
    try {
      const envelope = JSON.parse((ev as MessageEvent).data) as {
        idx: null;
        kind: "lagged";
        payload_json: string;
      };
      const { missed } = JSON.parse(envelope.payload_json) as { missed: number };
      handlers.onLagged?.(missed);
    } catch {
      /* ignore */
    }
  });

  // `event: ping` keepalives: the orchestrator sends these every 15 s
  // instead of SSE comments (Hono's writeSSE can't emit raw comments).
  // No listener registered → EventSource silently drops unknown events.
  // Explicit no-op to document the intent:
  es.addEventListener("ping", () => {
    /* keepalive — intentionally ignored */
  });

  es.onerror = (err) => handlers.onError?.(err);

  return () => es.close();
}
