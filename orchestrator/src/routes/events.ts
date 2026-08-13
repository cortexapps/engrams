/**
 * SSE events route (ADR 0051 Task 20).
 *
 * GET /api/v1/sessions/:id/events
 *
 * Streams SessionEvents from the control plane as SSE frames, in the
 * hand-built JSON envelope format the web parser (web/src/sse.ts) expects:
 *
 *   id: <idx>          (omitted for lagged frames — no idx → no id line)
 *   event: <kind>
 *   data: {"idx":<number|null>,"kind":"<kind>","payload_json":"..."}
 *
 * Cursor: max(?since=, Last-Event-ID), NaN-guarded — matches the
 * coordinator's "never goes backward" rule (web/src/sse.ts).
 *
 * Keepalive: coordinator emits SSE comments every 15s; Hono's writeSSE
 * cannot emit raw comments, so an empty `event: ping` frame is used instead
 * (web/src/sse.ts listens per-kind, ignores unknown → transparent).
 *
 * Bigint handling: idx is `optional int64` → `bigint | undefined` in
 * protobuf-es. idx 0 (bigint 0n) is NOT absent — we check `!== undefined`,
 * not truthiness. Number(0n) === 0 — safe for the wire envelope.
 *
 * Injectable deps for tests: see makeEventsRoute(deps).
 */

import { Hono, type Context } from "hono";
import { streamSSE } from "hono/streaming";
import { sessions as defaultSessions } from "../control-plane/client.ts";
import { makeGuard } from "./guard.ts";
import type { GetSession, ResolveOwner } from "./guard.ts";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Subset of SessionService client used by the events route. */
export interface SessionsClient {
  streamEvents(
    req: { sessionId: string; since?: bigint },
    options?: { signal?: AbortSignal },
  ): AsyncIterable<{ idx?: bigint; kind: string; payloadJson: string }>;
}

/** Injectable deps for the events route. */
export interface EventsDeps {
  sessions?: SessionsClient;
  getSession?: GetSession;
  resolveOwner?: ResolveOwner;
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

export function makeEventsRoute(deps?: EventsDeps): Hono {
  const app = new Hono();
  const sessionsClient: SessionsClient =
    (deps?.sessions as SessionsClient | undefined) ??
    (defaultSessions as unknown as SessionsClient);
  const guardFn = makeGuard(deps?.getSession, deps?.resolveOwner);

  app.get("/api/v1/sessions/:id/events", async (c) => {
    // 1. Auth + ownership check — throws HTTPException on failure.
    await guardFn(c, "read");

    return streamSessionEventsSSE(c, c.req.param("id"), sessionsClient);
  });

  return app;
}

/** Stream one control-plane session with the canonical browser SSE contract. */
export function streamSessionEventsSSE(
  c: Context,
  sessionId: string,
  sessionsClient: SessionsClient,
): Response {
  return streamSSE(c, async (stream) => {
    // 2. Compute replay cursor: max(?since, Last-Event-ID).
    //    Parse as BigInt DIRECTLY from the string — the coordinator `idx`
    //    is int64, so a `Number(...)` round-trip would silently lose
    //    precision past 2^53 and resolve a reconnect to the wrong idx
    //    (replayed/skipped events). Only non-negative integer strings
    //    qualify; negatives (`?since=-1` "from start"), floats, and
    //    non-numeric drop out → `undefined` ≡ from the start of the log.
    const cursors = [c.req.query("since"), c.req.header("last-event-id")]
      .filter((v): v is string => typeof v === "string" && /^\d+$/.test(v))
      .map((v) => BigInt(v));
    const since = cursors.length
      ? cursors.reduce((a, b) => (b > a ? b : a))
      : undefined;

    // 3. Open upstream server-stream. Pass the browser's AbortSignal so
    //    a client disconnect triggers RST on the upstream gRPC stream.
    const upstream = sessionsClient.streamEvents(
      { sessionId, since },
      { signal: c.req.raw.signal },
    );

    // 4. Keepalive ping every 15 s (mirrors coordinator's axum KeepAlive).
    const ping = setInterval(
      () => void stream.writeSSE({ data: "", event: "ping" }),
      15_000,
    );

    try {
      for await (const ev of upstream) {
        // idx is optional int64 → bigint | undefined in protobuf-es.
        // IMPORTANT: 0n is a valid idx — check !== undefined, NOT !ev.idx.
        const hasIdx = ev.idx !== undefined;
        await stream.writeSSE({
          // Only set SSE id when there is an actual idx. Lagged frames
          // (idx unset) must NOT emit an id: line — reconnect cursors
          // must never be disturbed by a lag notification.
          ...(hasIdx ? { id: String(ev.idx) } : {}),
          event: ev.kind,
          data: JSON.stringify({
            // Normalise to JS number (safe: event counts never exceed
            // Number.MAX_SAFE_INTEGER in practice).
            idx: hasIdx ? Number(ev.idx) : null,
            kind: ev.kind,
            payload_json: ev.payloadJson,
          }),
        });
      }
    } finally {
      clearInterval(ping);
    }
  });
}

export default makeEventsRoute();
