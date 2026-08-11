// The client half of the WINDOWED transcript read (`ListSessionEvents`).
//
// Opening a long session used to replay the WHOLE persisted log over SSE.
// Prod's largest session — 11 122 events / 5.5 MB — shipped 7.8 MB and cost
// ~6.4 s of EventSource dispatch + React + layout before the transcript was
// usable (the fold itself, `buildMessages`, is 27 ms: the bytes are the cost,
// not the logic). Four kinds — tool_call_requested, tool_call_started,
// tool_call_completed, tool_result_submitted — hold ~95 % of those bytes.
//
// So the client reads the log as two pieces and merges them into the ONE
// idx-ordered array `buildMessages` takes:
//
//   - the SPINE — every event from idx 0 for the CHEAP kinds, plus the few
//     tool calls that are conversation structure rather than tool detail (a
//     plan proposal, a question, the agent's task list). It keeps the
//     whole-session folds honest: the file-change rollup, the plan ordinals,
//     the composer mode chip and the queued-prompt rail all read the log from
//     its start. The spine carries whole-session FACTS, not conversation
//     PROSE — see `OFF_SPINE_KINDS`.
//   - the WINDOW — the newest ~200 events, UNFILTERED, so the tail the reader
//     actually looks at keeps full tool detail. A 200-event tail is 152 KB.
//     `loadOlder` prepends one more window per backfill.
//
// Every window edge SNAPS DOWN to the `run_started` at or before it, so a
// window never cuts a turn in half. An unsnapped edge strands the run receipt,
// the user bubble and the plan handoff on the other side of the boundary.
//
// Known limit: a deferred decision (a plan approval, a question answer) rides
// `tool_result_submitted`, which is NOT a spine kind. A decision made below the
// window floor stays invisible until the reader backfills that window, so an
// old card can read as unresolved. The card that MATTERS — the newest, still
// pending one — is at the tail by construction, inside the window.
//
// Known limit: a run below the window floor is not narrated at all until the
// reader backfills it — its `run_started` is loaded, but the prompt and the
// answer are not. That is the point: the session opens on its LAST page and
// grows upward.

import { parseEventFrame } from "../events";
import { SESSION_EVENT_KINDS } from "../sse";
import type { IndexedEvent } from "./types";

/** Events per transcript window. One window is the reader's working set. */
export const WINDOW = 200;

/** Events per spine page. The server clamps `limit` to 1000. */
export const SPINE_PAGE = 1000;

/** `before_idx` for the LAST page: i64::MAX, above every possible idx. */
export const TAIL_BEFORE_IDX = 9223372036854775807n;

/** Guards against an unbounded read if the server ever stops advancing its
 *  cursor. 50 pages = 50 000 spine events, far above any real session. */
const MAX_SPINE_PAGES = 50;

/** How far a snap may reach back to find its `run_started`. A turn longer than
 *  this stays unsnapped (the reader keeps the split turn and can backfill)
 *  rather than pulling an unbounded read into the session open. */
const MAX_SNAP_FILL = 1000;

/** The four kinds that carry ~95 % of a long session's bytes. */
const HEAVY_TOOL_KINDS: ReadonlySet<string> = new Set([
  "tool_call_requested",
  "tool_call_started",
  "tool_call_completed",
  "tool_result_submitted",
]);

/** Kinds the spine does NOT read in full. The heavy tool kinds are here for
 *  their BYTES; `agent_message` is here for its HEIGHT.
 *
 *  A spine that held `agent_message` sent few bytes but made the page as TALL
 *  as an unwindowed one: `buildMessages` folds a run's prose into ONE assistant
 *  bubble, so a 10 251-event session still rendered 13 bubbles up to 14 036 px
 *  tall — a 157 744 px transcript, identical to the unwindowed page. The
 *  product is "open on the LAST page and grow upward as the reader scrolls",
 *  so conversation TEXT has to be windowed too, not only tool detail.
 *
 *  What stays is the run SKELETON — `run_started` / `run_completed` /
 *  `run_interrupted` and the prompt_* kinds, all tiny — because the window
 *  edges snap to it, plus the kinds the whole-session folds read (file changes,
 *  mode, plan ordinals, the queued-prompt rail).
 *
 *  Consequence, and it is the intended one: a run below the window floor shows
 *  NOTHING until the reader backfills it. The user's prompt rides
 *  `agent_message{role:"user"}` and the wire filters on kind, not role, so it
 *  is deferred with the rest of that turn. `run_started.prompt_summary` would
 *  be a cheap stand-in and `buildMessages` uses it when it is set — but every
 *  harness sets it to `None` on purpose today (it would render the prompt twice
 *  beside the echo), so in practice the turn simply is not loaded yet. */
