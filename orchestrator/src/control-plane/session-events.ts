/**
 * Reverse-channel session-event reader + curation (ADR 0059 P1.2).
 *
 * The coordinator's append-only session event log is the durable, ordered,
 * replayable inbox for the reverse channel (ADR 0059 Decision 1). The
 * SessionIngestWorkflow pump walks it forward in bounded pages via the unary
 * `ListSessionEvents` RPC, forwarding only the events an external surface
 * (Slack, …) cares about and detecting the terminal status_changed that ends
 * the loop. Curation + terminal detection live here so they are unit-testable
 * in isolation; the workflow just drives the loop.
 */

import { sessions } from "./client.ts";

/**
 * Curated event kinds the reverse channel forwards to a thread as content.
 * `status_changed` is deliberately absent — it is a control signal handled by
 * terminal detection, not content. Everything else (stdout, agent_message,
 * chunks, tool calls, idle, …) is noise for an external surface.
 *
 * Values match `SessionEvent::kind()` in engram-coordinator (state.rs).
 */
export const CURATED_KINDS: ReadonlySet<string> = new Set([
  "run_started",
  "run_completed",
  "user_question",
  "question_answered",
  "integration_asset",
  "file_shared",
]);

/** Whether an event kind is forwarded to a thread as content. */
export function curated(kind: string): boolean {
  return CURATED_KINDS.has(kind);
}

/**
 * Terminal `engram_core::SessionState` values (snake_case on the wire). A
 * `status_changed` into one of these ends the ingest loop. `run_completed` is
 * NOT here on purpose (ADR 0059 Invariant 2): a session re-runs on a follow-up
 * mention, so only a terminal session state exits.
 */
const TERMINAL_STATES: ReadonlySet<string> = new Set(["completed", "failed", "dead"]);

/** A forwarded content event (a curated SessionEvent, narrowed). */
export interface CuratedEvent {
  idx: bigint;
  kind: string;
  payloadJson: string;
}

/** One bounded page of the log, curated for the reverse channel. */
export interface BoundedRead {
  /** Curated content events, in idx order. */
  events: CuratedEvent[];
  /** Cursor to pass as `after` next call. Echoes `after` on an empty/tail page. */
  nextAfter: bigint;
  /** Set when the page contained a terminal `status_changed`; `ok` = Completed. */
  terminal?: { ok: boolean };
  /** Text of the LAST assistant `agent_message` in this page, if any. Drives the
   *  closing-summary enrichment (ADR 0059, onComplete): agent_message is not a
   *  curated content kind, but the pump already walks every page, so we surface
   *  the last assistant text here and the pump tracks the most-recent across
   *  pages — no extra coordinator round-trip. */
  lastAssistantText?: string;
}

/**
 * The slice of a proto `SessionEvent` the reader inspects — a structural seam
 * so tests inject plain objects without constructing proto messages, and the
 * reader is decoupled from codegen details.
 */
export interface WireEvent {
  idx?: bigint;
  kind: string;
  payloadJson: string;
}

/** The injectable list seam: one bounded read of the log. */
export type ListEventsFn = (
  sessionId: string,
  after: bigint,
  limit: bigint,
) => Promise<{ events: WireEvent[]; nextAfterIdx: bigint }>;

/** Page size per bounded read — keeps the pump's step count proportional to
 *  event activity (ADR 0059 §DBOS adoption, `operation_outputs` growth). */
const PAGE_LIMIT = 200n;

/** Production list fn: the coordinator's unary `ListSessionEvents` RPC. */
const defaultList: ListEventsFn = async (sessionId, after, limit) => {
  const resp = await sessions.listSessionEvents({ sessionId, afterIdx: after, limit });
  return { events: resp.events, nextAfterIdx: resp.nextAfterIdx };
};

/**
 * Read one bounded page of a session's log starting strictly after `after`,
 * curated for the reverse channel. Returns the forwarded content events, the
 * next cursor, and — if the page crossed into a terminal session state — a
 * `terminal` marker the pump uses to send its closing signal and stop.
 *
 * `list` is injectable for tests; production uses the coordinator RPC.
 */
export async function readSessionEventsBounded(
  sessionId: string,
  after: bigint,
  list: ListEventsFn = defaultList,
): Promise<BoundedRead> {
  const { events: page, nextAfterIdx } = await list(sessionId, after, PAGE_LIMIT);
  const events: CuratedEvent[] = [];
  let terminal: { ok: boolean } | undefined;
  let lastAssistantText: string | undefined;
  for (const ev of page) {
    if (ev.kind === "status_changed") {
      const to = parseTerminalState(ev.payloadJson);
      if (to) terminal = { ok: to === "completed" };
      continue; // a control signal, never forwarded as content
    }
    if (ev.kind === "agent_message") {
      const text = parseAssistantText(ev.payloadJson);
      if (text !== undefined) {
        lastAssistantText = text; // keep the latest, for the closing summary
        // Forward the assistant's text to the thread as content (the workflow
        // coalesces consecutive ones into one per-turn message). Only the
        // assistant role — the prompt echo (`user`) and system notes never post.
        if (ev.idx !== undefined) {
          events.push({ idx: ev.idx, kind: "agent_message", payloadJson: ev.payloadJson });
        }
      }
      continue;
    }
    if (curated(ev.kind) && ev.idx !== undefined) {
      events.push({ idx: ev.idx, kind: ev.kind, payloadJson: ev.payloadJson });
    }
  }
  return { events, nextAfter: nextAfterIdx, terminal, lastAssistantText };
}

/** Extract the text of an `agent_message` payload iff it is from the assistant
 *  (the prompt echo rides `role:"user"`, system notes `role:"system"`). */
function parseAssistantText(payloadJson: string): string | undefined {
  try {
    const p = JSON.parse(payloadJson) as { role?: unknown; text?: unknown };
    return p?.role === "assistant" && typeof p.text === "string" ? p.text : undefined;
  } catch {
    return undefined;
  }
}

/** Extract a terminal `to` state from a status_changed payload, else undefined. */
function parseTerminalState(payloadJson: string): string | undefined {
  try {
    const to: unknown = (JSON.parse(payloadJson) as { to?: unknown })?.to;
    return typeof to === "string" && TERMINAL_STATES.has(to) ? to : undefined;
  } catch {
    return undefined;
  }
}
