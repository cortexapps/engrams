/** `system.slack_thread_relay` — the Slack thread ↔ session relay as an
 * installed message handler (ADR 0119 phase 4.5, contract 3).
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
 * session's binding, and records the render state it will thread through
 * every later handler call. Handler calls run inside their own checkpointed
 * steps (`step:<path>.__relay__:<n>`); render state lives in this module's
 * per-run map, rebuilt deterministically on replay because every mutation
 * happens inside a step whose outcome is checkpointed.
 *
 * Delivery failures never fail the run — the legacy loop's "log and drop it,
 * keep the thread alive" posture (slack-thread.ts handleInbound).
 */

import { z } from "zod";

import { log as rootLog } from "../../../../log.ts";
import { makeSlackPolicy } from "../../../../integrations/slack-policy.ts";
import {
  routeSessionEvent,
  summarizeAsset,
  type AssetSummary,
  type CommunicationPolicy,
  type QuestionProtocol,
  type StartedSession,
} from "../../../../workflows/communication-policy.ts";
import type { SourceMention } from "../../../../workflows/thread-inbox.ts";
import { config as appConfig } from "../../../../config.ts";
import { tools as defaultTools } from "../../../../tools/registry.ts";
import { sessionRefSchema } from "../../definition.ts";
import type { RunContext } from "../../context.ts";
import type { AutomationInbox } from "../../inbox.ts";
import { registerBlock, type BlockOutcome } from "../registry.ts";

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

export const slackRelayConfigSchema = z.object({
  session: sessionRefSchema,
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

/** Per-run render state (the legacy `ThreadRender`). Keyed by run id; every
 * mutation happens inside a checkpointed handler step, so replay rebuilds it
 * identically from the recv sequence. */
interface RelayState {
  sessionId: string;
  mention: SourceMention;
  questionTs: Map<string, string>;
  questionProtocols: Map<string, QuestionProtocol>;
  assets: AssetSummary[];
  bubble: { ts: string; text: string } | null;
  lastAssistantText: string | null;
}

const states = new Map<string, RelayState>();

/** Test hook: the module-level state map is per process. */
export function resetSlackRelayStateForTest(): void {
  states.clear();
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
  states.set(ctx.runId, {
    sessionId,
    mention: mentionFrom(config),
    questionTs: new Map(),
    questionProtocols: new Map(),
    assets: [],
    bubble: null,
    lastAssistantText: null,
  });
  return { kind: "ok", outputs: { session_id: sessionId, installed: true } };
}

/** A later `send_prompt` for a new mention re-points the turn: the built-in
 * graph calls this block again with the new mention ts (structure-locked,
 * but `mentionTs`/`eventId` are tunable at runtime via `$ref`). Re-executing
 * an installed relay is a re-point, not a second install. */
function repoint(config: SlackRelayConfig, ctx: RunContext): void {
  const st = states.get(ctx.runId);
  if (!st) return;
  st.mention = mentionFrom(config);
  st.bubble = null; // a new turn — the next response opens a fresh message
}

async function onMessage(
  msg: AutomationInbox,
  config: SlackRelayConfig,
  ctx: RunContext,
): Promise<"consumed" | "pass"> {
  const st = states.get(ctx.runId);
  if (!st) return "pass";
  const pol = deps().policy(ctx.runId);
  const session: StartedSession = {
    id: st.sessionId,
    webUrl: deps().sessionWebUrl(st.sessionId),
  };

  if (msg.kind === "session_event") {
    if (msg.sessionId !== st.sessionId) return "pass";
    try {
      await dispatch(pol, st, msg.event, session);
    } catch (err) {
      log.error(
        { runId: ctx.runId, sessionId: st.sessionId, kind: msg.event.kind, err },
        "slack relay: failed to render a session event; dropping it",
      );
    }
    return "consumed";
  }

  if (msg.kind === "signal" && msg.name === SLACK_ANSWER_SIGNAL) {
    const answer = parseAnswer(msg.payload);
    if (!answer) return "consumed";
    const via = st.questionProtocols.get(answer.toolCallId) ?? "legacy";
    if (via === "legacy") {
      await pol.onDeliveryError(st.mention, LEGACY_QUESTION_MSG).catch(() => {});
      return "consumed";
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
    return "consumed";
  }

  // Idle/ended/other signals/events belong to the graph's waits.
  void config;
  return "pass";
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
  const effect = routeSessionEvent(event, st.questionProtocols);
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
        st.questionTs.set(effect.toolCallId, ref);
        st.questionProtocols.set(effect.toolCallId, effect.via);
      }
      break;
    }
    case "answered": {
      const ref = effect.toolCallId ? st.questionTs.get(effect.toolCallId) : undefined;
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

/** The closing recap the Slack built-in's finalize hook posts. */
export function slackRelayClosingSummary(
  runId: string,
): { lastMessage: string | null; assets: AssetSummary[] } | null {
  const st = states.get(runId);
  return st ? { lastMessage: st.lastAssistantText, assets: [...st.assets] } : null;
}

export function registerSlackRelayBlock(): void {
  registerBlock<SlackRelayConfig>({
    type: "system.slack_thread_relay",
    system: true,
    outputs: ["session_id", "installed"],
    configSchema: slackRelayConfigSchema,
    async execute(config, ctx) {
      if (states.has(ctx.runId)) {
        repoint(config, ctx);
        return { kind: "ok", outputs: { session_id: states.get(ctx.runId)!.sessionId, installed: true } };
      }
      return install(config, ctx);
    },
    onMessage,
  });
}
