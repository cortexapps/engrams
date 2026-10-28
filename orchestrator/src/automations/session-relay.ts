/** The session relay (ADR 0119 contract 5): a session mirrored into a place
 * (a Slack thread), bound to the SESSION, not to a run.
 *
 * Until contract 4 the relay was a message handler installed in a run: the
 * run had to stay alive for as long as the conversation, which is why the
 * Slack built-in looped `wait_event → send_prompt` for hours. Now a run
 * installs the relay (one `automation_session_relay` row) and ends; the
 * automation consumer renders every later curated event of the session
 * straight into the place, with no run live, and the session's end removes
 * the row. A later run re-points the relay at a new mention (⏳/✅ land on
 * the message that started that turn, a fresh bubble opens) and ends again.
 *
 * The rendering itself is unchanged: the Slack `CommunicationPolicy`
 * (`integrations/slack-policy.ts`) and the pure `routeSessionEvent`
 * (`workflows/communication-policy.ts`). The render state (open bubble,
 * posted questions, assets, last text) lives in the row and every write
 * CASes on its version, so a re-point racing a render never loses state.
 *
 * Delivery failures never throw out of here: a Slack outage drops the
 * message and keeps the conversation alive (the retired loop's posture).
 */

import type { CuratedEvent, TerminalOutcome } from "../control-plane/session-events.ts";
import { makeSlackPolicy } from "../integrations/slack-policy.ts";
import { sessionWebUrl as defaultSessionWebUrl } from "../links.ts";
import { log as rootLog } from "../log.ts";
import { tools as defaultTools } from "../tools/registry.ts";
import {
  routeSessionEvent,
  summarizeAsset,
  type AssetSummary,
  type ClosingSummary,
  type CommunicationPolicy,
  type QuestionProtocol,
  type SourceAnswer,
  type SourceMention,
  type StartedSession,
} from "../workflows/communication-policy.ts";
import { makeSessionRelayStore, type SessionRelayRow, type SessionRelayStore } from "../db/session-relays.ts";

const log = rootLog.child({ component: "session-relay" });

/** Signal name the interactivity route sends a `slack_choice` click as. */
export const SLACK_ANSWER_SIGNAL = "slack_answer";

/** Every slack_choice card's answer id starts with this: such a click is
 * the run's (the choice block waits for it), never a session question. */
export const CHOICE_ID_PREFIX = "choice:";

/** Cap an assistant bubble's accumulated text (the retired workflow's cap). */
export const MAX_BUBBLE_CHARS = 8000;

const ANSWER_FAIL_MSG = "I couldn't record that answer — the session may have ended.";
const LEGACY_QUESTION_MSG = "This question predates an upgrade and can no longer be answered.";
const CAS_ATTEMPTS = 3;

export const SLACK_RELAY_PROVIDER = "slack";

/** The render state held in `automation_session_relay.state`: plain JSON. */
export interface RelayRenderState extends Record<string, unknown> {
  questionTs: Record<string, string>;
  questionProtocols: Record<string, QuestionProtocol>;
  assets: AssetSummary[];
  bubble: { ts: string; text: string } | null;
  lastAssistantText: string | null;
  /** The session is on a turn (⏳ sits on the mention). */
  working: boolean;
  /** Mentions a re-point left mid-turn, still wearing ⏳: the next idle
   * settles them (✅) along with the current mention. */
  settle: string[];
  /** The log index of the last event rendered (a decimal string: the
   * state is JSON). The listener commits its cursor after the render, so
   * a crash between the two replays the event; the index drops the replay
   * instead of rendering it twice. */
  lastIdx: string | null;
}

export function freshRelayState(): RelayRenderState {
  return {
    questionTs: {},
    questionProtocols: {},
    assets: [],
    bubble: null,
    lastAssistantText: null,
    working: false,
    settle: [],
    lastIdx: null,
  };
}

function stateOf(row: SessionRelayRow): RelayRenderState {
  const s = row.state as Partial<RelayRenderState>;
  return {
    questionTs: { ...(s.questionTs ?? {}) },
    questionProtocols: { ...(s.questionProtocols ?? {}) },
    assets: [...(s.assets ?? [])],
    bubble: s.bubble ?? null,
    lastAssistantText: s.lastAssistantText ?? null,
    working: s.working ?? false,
    lastIdx: s.lastIdx ?? null,
    settle: [...(s.settle ?? [])],
  };
}

