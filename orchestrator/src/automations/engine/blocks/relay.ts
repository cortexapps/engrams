/** `relay_session` — mirror a session into a place (a Slack thread) as an
 * installed message handler (ADR 0119 phase 4.5, contract 3). A catalog
 * block since 2026-09: any automation may relay a session; Slack is the
 * first provider, and the config's `provider` enum is where the next one
 * lands.
 *
 * Ports the drain loop of `workflows/slack-thread.ts` (`dispatchSessionEvent`
 * + the `trigger_answer` arm of `handleInbound`) onto the engine. The
 * rendering itself is the unchanged Slack `CommunicationPolicy`
 * (`integrations/slack-policy.ts`) and the pure routing is the unchanged
 * `routeSessionEvent` (`workflows/communication-policy.ts`) — only the loop
 * that drives them moved. Per-turn mention ownership (⏳/✅ on the message
 * that started the turn, a fresh bubble per turn) is driven by the built-in
 * graph re-executing this block's sibling `send_prompt`; the relay tracks
 * the current mention from the run's trigger facts.
 *
 * Execute installs: it marks the session relay-bound (so the automation
 * consumer forwards CURATED events, not just idle/terminal), flips the
 * session's binding, and returns the initial render state as its
 * `handler_state` output. Handler calls run inside their own checkpointed
 * steps (`step:<path>.__relay__:<n>`), and EVERY call returns the next
 * render state as its step output (`HandlerResult.state`). The interpreter
 * hands the previous step's recorded state back in as `ctx.handlerState`.
 * This module keeps NO per-run memory: a checkpointed step's closure does
 * not re-run on DBOS recovery — only its recorded output is replayed — so a
 * process-local map would be empty after a pod restart and the relay would
 * silently stop rendering. Threading the state through the outputs is what
 * makes a restarted pod continue the bubble / answer the question exactly
 * where the old one stopped.
 *
 * Delivery failures never fail the run — the legacy loop's "log and drop it,
 * keep the thread alive" posture (slack-thread.ts handleInbound).
 */

import { z } from "zod";

import { log as rootLog } from "../../../log.ts";
import { makeSlackPolicy } from "../../../integrations/slack-policy.ts";
import {
  routeSessionEvent,
  summarizeAsset,
  type AssetSummary,
  type ClosingSummary,
  type CommunicationPolicy,
  type QuestionProtocol,
  type StartedSession,
} from "../../../workflows/communication-policy.ts";
import type { SourceMention } from "../../../workflows/thread-inbox.ts";
import { config as appConfig } from "../../../config.ts";
import { tools as defaultTools } from "../../../tools/registry.ts";
import { sessionRefSchema } from "../definition.ts";
import type { RunContext } from "../context.ts";
import type { AutomationInbox } from "../inbox.ts";
import { registerBlock, type BlockOutcome, type HandlerResult } from "./registry.ts";

const log = rootLog.child({ component: "slack-relay" });

/** Signal name the interactivity route sends an AskUserQuestion answer as. */
export const SLACK_ANSWER_SIGNAL = "slack_answer";

const ANSWER_FAIL_MSG = "I couldn't record that answer — the session may have ended.";
const LEGACY_QUESTION_MSG = "This question predates an upgrade and can no longer be answered.";
/** Cap an assistant bubble's accumulated text (slack-thread.ts MAX_BUBBLE_CHARS). */
export const MAX_BUBBLE_CHARS = 8000;

// ---------------------------------------------------------------------------
// Deps seam (production wiring vs tests)
// ---------------------------------------------------------------------------

export interface SlackRelayDeps {
  /** Build the Slack policy for one run; `runId` is stamped into every Block
   * Kit route so answers come back to this run (see slack-interactivity). */
  policy(runId: string): CommunicationPolicy;
  completeToolCall(sessionId: string, toolCallId: string, result: unknown): Promise<void>;
  sessionWebUrl(sessionId: string): string;
}

let runtimeDeps: SlackRelayDeps | null = null;

export function setSlackRelayDeps(deps: SlackRelayDeps | null): void {
  runtimeDeps = deps;
}

function deps(): SlackRelayDeps {
  if (runtimeDeps) return runtimeDeps;
  runtimeDeps = {
    policy: (runId) => makeSlackPolicy({ routeExtras: { runId } }),
    completeToolCall: (sessionId, toolCallId, result) =>
      defaultTools.complete(sessionId, toolCallId, result),
    sessionWebUrl: (sessionId) => `${appConfig.baseUrl}/sessions/${sessionId}`,
  };
  return runtimeDeps;
}

// ---------------------------------------------------------------------------
// Config + render state
// ---------------------------------------------------------------------------