const OFF_SPINE_KINDS: ReadonlySet<string> = new Set<string>([
  ...HEAVY_TOOL_KINDS,
  "agent_message",
]);

/** Tools whose CALL is conversation structure — a plan card, a question card,
 *  the agent's task list — so the spine keeps them from idx 0. Claude exposes
 *  a registered tool as `mcp__engrams__<name>` while a generic request event
 *  carries the canonical registry name, so both spellings are sent. */
const STRUCTURAL_TOOLS = ["exit_plan_mode", "ask_user_question", "TaskCreate", "TaskUpdate"];

export const SPINE_TOOL_NAMES: readonly string[] = STRUCTURAL_TOOLS.flatMap((name) => [
  name,
  `mcp__engrams__${name}`,
]);

/** Cheap kinds in full, plus the two tool kinds the tool-name filter narrows. */
export const SPINE_KINDS: readonly string[] = [
  ...SESSION_EVENT_KINDS.filter((kind) => !OFF_SPINE_KINDS.has(kind)),
  "tool_call_requested",
  "tool_call_started",
];

/** Kinds the SSE feed dispatches. A page is filtered to the same set, so a
 *  replayed transcript holds exactly what a live one holds (the SSE layer
 *  drops an unregistered kind by never listening for it). */
const KNOWN_KINDS: ReadonlySet<string> = new Set<string>(SESSION_EVENT_KINDS);

/** One `ListSessionEvents` request, in the terms this module reasons in. */
export interface PageRequest {
  /** Events strictly AFTER this idx. Unset = from the start of the log. */
  afterIdx?: number;
  /** Events strictly BEFORE this idx, returned oldest-first. */
  beforeIdx?: bigint;
  limit: number;
  kinds?: readonly string[];
  toolNames?: readonly string[];
}

/** One decoded page. `count` is the RAW event count the server returned
 *  (before unknown kinds are dropped) — a short page means the log ran out,
 *  which is how every "is there more?" answer here is derived. */
export interface RawPage {
  events: IndexedEvent[];
  count: number;
  /** The server's `next_after_idx` — the forward cursor. */
  next: number;
}

/** The transport-free seam: the hook binds this to the Connect client, the
 *  tests bind it to a fixture log. */
export type ListEventsPage = (req: PageRequest) => Promise<RawPage>;

/** Decode a wire page through the SAME path the SSE feed uses. */
export function decodePage(
  events: readonly { idx?: bigint; kind: string; payloadJson: string }[],
): IndexedEvent[] {
  const out: IndexedEvent[] = [];
  for (const e of events) {
    // A durable event always carries an idx; a frame without one is not
    // replayable and never enters the transcript.
    if (e.idx == null) continue;
    if (!KNOWN_KINDS.has(e.kind)) continue;
    const parsed = parseEventFrame(Number(e.idx), e.kind, e.payloadJson);
    if (parsed) out.push(parsed);
  }
  return out;
}

/** Merge two idx-ASCENDING event runs into one, de-duped on idx. `b` wins a
 *  tie — it is always the fresher read of the same durable row. */
export function mergeIndexed(
  a: readonly IndexedEvent[],
  b: readonly IndexedEvent[],
): IndexedEvent[] {
  const out: IndexedEvent[] = [];
  let i = 0;
  let j = 0;
  while (i < a.length && j < b.length) {
    const l = a[i]!;
    const r = b[j]!;
    if (l.idx < r.idx) {
      out.push(l);
      i++;
    } else if (l.idx > r.idx) {
      out.push(r);
      j++;
    } else {
      out.push(r);
      i++;
      j++;
    }
  }
  while (i < a.length) out.push(a[i++]!);
  while (j < b.length) out.push(b[j++]!);
  return out;
}

/** The idx of every `run_started` — the turn boundaries a window snaps to. */
export function runStartIdxs(events: readonly IndexedEvent[]): number[] {
  const out: number[] = [];
  for (const e of events) if (e.event.type === "run_started") out.push(e.idx);
  return out;
}

/** The `run_started` at or before `idx`, or null when the log starts inside a
 *  turn (nothing to snap to). `runStarts` is ascending. */
export function snapTo(runStarts: readonly number[], idx: number): number | null {
  let found: number | null = null;
  for (const start of runStarts) {
    if (start > idx) break;
    found = start;
  }
  return found;
}