function mentionOf(row: SessionRelayRow): SourceMention {
  const t = row.target as Partial<SourceMention>;
  return {
    team: t.team ?? "",
    channel: t.channel ?? "",
    threadRoot: t.threadRoot ?? "",
    user: t.user ?? "",
    ts: t.ts ?? t.threadRoot ?? "",
    eventId: t.eventId ?? "",
  };
}

// ---------------------------------------------------------------------------
// Deps seam (production wiring vs tests)
// ---------------------------------------------------------------------------

export interface SessionRelayDeps {
  store: SessionRelayStore;
  /** The policy for one session; `sessionId` rides every Block Kit route so
   * an answer comes back to the session (see slack-interactivity). */
  policy(sessionId: string): CommunicationPolicy;
  completeToolCall(sessionId: string, toolCallId: string, result: unknown): Promise<void>;
  sessionWebUrl(sessionId: string): string;
}

let runtimeDeps: SessionRelayDeps | null = null;

export function setSessionRelayDeps(deps: SessionRelayDeps | null): void {
  runtimeDeps = deps;
}

export function sessionRelayDeps(): SessionRelayDeps {
  if (runtimeDeps) return runtimeDeps;
  runtimeDeps = {
    store: makeSessionRelayStore(),
    policy: (sessionId) => makeSlackPolicy({ routeExtras: { sessionId } }),
    completeToolCall: (sessionId, toolCallId, result) =>
      defaultTools.complete(sessionId, toolCallId, result),
    sessionWebUrl: defaultSessionWebUrl,
  };
  return runtimeDeps;
}

// ---------------------------------------------------------------------------
// Install / re-point (called from a run's block)
// ---------------------------------------------------------------------------

export interface InstallSessionRelayInput {
  sessionId: string;
  automationId: string;
  target: SourceMention;
}

/** Install the relay on a session, or re-point an installed one at a new
 * mention. A re-point keeps the questions, assets and last text (the
 * conversation continues) and closes the open bubble: the next response
 * opens a fresh message under the new mention. A mention left mid-turn
 * keeps its ⏳ until the session next goes idle, which settles it.
 * Idempotent per mention. */
export async function installSessionRelay(
  input: InstallSessionRelayInput,
  deps: Pick<SessionRelayDeps, "store"> = sessionRelayDeps(),
): Promise<{ created: boolean; previous?: SourceMention }> {
  const target: Record<string, unknown> = { ...input.target };
  const installed = await deps.store.install({
    sessionId: input.sessionId,
    automationId: input.automationId,
    provider: SLACK_RELAY_PROVIDER,
    target,
    state: freshRelayState(),
  });
  if (installed.created) return { created: true };
  let row: SessionRelayRow | null = installed.row;
  for (let attempt = 0; row !== null && attempt < CAS_ATTEMPTS; attempt += 1) {
    const state = stateOf(row);
    // Same mention: a re-executed step (crash before checkpoint). Nothing
    // to change, and the open bubble must stay open.
    if (mentionOf(row).ts === input.target.ts && mentionOf(row).eventId === input.target.eventId) {
      return { created: false };
    }
    const ok = await deps.store.update(row.sessionId, row.version, {
      target,
      state: {
        ...state,
        bubble: null,
        working: false,
        settle: state.working ? [...state.settle, mentionOf(row).ts] : state.settle,
      },
    });
    if (ok) return { created: false, previous: mentionOf(row) };
    row = await deps.store.get(input.sessionId);
  }
  if (row === null) {
    // Removed under us (the session ended): install fresh, the caller's
    // intent holds.
    await deps.store.install({
      sessionId: input.sessionId,
      automationId: input.automationId,
      provider: SLACK_RELAY_PROVIDER,
      target,
      state: freshRelayState(),
    });
    return { created: true };
  }
  throw new Error(`session relay "${input.sessionId}": re-point lost ${CAS_ATTEMPTS} CAS races`);
}

/** Remove a session's relay. True when one existed. */
export async function removeSessionRelay(
  sessionId: string,
  deps: Pick<SessionRelayDeps, "store"> = sessionRelayDeps(),
): Promise<boolean> {
  return deps.store.remove(sessionId);
}

