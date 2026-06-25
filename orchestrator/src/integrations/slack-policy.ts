/**
 * Slack CommunicationPolicy (ADR 0059 P2.10) — the v1 provider-mechanics impl
 * behind the SlackThreadWorkflow's policy seam. Pure Block Kit shaping lives in
 * slack-blocks.ts; this module is the thin layer that drives the Slack WebClient
 * (reactions, posts, updates, thread reads). Every method is invoked by the
 * framework as a checkpointed DBOS step, so it runs once per effect.
 *
 * The Slack client is injected (default = the authed WebClient from slack.ts),
 * which keeps the policy — including the thread→prompt fold — unit-testable
 * without a network or the engine.
 */

import type { KnownBlock } from "@slack/types";

import { log as rootLog } from "../log.ts";
import { getSlackClient } from "./slack.ts";
import {
  parseUserQuestion,
  buildQuestionBlocks,
  buildAnsweredBlocks,
  buildAssetLine,
  buildClosingBlocks,
  buildMessageBlocks,
} from "./slack-blocks.ts";
import { summarizeAsset, type CommunicationPolicy, type StartedSession } from "../workflows/communication-policy.ts";
import type { SourceMention } from "../workflows/thread-inbox.ts";

const log = rootLog.child({ component: "slack" });

/** The flavor appended to a triggered agent's system prompt (ADR 0059 Decision
 *  8) — NOT connector config; a constant this policy provides at session
 *  create so the agent behaves well in a chat thread. */
const SYSTEM_PROMPT_APPEND =
  "You are running inside an engrams session triggered from a Slack thread. " +
  "Keep replies concise and chat-friendly. When you need a decision or " +
  "clarification, ask via AskUserQuestion — it renders as interactive buttons " +
  "in Slack — rather than guessing. The user cannot see your terminal, so " +
  "surface results, links, and artifacts explicitly.";

/** A reply in a Slack thread (the subset the prompt fold reads). */
export interface SlackReply {
  ts?: string;
  user?: string;
  bot_id?: string;
  text?: string;
}

/** The Slack WebClient surface this policy uses — a structural subset so a fake
 *  satisfies it in tests (the real WebClient does too). */
export interface SlackPolicyClient {
  reactions: {
    add(args: { channel: string; timestamp: string; name: string }): Promise<unknown>;
    remove(args: { channel: string; timestamp: string; name: string }): Promise<unknown>;
  };
  chat: {
    postMessage(args: {
      channel: string;
      thread_ts?: string;
      text?: string;
      blocks?: KnownBlock[];
    }): Promise<{ ts?: string }>;
    update(args: { channel: string; ts: string; text?: string; blocks?: KnownBlock[] }): Promise<unknown>;
  };
  conversations: {
    replies(args: {
      channel: string;
      ts: string;
      oldest?: string;
      limit?: number;
    }): Promise<{ messages?: SlackReply[] }>;
  };
}

export interface SlackPolicyDeps {
  client?: () => Promise<SlackPolicyClient>;
  /** The bot's own user id, so its `<@bot>` mention is stripped from prompts.
   *  Optional: when unset, all `<@…>` mentions are stripped. */
  botUserId?: string;
}

/**
 * Fold a page of thread replies into the session prompt + the new cursor. Pure.
 * The cursor (`maxTs`) advances past EVERY reply seen — including the bot's own
 * — so the next gather never re-reads; only HUMAN message text feeds the prompt
 * (the agent must never be prompted with its own posts). `since` is exclusive:
 * `conversations.replies(oldest=)` is inclusive, so the boundary message is
 * dropped here.
 *
 * Shape: the triggering @mention (identified by `triggerTs`) is the directive
 * and goes at the BOTTOM; every other human message is prior thread context,
 * wrapped in `<thread context>…</thread context>` above it. With no other
 * messages the prompt is just the directive (no wrapper). If `triggerTs` matches
 * nothing in this page (the mention wasn't returned), fall back to a plain join.
 */
export function foldReplies(
  messages: SlackReply[],
  since: string | null,
  botUserId?: string,
  triggerTs?: string,
): { prompt: string; maxTs: string } {
  let maxTs = since ?? "0";
  const human: { ts: string; text: string }[] = [];
  for (const msg of messages) {
    const ts = msg.ts ?? "";
    if (since && num(ts) <= num(since)) continue; // already delivered
    if (num(ts) > num(maxTs)) maxTs = ts; // advance past everything seen
    if (msg.bot_id || (botUserId && msg.user === botUserId)) continue; // never our own
    const text = stripMentions(msg.text ?? "", botUserId).trim();
    if (text) human.push({ ts, text });
  }

  const idx = triggerTs ? human.findIndex((h) => h.ts === triggerTs) : -1;
  if (idx === -1) {
    // No identified directive — emit the messages plainly, no wrapper.
    return { prompt: human.map((h) => h.text).join("\n\n"), maxTs };
  }
  const context = human.filter((_, i) => i !== idx).map((h) => h.text);
  const directive = human[idx].text;
  const prompt = context.length
    ? `<thread context>\n${context.join("\n")}\n</thread context>\n\n${directive}`
    : directive;
  return { prompt, maxTs };
}

const num = (ts: string): number => Number.parseFloat(ts) || 0;

/** Strip the bot's `<@id>` mention (or all mentions when the id is unknown). */
function stripMentions(text: string, botUserId?: string): string {
  const re = botUserId ? new RegExp(`<@${botUserId}(\\|[^>]*)?>`, "g") : /<@[^>]+>/g;
  return text.replace(re, " ").replace(/\s+/g, " ");
}

