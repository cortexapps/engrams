import { API_BASE } from "./api";
import type { IndexedEvent, SessionEvent, SessionEventKind } from "./types";

// The native EventSource does not let us pass a Last-Event-ID header
// directly — but it sends one automatically on auto-reconnect, and we
// can also pass `?since=N` on first connect to replay from a known
// point. The coordinator takes the max of `?since=` and the
// `Last-Event-ID` header, so an explicit `?since` never goes backward
// across a reconnect.

export interface SseHandlers {
  onEvent: (e: IndexedEvent) => void;
  onError?: (err: Event) => void;
  onLagged?: (missed: number) => void;
}

/**
 * Subscribe to `GET /sessions/:id/events`. Returns a `close()` thunk.
 * `since` defaults to -1 (replay everything from the start of the log).
 */
export function subscribeSession(sessionId: string, handlers: SseHandlers, since = -1): () => void {
  const url = `${API_BASE}/sessions/${sessionId}/events?since=${since}`;
  const es = new EventSource(url);

  const dispatch = (kind: SessionEventKind, ev: MessageEvent) => {
    const idx = Number(ev.lastEventId);
    if (!Number.isFinite(idx)) return;
    let payload: SessionEvent | null = null;
    let rewound = false;
    let recoveryEpoch = 0;
    try {
      const raw = JSON.parse(ev.data) as Record<string, unknown>;
      // ADR 0028 A.log: the coordinator folds `_rewound` +
      // `_recovery_epoch` into the data object. Lift them onto the
      // IndexedEvent and strip them so the typed SessionEvent stays
      // clean.
      rewound = raw._rewound === true;
      recoveryEpoch = typeof raw._recovery_epoch === "number" ? raw._recovery_epoch : 0;
      delete raw._rewound;
      delete raw._recovery_epoch;
      // The coordinator emits the SSE `event:` field as the discriminant
      // string and the `data:` body as the SessionEvent JSON. The body
      // already carries `type: <kind>`, but be defensive in case a
      // future shape changes.
      payload = { ...raw, type: kind } as SessionEvent;
    } catch {
      return;
    }
    handlers.onEvent({ idx, event: payload, rewound, recoveryEpoch });
  };

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
    es.addEventListener(kind, (ev) => dispatch(kind, ev as MessageEvent));
  }

  // The coordinator surfaces broadcast lag as an `event: lagged` frame.
  es.addEventListener("lagged", (ev) => {
    try {
      const { missed } = JSON.parse((ev as MessageEvent).data) as {
        missed: number;
      };
      handlers.onLagged?.(missed);
    } catch {
      /* ignore */
    }
  });

  es.onerror = (err) => handlers.onError?.(err);

  return () => es.close();
}