export const RELAY_SESSION_TYPE = "relay_session";

export const slackRelayConfigSchema = z.object({
  session: sessionRefSchema,
  /** The place the session is mirrored into. Slack is the only provider so
   * far; the thread coordinates below are Slack's. */
  provider: z.enum(["slack"]).default("slack"),
  team: z.string().min(1),
  channel: z.string().min(1),
  /** The thread root ts (the mention that opened the thread). */
  threadTs: z.string().min(1),
  /** The mention ts the current turn reacts on; defaults to threadTs. */
  mentionTs: z.string().min(1).optional(),
  userId: z.string().min(1).optional(),
  eventId: z.string().min(1).optional(),
});
export type SlackRelayConfig = z.infer<typeof slackRelayConfigSchema>;

export const RELAY_STATE_KIND = "slack_relay";

/** The relay's render state (the legacy `ThreadRender`), as a checkpointed
 * step output: plain JSON only (Records, not Maps), tagged so the recap can
 * find it among the run's step outputs without knowing the relay's block id. */
export interface RelayState extends Record<string, unknown> {
  kind: typeof RELAY_STATE_KIND;
  /** Monotonic per change. Install and re-point blocks each record a state
   * on their own outputs, so "the latest" is the highest seq among the
   * run's step outputs, not a position in the graph. */
  seq: number;
  sessionId: string;
  mention: SourceMention;
  questionTs: Record<string, string>;
  questionProtocols: Record<string, QuestionProtocol>;
  assets: AssetSummary[];
  bubble: { ts: string; text: string } | null;
  lastAssistantText: string | null;
}

function isRelayState(v: unknown): v is RelayState {
  return (
    typeof v === "object" &&
    v !== null &&
    (v as { kind?: unknown }).kind === RELAY_STATE_KIND &&
    typeof (v as { sessionId?: unknown }).sessionId === "string"
  );
}

/** The threaded state the interpreter handed this call (see registry.ts
 * `onMessage`): the previous handler step's recorded output. */
function stateFrom(ctx: RunContext): RelayState | null {
  return isRelayState(ctx.handlerState) ? ctx.handlerState : null;
}

/** The latest relay state among the run's recorded step outputs — what the
 * interpreter mirrors onto the relay block's `handler_state` after every
 * handler step. Null when no relay was installed on this run. */
function stateFromSteps(ctx: RunContext): RelayState | null {
  let latest: RelayState | null = null;
  for (const outputs of Object.values(ctx.steps)) {
    const st = outputs["handler_state"];
    if (isRelayState(st) && (latest === null || st.seq > latest.seq)) latest = st;
  }
  return latest;
}

function mentionFrom(config: SlackRelayConfig): SourceMention {
  return {
    team: config.team,
    channel: config.channel,
    threadRoot: config.threadTs,
    user: config.userId ?? "",
    ts: config.mentionTs ?? config.threadTs,
    eventId: config.eventId ?? `${config.channel}:${config.threadTs}`,
  };
}

// ---------------------------------------------------------------------------
// The block
// ---------------------------------------------------------------------------

async function install(config: SlackRelayConfig, ctx: RunContext): Promise<BlockOutcome> {
  const sessionId = await ctx.resolveSession(config.session);
  await ctx.deps.sessions.setSessionRelay(sessionId, true);
  const state: RelayState = {
    kind: RELAY_STATE_KIND,
    seq: 0,
    sessionId,
    mention: mentionFrom(config),
    questionTs: {},
    questionProtocols: {},
    assets: [],
    bubble: null,
    lastAssistantText: null,
  };
  return {
    kind: "ok",
    outputs: { session_id: sessionId, installed: true, handler_state: state },
  };
}

/** A later turn re-points the relay: the built-in graph carries a second
 * relay block inside its loop body, templated on the accepted follow-up's
 * ts/user/event, so ⏳/✅ land on the message that started THAT turn and
 * its responses open a fresh bubble. Executing the relay's type while one
 * is installed is a re-point, not a second install: the carried state
 * (questions, assets, last message) continues; only the mention and the
 * bubble change. */
function repoint(config: SlackRelayConfig, prev: RelayState): BlockOutcome {
  const state: RelayState = {
    ...prev,
    seq: prev.seq + 1,
    mention: mentionFrom(config),
    bubble: null, // a new turn — the next response opens a fresh message
  };
  return {
    kind: "ok",
    outputs: { session_id: prev.sessionId, installed: true, handler_state: state },
  };
}

