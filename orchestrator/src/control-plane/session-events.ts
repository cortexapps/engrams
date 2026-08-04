/**
 * Reverse-channel session-event reader + curation (ADR 0060 P1.2).
 *
 * The coordinator's append-only session event log is the durable, ordered,
 * replayable inbox for the reverse channel (ADR 0060 Decision 1). The
 * Session listeners walk it forward in bounded pages via the unary
 * `ListSessionEvents` RPC before tailing the server stream. Curation + terminal
 * detection live here so catch-up and stream delivery see identical events.
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
  "tool_call_requested",
  "tool_result_submitted",
  "tool_call_completed",
  "integration_asset",
  "file_shared",
]);

/** Whether an event kind is forwarded to a thread as content. */
export function curated(kind: string): boolean {
  return CURATED_KINDS.has(kind);
}

/** How the reverse channel reports a terminal session state to a thread:
 *  `completed` (success → closing summary), `failed` (the run errored → ❌), or
 *  `neutral` (the sandbox was reclaimed — NOT a failure). */
export type TerminalOutcome = "completed" | "failed" | "neutral";

/**
 * Terminal `engram_core::SessionState` values (snake_case on the wire) → the
 * outcome each maps to. A `status_changed` into one of these keys ends the
 * ingest loop:
 *  - `completed` → success.
 *  - `failed`    → the run errored.
 *  - `dead`      → neutral: the sandbox was reclaimed out from under the session
 *    (idle host roll, `host_lost`, dev-stack churn). The work up to that point
 *    stands; the thread just can't continue — so it is NOT reported as a failure.
 *    (`host_lost` is a transient on the way to `dead` and is itself non-terminal;
 *    only `dead` exits the loop.)
 * `run_completed` is deliberately absent (ADR 0060 Invariant 2): a session
 * re-runs on a follow-up mention, so only a terminal session state exits.
 */
const TERMINAL_OUTCOME: Readonly<Record<string, TerminalOutcome>> = {
  completed: "completed",
  failed: "failed",
  dead: "neutral",
};

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
  /** Set when the page contained a terminal `status_changed`. `outcome`
   *  classifies it — `completed` (success), `failed` (the run errored), or
   *  `neutral` (the sandbox was reclaimed; not a failure — see TERMINAL_OUTCOME). */
  terminal?: { outcome: TerminalOutcome };
  /** Text of the LAST assistant `agent_message` in this page, if any. Drives the
   *  closing-summary enrichment (ADR 0060, onComplete): agent_message is not a
   *  curated content kind, but bounded readers already walk every page, so this
   *  remains available without an extra coordinator round-trip. */
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

/** Apply the exact reverse-channel curation rules to one streamed frame. */
export function curateWireEvent(ev: WireEvent): CuratedEvent | undefined {
  if (ev.idx === undefined) return undefined;
  if (ev.kind === "agent_message") {
    return parseAgentMessage(ev.payloadJson) === undefined
      ? undefined
      : { idx: ev.idx, kind: ev.kind, payloadJson: ev.payloadJson };
  }
  return curated(ev.kind)
    ? { idx: ev.idx, kind: ev.kind, payloadJson: ev.payloadJson }
    : undefined;
}

/** The injectable list seam: one bounded read of the log. `signal` aborts the
 * underlying RPC when the caller's deadline fires (issue #704). */
export type ListEventsFn = (
  sessionId: string,
  after: bigint,
  limit: bigint,
  signal?: AbortSignal,
) => Promise<{ events: WireEvent[]; nextAfterIdx: bigint }>;

/** Page size per bounded read. */
const PAGE_LIMIT = 200n;

/** Production list fn: the coordinator's unary `ListSessionEvents` RPC. */
const defaultList: ListEventsFn = async (sessionId, after, limit, signal) => {
  const resp = await sessions.listSessionEvents(
    { sessionId, afterIdx: after, limit },
    signal ? { signal } : {},
  );
  return { events: resp.events, nextAfterIdx: resp.nextAfterIdx };
};

/**
 * Read one bounded page of a session's log starting strictly after `after`,
 * curated for the reverse channel. Returns the forwarded content events, the
 * next cursor, and — if the page crossed into a terminal session state — a
 * `terminal` marker the listener uses to send its closing signal and stop.
 *
 * `list` is injectable for tests; production uses the coordinator RPC.
 */
export async function readSessionEventsBounded(
  sessionId: string,
  after: bigint,
  list: ListEventsFn = defaultList,
  signal?: AbortSignal,
): Promise<BoundedRead> {
  const { events: page, nextAfterIdx } = await list(sessionId, after, PAGE_LIMIT, signal);
  const events: CuratedEvent[] = [];
  let terminal: { outcome: TerminalOutcome } | undefined;
  let lastAssistantText: string | undefined;
  for (const ev of page) {
    if (ev.kind === "status_changed") {
      const outcome = parseTerminalOutcome(ev.payloadJson);
      if (outcome) terminal = { outcome };
      continue; // a control signal, never forwarded as content
    }
    if (ev.kind === "agent_message") {
      const msg = parseAgentMessage(ev.payloadJson);
      if (msg !== undefined) {
        if (msg.role === "assistant") {
          lastAssistantText = msg.text; // keep the latest, for the closing summary
        }
        // Forward the assistant's turns and the user prompt echo as content.
        // Consumers pick their role: the Slack workflow posts only assistant
        // text (communication-policy.ts), the title consumer reads only the
        // user echo. System notes never forward.
        const curatedEvent = curateWireEvent(ev);
        if (curatedEvent) events.push(curatedEvent);
      }
      continue;
    }
    const curatedEvent = curateWireEvent(ev);
    if (curatedEvent) events.push(curatedEvent);
  }
  return { events, nextAfter: nextAfterIdx, terminal, lastAssistantText };
}

/** Parse an `agent_message` payload into the two roles the reverse channel
 *  forwards: the assistant's turns and the user prompt echo (the title
 *  consumer reads the latter). System notes and malformed payloads → undefined. */
function parseAgentMessage(
  payloadJson: string,
): { role: "assistant" | "user"; text: string } | undefined {
  try {
    const p = JSON.parse(payloadJson) as { role?: unknown; text?: unknown };
    return (p?.role === "assistant" || p?.role === "user") && typeof p.text === "string"
      ? { role: p.role, text: p.text }
      : undefined;
  } catch {
    return undefined;
  }
}

/** Map a session's CURRENT status (GetSession) to its terminal outcome, or
 *  undefined while non-terminal. Same vocabulary as the event-log transition. */
export function terminalOutcomeForStatus(status: string): TerminalOutcome | undefined {
  return TERMINAL_OUTCOME[status];
}

/** Map a `status_changed` payload's `to` state to its terminal outcome, or
 *  undefined if `to` is not a terminal state. */
export function parseTerminalOutcome(payloadJson: string): TerminalOutcome | undefined {
  try {
    const to: unknown = (JSON.parse(payloadJson) as { to?: unknown })?.to;
    return typeof to === "string" ? TERMINAL_OUTCOME[to] : undefined;
  } catch {
    return undefined;
  }
}
