import { API_BASE } from "./lib/base";
import { parseOrchestratorFrame } from "./events";
import type { IndexedEvent, SessionEventKind } from "./lib/types";

// The native EventSource does not let us pass a Last-Event-ID header
// directly — but it sends one automatically on auto-reconnect, and we
// can also pass `?since=N` on first connect to replay from a known
// point. The orchestrator (and coordinator) takes the max of `?since=`
// and the `Last-Event-ID` header, so an explicit `?since` never goes
// backward across a reconnect.

/** Phase 1c: one live token delta of the in-flight assistant message. */
export interface SessionDelta {
  runId: string;
  messageId: string;
  chunk: string;
}

export interface SseHandlers {
  onEvent: (e: IndexedEvent) => void;
  onError?: (err: Event) => void;
  onLagged?: (missed: number) => void;
  /**
   * Phase 1c: an EPHEMERAL token chunk (`agent_message_chunk`). Delivered
   * on a SEPARATE path from `onEvent` because these frames carry NO `idx`
   * (they are never persisted, so `parseOrchestratorFrame` — which drops
   * idx-less frames — must not see them, and they must never enter the
   * durable event array nor disturb the reconnect cursor).
   */
  onDelta?: (d: SessionDelta) => void;
}

/** Durable event discriminants this client subscribes to explicitly. */
export const SESSION_EVENT_KINDS: readonly SessionEventKind[] = [
  "status_changed",
  "exec_started",
  "exec_completed",
  "stdout",
  "stderr",
  "snapshot_taken",
  "evicted",
  "resumed",
  "resume_started",
  "run_started",
  "agent_message",
  "tool_call_started",
  "tool_call_completed",
  "browser_activity",
  "tool_call_requested",
  "tool_result_submitted",
  "run_completed",
  "run_interrupted",
  "harness_idle",
  // ADR 0107: mode directives (the plan chip + thread mode markers).
  "harness_mode_changed",
  "prompt_queued",
  "prompt_edited",
  "prompt_dequeued",
  "prompt_steered",
  "integration_asset",
  "file_shared",
  "recovered_from_checkpoint",
  // ADR 0090: the durability-rollback warning marker.
  "durability_rollback",
  // ADR 0054: historical interactive AskUserQuestion round-trip.
  "user_question",
  "question_answered",
  // ADR 0054 Flavor A: rich file-change diffs.
  "file_changed",
];

/**
 * Subscribe to `GET /api/v1/sessions/:id/events`. Returns a `close()` thunk.
 * `since` defaults to -1 (replay everything from the start of the log).
 *
 * The EventSource points at the orchestrator route (Task 26, Task 28);
 * the vite proxy /api catch-all routes all /api/v1/… → orchestrator :8787.
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
  for (const kind of SESSION_EVENT_KINDS) {
    es.addEventListener(kind, (ev) => {
      const indexed = parseOrchestratorFrame((ev as MessageEvent).data, kind);
      if (indexed) handlers.onEvent(indexed);
    });
  }

  // Phase 1c: ephemeral token chunks. Like `lagged`, these arrive with no
  // SSE `id:` line (idx is null) and are handled OUT of the generic event
  // path — they're a live-only overlay, never durable events. We parse the
  // two-level envelope directly and hand the chunk to `onDelta`.
  es.addEventListener("agent_message_chunk", (ev) => {
    try {
      const envelope = JSON.parse((ev as MessageEvent).data) as {
        idx: number | null;
        kind: string;
        payload_json: string;
      };
      const p = JSON.parse(envelope.payload_json) as {
        run_id: string;
        message_id: string;
        chunk: string;
      };
      if (typeof p.message_id === "string" && typeof p.chunk === "string") {
        handlers.onDelta?.({ runId: p.run_id, messageId: p.message_id, chunk: p.chunk });
      }
    } catch {
      /* ignore malformed chunk frame — the durable message will still land */
    }
  });

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