// ---------------------------------------------------------------------------
// Rendering (called from the automation consumer, no run live)
// ---------------------------------------------------------------------------

/** Render one curated session event into the relayed place and persist the
 * next render state. "no_relay" when the session has none. Never throws on
 * a render failure: the event is dropped and logged. */
export async function renderSessionEvent(
  sessionId: string,
  event: CuratedEvent,
  deps: SessionRelayDeps = sessionRelayDeps(),
): Promise<"rendered" | "no_relay"> {
  let row = await deps.store.get(sessionId);
  if (row === null) return "no_relay";
  const pol = deps.policy(sessionId);
  const session: StartedSession = { id: sessionId, webUrl: deps.sessionWebUrl(sessionId) };
  const mention = mentionOf(row);
  const before = stateOf(row);
  if (before.lastIdx !== null && event.idx <= BigInt(before.lastIdx)) return "rendered";
  let st = stateOf(row);
  st.lastIdx = String(event.idx);
  try {
    await dispatch(pol, mention, st, event, session);
  } catch (err) {
    log.error({ sessionId, kind: event.kind, err }, "session relay: failed to render a session event; dropping it");
  }
  // Whatever dispatch mutated before a throw is the truth of the thread (a
  // bubble that was posted exists), so the state is saved either way.
  for (let attempt = 0; attempt < CAS_ATTEMPTS; attempt += 1) {
    if (await deps.store.update(sessionId, row.version, { state: st })) return "rendered";
    row = await deps.store.get(sessionId);
    if (row === null) return "rendered";
    // Another writer won (a re-point, a render of a sibling event): what
    // this render changed goes on top of the row as it is now, never over
    // it — a re-point's closed bubble stays closed, a sibling's question
    // card stays known.
    st = mergeRenderState(stateOf(row), before, st, mention, mentionOf(row).ts === mention.ts);
  }
  log.warn({ sessionId }, "session relay: render state write lost every CAS race; the next event re-reads");
  return "rendered";
}

/** What one render changed (`before` → `after`), applied to the state
 * `base` another writer saved meanwhile. Questions and assets are added
 * (never removed) by a render, so the additions join. The bubble and the
 * last text are this render's only while the row still points at the
 * mention it rendered under; after a re-point the bubble is closed and
 * stays so. */
function mergeRenderState(
  base: RelayRenderState,
  before: RelayRenderState,
  after: RelayRenderState,
  mention: SourceMention,
  sameMention: boolean,
): RelayRenderState {
  const addedAssets = after.assets.slice(before.assets.length);
  // A turn that began under a mention the row no longer points at wears
  // its ⏳ there: the next idle must settle it.
  const settle =
    !sameMention && after.working && !before.working ? [...base.settle, mention.ts] : base.settle;
  return {
    questionTs: { ...base.questionTs, ...after.questionTs },
    questionProtocols: { ...base.questionProtocols, ...after.questionProtocols },
    assets: [...base.assets, ...addedAssets],
    bubble: sameMention ? after.bubble : base.bubble,
    lastAssistantText:
      after.lastAssistantText !== before.lastAssistantText ? after.lastAssistantText : base.lastAssistantText,
    working: sameMention ? after.working : base.working,
    settle: sameMention ? after.settle : settle,
    lastIdx:
      base.lastIdx === null || (after.lastIdx !== null && BigInt(after.lastIdx) > BigInt(base.lastIdx))
        ? after.lastIdx
        : base.lastIdx,
  };
}

/** The session ended: post the closing message and remove the relay. A
 * NEUTRAL end (the sandbox reclaimed — a kept thread session swept after
 * days of quiet) posts nothing: to the people in the thread nothing
 * happened, and the next mention starts a fresh session there. */
export async function closeSessionRelay(
  sessionId: string,
  outcome: TerminalOutcome,
  deps: SessionRelayDeps = sessionRelayDeps(),
): Promise<"closed" | "no_relay"> {
  const row = await deps.store.get(sessionId);
  if (row === null) return "no_relay";
  const pol = deps.policy(sessionId);
  const facts = relayFactsOf(row, deps);
  try {
    await settleMentions(pol, facts.mention, stateOf(row));
    if (outcome === "completed") {
      await pol.onComplete(facts.mention, facts.session, facts.summary);
    } else if (outcome === "failed") {
      await pol.onFail(facts.mention, "The session hit an error and the thread cannot continue.");
    }
  } catch (err) {
    log.error({ sessionId, outcome, err }, "session relay: failed to post the closing message");
  }
  await deps.store.remove(sessionId);
  return "closed";
}