/** The whole spine: cheap kinds from idx 0, paged forward until exhausted. */
export async function loadSpine(list: ListEventsPage): Promise<IndexedEvent[]> {
  let out: IndexedEvent[] = [];
  let cursor: number | undefined;
  for (let page = 0; page < MAX_SPINE_PAGES; page++) {
    const p = await list({
      afterIdx: cursor,
      limit: SPINE_PAGE,
      kinds: SPINE_KINDS,
      toolNames: SPINE_TOOL_NAMES,
    });
    out = out.length === 0 ? p.events : mergeIndexed(out, p.events);
    // A short page is the last page. A cursor that does not advance would
    // loop forever — stop instead.
    if (p.count < SPINE_PAGE) break;
    if (cursor !== undefined && p.next <= cursor) break;
    cursor = p.next;
  }
  return out;
}

/** One transcript window, in full fidelity, snapped to a turn boundary. */
export interface WindowResult {
  events: IndexedEvent[];
  /** The oldest idx this window covers in FULL fidelity (every kind). */
  floor: number | null;
  /** The read reached the start of the log — nothing older exists. */
  exhausted: boolean;
}

/**
 * Take a backward page and extend it DOWN to the `run_started` at or before
 * its oldest event, so the window opens on a whole turn. The fill is read
 * forward from the run boundary (the exact missing span), not as another
 * backward page, so it costs only what the split turn actually needs.
 */
export async function snapWindow(
  list: ListEventsPage,
  page: RawPage,
  runStarts: readonly number[],
): Promise<WindowResult> {
  // A page shorter than the limit means the server had nothing older: the
  // page already starts at the log's start, so there is nothing to snap to.
  const exhausted = page.count < WINDOW;
  const oldest = page.events[0]?.idx;
  if (oldest === undefined) return { events: page.events, floor: null, exhausted: true };

  const snap = exhausted ? null : snapTo(runStarts, oldest);
  if (snap === null || snap >= oldest) {
    return { events: page.events, floor: oldest, exhausted };
  }

  const fill: IndexedEvent[] = [];
  let cursor = snap - 1;
  let read = 0;
  while (cursor < oldest - 1 && read < MAX_SNAP_FILL) {
    const remaining = oldest - 1 - cursor;
    const p = await list({
      afterIdx: cursor < 0 ? undefined : cursor,
      limit: Math.min(remaining, WINDOW),
    });
    if (p.count === 0) break;
    read += p.count;
    for (const e of p.events) if (e.idx < oldest) fill.push(e);
    if (p.next <= cursor) break; // no forward progress — stop rather than spin
    cursor = p.next;
  }
  // The fill only reaches the boundary when it actually got there; a run
  // longer than the cap keeps its true (unsnapped) floor.
  const floor = cursor >= oldest - 1 ? snap : (fill[0]?.idx ?? oldest);
  return { events: mergeIndexed(fill, page.events), floor, exhausted };
}

/** Read one window that ends just before `beforeIdx` (the backfill step). */
export async function loadWindow(
  list: ListEventsPage,
  beforeIdx: bigint,
  runStarts: readonly number[],
): Promise<WindowResult> {
  const page = await list({ beforeIdx, limit: WINDOW });
  return snapWindow(list, page, runStarts);
}

/** The transcript as the hook holds it after the open sequence. */
export interface Transcript {
  /** Spine + tail window, idx-ordered and idx-deduped: ONE array. */
  events: IndexedEvent[];
  /** Oldest idx covered in full fidelity — the backfill cursor. */
  floor: number | null;
  /** Older transcript exists below the floor. */
  hasMore: boolean;
  /** Turn boundaries, for snapping every later window. */
  runStarts: number[];
  /** The lowest idx the log holds — the floor cannot go below it. */
  logStart: number;
  /** `since` for the SSE subscribe: the highest idx already held. */
  since: number;
}

/**
 * The session-open sequence: the spine and the tail window are read in
 * PARALLEL (the tail is the page the reader sees first), then the tail is
 * snapped down to its run boundary using the spine's turn boundaries, and the
 * two are merged into one idx-ordered array.
 */
export async function loadTranscript(list: ListEventsPage): Promise<Transcript> {
  const [spine, tail] = await Promise.all([
    loadSpine(list),
    list({ beforeIdx: TAIL_BEFORE_IDX, limit: WINDOW }),
  ]);
  const runStarts = runStartIdxs(spine);
  const win = await snapWindow(list, tail, runStarts);
  const events = mergeIndexed(spine, win.events);
  const logStart = events[0]?.idx ?? 0;
  const floor = win.floor;
  return {
    events,
    floor,
    hasMore: !win.exhausted && floor != null && floor > logStart,
    runStarts,
    logStart,
    since: events.length > 0 ? events[events.length - 1]!.idx : -1,
  };
}