async function onMessage(
  msg: AutomationInbox,
  config: SlackRelayConfig,
  ctx: RunContext,
): Promise<HandlerResult> {
  const prev = stateFrom(ctx);
  if (!prev) return { verdict: "pass" };
  // Work on a copy: the previous state is a recorded step output and must
  // stay what that step recorded.
  const st: RelayState = {
    ...prev,
    seq: prev.seq + 1,
    questionTs: { ...prev.questionTs },
    questionProtocols: { ...prev.questionProtocols },
    assets: [...prev.assets],
  };
  const pol = deps().policy(ctx.runId);
  const session: StartedSession = {
    id: st.sessionId,
    webUrl: deps().sessionWebUrl(st.sessionId),
  };

  if (msg.kind === "session_event") {
    if (msg.sessionId !== st.sessionId) return { verdict: "pass" };
    try {
      await dispatch(pol, st, msg.event, session);
    } catch (err) {
      log.error(
        { runId: ctx.runId, sessionId: st.sessionId, kind: msg.event.kind, err },
        "slack relay: failed to render a session event; dropping it",
      );
    }
    // Whatever dispatch mutated before a throw is the truth of the thread
    // (a bubble that was posted exists), so the state is returned either way.
    return { verdict: "consumed", state: st };
  }

  if (msg.kind === "signal" && msg.name === SLACK_ANSWER_SIGNAL) {
    const answer = parseAnswer(msg.payload);
    if (!answer) return { verdict: "consumed" };
    const via = st.questionProtocols[answer.toolCallId] ?? "legacy";
    if (via === "legacy") {
      await pol.onDeliveryError(st.mention, LEGACY_QUESTION_MSG).catch(() => {});
      return { verdict: "consumed" };
    }
    try {
      await deps().completeToolCall(st.sessionId, answer.toolCallId, answer.answers);
    } catch (err) {
      log.error(
        { runId: ctx.runId, sessionId: st.sessionId, err },
        "slack relay: failed to enqueue answer — keeping the thread alive",
      );
      await pol.onDeliveryError(st.mention, ANSWER_FAIL_MSG).catch(() => {});
    }
    return { verdict: "consumed" };
  }

  // Idle/ended/other signals/events belong to the graph's waits.
  void config;
  return { verdict: "pass" };
}

function parseAnswer(
  payload: Record<string, unknown> | undefined,
): { toolCallId: string; answers: Record<string, string[]> } | null {
  if (!payload) return null;
  const toolCallId = payload["toolCallId"];
  const answers = payload["answers"];
  if (typeof toolCallId !== "string" || toolCallId === "") return null;
  if (typeof answers !== "object" || answers === null || Array.isArray(answers)) return null;
  return { toolCallId, answers: answers as Record<string, string[]> };
}

/** The legacy `dispatchSessionEvent`, verbatim in effect. */
async function dispatch(
  pol: CommunicationPolicy,
  st: RelayState,
  event: Parameters<typeof routeSessionEvent>[0],
  session: StartedSession,
): Promise<void> {
  const m = st.mention;
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
      await pol.onWorking(m);
      break;
    case "idle":
      st.bubble = null;
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

/** The closing recap the Slack built-in's finalize hook posts, read from the
 * run's recorded step outputs (never process memory). */
export function slackRelayClosingSummary(ctx: RunContext): ClosingSummary | null {
  const st = stateFromSteps(ctx);
  return st ? { lastMessage: st.lastAssistantText, assets: [...st.assets] } : null;
}

/** Everything the recap block needs to render a terminal message the way the
 * legacy loop did: the thread route, the session, and the summary. Null when
 * no relay was installed on this run (the run ended before the relay block).
 * Read from `ctx.steps`, so a recap after a pod restart sees the state the
 * last handler step recorded. */
export function slackRelayFinalFacts(ctx: RunContext): {
  mention: SourceMention;
  session: StartedSession;
  summary: ClosingSummary;
} | null {
  const st = stateFromSteps(ctx);
  if (!st) return null;
  return {
    mention: st.mention,
    session: { id: st.sessionId, webUrl: deps().sessionWebUrl(st.sessionId) },
    summary: { lastMessage: st.lastAssistantText, assets: [...st.assets] },
  };
}

/** Production policy access for the sibling relay_close block. */
export function slackRelayPolicy(runId: string): CommunicationPolicy {
  return deps().policy(runId);
}

export function registerSlackRelayBlock(): void {
  registerBlock<SlackRelayConfig>({
    type: RELAY_SESSION_TYPE,
    refusesDryRun: true,
    outputs: ["session_id", "installed", "handler_state"],
    configSchema: slackRelayConfigSchema,
    async execute(config, ctx) {
      // An installed relay re-executed is a re-point: the interpreter hands
      // the threaded state in as `ctx.handlerState`.
      const prev = stateFrom(ctx);
      return prev ? repoint(config, prev) : install(config, ctx);
    },
    onMessage,
  });
}