/** A human answered a session's question card (the interactivity route).
 * The answer completes the deferred tool call; the session's own
 * `tool_result_submitted` event then updates the card through `render`. */
export async function deliverSessionAnswer(
  sessionId: string,
  answer: SourceAnswer,
  deps: SessionRelayDeps = sessionRelayDeps(),
): Promise<"delivered" | "legacy" | "failed" | "no_relay"> {
  const row = await deps.store.get(sessionId);
  if (row === null) return "no_relay";
  const pol = deps.policy(sessionId);
  const mention = mentionOf(row);
  const via = stateOf(row).questionProtocols[answer.toolCallId] ?? "legacy";
  if (via === "legacy") {
    await pol.onDeliveryError(mention, LEGACY_QUESTION_MSG).catch(() => {});
    return "legacy";
  }
  try {
    await deps.completeToolCall(sessionId, answer.toolCallId, answer.answers);
    return "delivered";
  } catch (err) {
    log.error({ sessionId, err }, "session relay: failed to enqueue answer — keeping the thread alive");
    await pol.onDeliveryError(mention, ANSWER_FAIL_MSG).catch(() => {});
    return "failed";
  }
}

export interface SessionRelayFacts {
  mention: SourceMention;
  session: StartedSession;
  summary: ClosingSummary;
}

/** What a closing message needs: the thread route, the session, and the
 * summary of what it said and produced. */
export function relayFactsOf(row: SessionRelayRow, deps: Pick<SessionRelayDeps, "sessionWebUrl">): SessionRelayFacts {
  const st = stateOf(row);
  return {
    mention: mentionOf(row),
    session: { id: row.sessionId, webUrl: deps.sessionWebUrl(row.sessionId) },
    summary: { lastMessage: st.lastAssistantText, assets: [...st.assets] },
  };
}

/** Settle the mentions a re-point left mid-turn: each gets its ✅. */
async function settleMentions(pol: CommunicationPolicy, m: SourceMention, st: RelayRenderState): Promise<void> {
  for (const ts of st.settle) await pol.onIdle({ ...m, ts });
  st.settle = [];
}

/** The legacy `dispatchSessionEvent`, verbatim in effect. */
async function dispatch(
  pol: CommunicationPolicy,
  m: SourceMention,
  st: RelayRenderState,
  event: CuratedEvent,
  session: StartedSession,
): Promise<void> {
  const effect = routeSessionEvent(event, new Map(Object.entries(st.questionProtocols)));
  switch (effect.kind) {
    case "message": {
      if (!effect.text) break;
      const append =
        st.bubble !== null && st.bubble.text.length + effect.text.length + 2 <= MAX_BUBBLE_CHARS;
      const text = append ? `${st.bubble!.text}\n\n${effect.text}` : effect.text;
      const ref = append ? st.bubble!.ts : undefined;
      const ts = await pol.onAssistantMessage(m, text, ref);
      st.bubble = { ts, text };
      st.lastAssistantText = effect.text;
      break;
    }
    case "working":
      st.bubble = null;
      st.working = true;
      await pol.onWorking(m);
      break;
    case "idle":
      st.bubble = null;
      st.working = false;
      await settleMentions(pol, m, st);
      await pol.onIdle(m);
      break;
    case "question": {
      st.bubble = null;
      const ref = await pol.onUserQuestion(m, event);
      if (effect.toolCallId) {
        st.questionTs[effect.toolCallId] = ref;
        st.questionProtocols[effect.toolCallId] = effect.via;
      }
      break;
    }
    case "answered": {
      const ref = effect.toolCallId ? st.questionTs[effect.toolCallId] : undefined;
      await pol.onAnswered(m, event, ref);
      break;
    }
    case "asset": {
      st.bubble = null;
      const recap = summarizeAsset(event);
      if (recap) st.assets.push(recap);
      await pol.onAsset(m, event, session);
      break;
    }
    case "ignore":
      break;
  }
}