/** Build the production Slack CommunicationPolicy (client injectable for tests). */
export function makeSlackPolicy(deps: SlackPolicyDeps = {}): CommunicationPolicy {
  const getClient =
    deps.client ?? (async () => (await getSlackClient()) as unknown as SlackPolicyClient);
  const botUserId = deps.botUserId;

  const route = (m: SourceMention) => ({
    team: m.team,
    channel: m.channel,
    threadRoot: m.threadRoot,
  });

  /** React on the mention; best-effort (a replayed/duplicate add must not wedge
   *  the step). */
  async function react(m: SourceMention, name: string): Promise<void> {
    try {
      const c = await getClient();
      await c.reactions.add({ channel: m.channel, timestamp: m.ts, name });
    } catch {
      /* already_reacted / missing_scope — UX-only, never block the workflow */
    }
  }

  /** Remove a reaction; best-effort (a missing/duplicate remove must not wedge
   *  the step). */
  async function unreact(m: SourceMention, name: string): Promise<void> {
    try {
      const c = await getClient();
      await c.reactions.remove({ channel: m.channel, timestamp: m.ts, name });
    } catch {
      /* no_reaction / missing_scope — UX-only */
    }
  }

  async function post(m: SourceMention, text: string, blocks?: KnownBlock[]): Promise<string> {
    const c = await getClient();
    const res = await c.chat.postMessage({
      channel: m.channel,
      thread_ts: m.threadRoot,
      text,
      ...(blocks ? { blocks } : {}),
    });
    return res.ts ?? "";
  }

  return {
    systemPromptAppend: SYSTEM_PROMPT_APPEND,

    onPickup: (m) => react(m, "eyes"),

    async onStarted(m, session) {
      log.info({ channel: m.channel, thread: m.threadRoot, sessionId: session.id }, "slack: session started");
      await post(m, `Started a session — ${session.webUrl}`);
    },

    /** A run began — the agent is working on this turn (⏳ on the message). */
    onWorking: (m) => react(m, "hourglass_flowing_sand"),

    /** The run finished — turn done, session idle, waiting for the user: clear
     *  the ⏳ and add ✅ on the message that drove the turn. */
    async onIdle(m) {
      log.info({ channel: m.channel, thread: m.threadRoot, ts: m.ts }, "slack: turn complete — waiting for the user");
      await unreact(m, "hourglass_flowing_sand");
      await react(m, "white_check_mark");
    },

    /** Render or extend the turn's running message. With no `ref` we post a new
     *  thread message; with one we edit it in place (the framework hands us the
     *  full accumulated text each time). */
    async onAssistantMessage(m, text, ref) {
      const blocks = buildMessageBlocks(text);
      if (blocks.length === 0) return ref ?? "";
      const fallback = text.length > 3000 ? `${text.slice(0, 2999)}…` : text;
      if (ref) {
        const c = await getClient();
        await c.chat.update({ channel: m.channel, ts: ref, text: fallback, blocks });
        return ref;
      }
      return post(m, fallback, blocks);
    },

    async onUserQuestion(m, ev) {
      log.info({ channel: m.channel, thread: m.threadRoot }, "slack: posting agent question");
      const parsed = parseUserQuestion(ev.payloadJson);
      if (!parsed) return post(m, "The agent asked a question (couldn't render it here).");
      const blocks = buildQuestionBlocks(route(m), parsed);
      const text = parsed.questions.map((q) => q.question).join(" / ") || "The agent has a question";
      return post(m, text, blocks);
    },

    async onAnswered(m, ev, ref) {
      const answers = parseAnswers(ev.payloadJson);
      const blocks = buildAnsweredBlocks(answers);
      const text = "Answered";
      if (ref) {
        const c = await getClient();
        await c.chat.update({ channel: m.channel, ts: ref, text, blocks });
      } else {
        await post(m, text, blocks);
      }
    },

    async onAsset(m, ev) {
      const asset = summarizeAsset(ev);
      if (!asset) return; // transient action — not worth a thread post
      await post(m, buildAssetLine(asset));
    },

    async onComplete(m, session: StartedSession, summary) {
      log.info(
        { channel: m.channel, thread: m.threadRoot, sessionId: session.id, assets: summary.assets.length },
        "slack: session complete",
      );
      await post(m, "Session complete", buildClosingBlocks(session, summary));
    },

    async onFail(m, message) {
      log.warn({ channel: m.channel, thread: m.threadRoot, reason: message }, "slack: session failed");
      await react(m, "x");
      await post(m, `❌ ${message}`);
    },

    async gatherThreadContext(m, since) {
      const c = await getClient();
      const res = await c.conversations.replies({
        channel: m.channel,
        ts: m.threadRoot,
        ...(since ? { oldest: since } : {}),
      });
      // m.ts is the triggering @mention — the directive; everything else in the
      // thread is context (wrapped). For a follow-up the workflow passes the new
      // mention, so its ts is the directive for that turn.
      return foldReplies(res.messages ?? [], since, botUserId, m.ts);
    },
  };

  function parseAnswers(payloadJson: string): Record<string, string[]> {
    try {
      const a = (JSON.parse(payloadJson) as { answers?: unknown }).answers;
      return a && typeof a === "object" ? (a as Record<string, string[]>) : {};
    } catch {
      return {};
    }
  }
}
