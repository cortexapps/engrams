/**
 * Slack CommunicationPolicy (ADR 0060 P2.10) — the v1 provider-mechanics impl
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
  parseQuestionAnswers,
  buildQuestionBlocks,
  buildAnsweredBlocks,
  buildAssetLine,
  buildClosingBlocks,
  buildMessageBlocks,
  buildProfilePickerBlocks,
  buildProfileChosenBlocks,
} from "./slack-blocks.ts";
import { summarizeAsset, type CommunicationPolicy, type StartedSession } from "../workflows/communication-policy.ts";
import type { SourceMention } from "../workflows/thread-inbox.ts";
import { fetchArtifactBytes, type FetchedArtifact } from "../control-plane/artifact-fetch.ts";

const log = rootLog.child({ component: "slack" });

/** The flavor appended to a triggered agent's system prompt (ADR 0060 Decision
 *  8) — NOT connector config; a constant this policy provides at session
 *  create so the agent behaves well in a chat thread. */
const SYSTEM_PROMPT_APPEND = `You are running inside an engrams session triggered from a Slack thread.
Keep replies concise and chat-friendly.

When you need a decision or clarification from the user, ask via the ask_user_question tool — it renders as interactive buttons in Slack — rather than guessing. The user cannot see your terminal, so surface results, links, and artifacts explicitly. When handling code related tasks, prefer showing your work rather than just saying you're done. Prefer video over images if available.

Conform to slack markdown in your responses. Examples:
Links are formatted as <url|optional link title>
Bold is single asterisks surrounding text, like *this*.
Italics are underlines surrounding text like _this_.`;

/** Inline-upload a shared file to Slack up to this size; a larger artifact posts
 *  a link to the session instead, so the orchestrator never buffers a huge blob
 *  in memory just to forward it. */
const MAX_SLACK_UPLOAD_BYTES = 50 * 1024 * 1024; // 50 MiB

/** A reply in a Slack thread (the subset the prompt fold reads). */
export interface SlackReply {
  ts?: string;
  user?: string;
  bot_id?: string;
  text?: string;
  /** Legacy attachments — bot/app posts often carry their content here with an
   *  empty top-level `text` (alerts, GitHub, workflow posts). */
  attachments?: { title?: string; text?: string; fallback?: string }[];
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
  files: {
    uploadV2(args: {
      channel_id: string;
      thread_ts?: string;
      file: Buffer | Uint8Array;
      filename: string;
      title?: string;
      initial_comment?: string;
    }): Promise<unknown>;
  };
}

/** Fetch a session artifact's bytes for upload (injectable for tests; the
 *  default collects them from the coordinator via `getArtifact`). */
export type FetchArtifactFn = (sessionId: string, artifactId: string) => Promise<FetchedArtifact>;

export interface SlackPolicyDeps {
  client?: () => Promise<SlackPolicyClient>;
  /** The bot's own user id, so its `<@bot>` mention is stripped from prompts.
   *  Optional: when unset, all `<@…>` mentions are stripped. */
  botUserId?: string;
  /** Fetch a shared artifact's bytes (injectable for tests). */
  fetchArtifact?: FetchArtifactFn;
}

/**
 * Fold a page of thread replies into the session prompt + the new cursor. Pure.
 * The cursor (`maxTs`) advances past EVERY reply seen — including the bot's own
 * — so the next gather never re-reads. Bot-authored replies never feed the
 * prompt (the agent must not be prompted with its own posts) — EXCEPT the
 * thread ROOT (`rootTs`): it is the subject of the thread, so a bot-authored
 * root (an alert, a workflow post, another app) stays in as context. `since`
 * is exclusive: `conversations.replies(oldest=)` is inclusive, so the boundary
 * message is dropped here.
 *
 * Shape: the triggering @mention (identified by `triggerTs`) is the directive
 * and goes at the BOTTOM; every other kept message is prior thread context,
 * wrapped in `<thread context>…</thread context>` above it. With no other
 * messages the prompt is just the directive (no wrapper). If `triggerTs` matches
 * nothing in this page (the mention wasn't returned), fall back to a plain join.
 */
export function foldReplies(
  messages: SlackReply[],
  since: string | null,
  botUserId?: string,
  triggerTs?: string,
  rootTs?: string,
): { prompt: string; maxTs: string } {
  let maxTs = since ?? "0";
  const kept: { ts: string; text: string }[] = [];
  for (const msg of messages) {
    const ts = msg.ts ?? "";
    if (since && num(ts) <= num(since)) continue; // already delivered
    if (num(ts) > num(maxTs)) maxTs = ts; // advance past everything seen
    const isRoot = rootTs !== undefined && ts === rootTs;
    if (!isRoot && (msg.bot_id || (botUserId && msg.user === botUserId))) continue;
    const text = stripMentions(replyText(msg), botUserId).trim();
    if (text) kept.push({ ts, text });
  }

  const idx = triggerTs ? kept.findIndex((h) => h.ts === triggerTs) : -1;
  if (idx === -1) {
    // No identified directive — emit the messages plainly, no wrapper.
    return { prompt: kept.map((h) => h.text).join("\n\n"), maxTs };
  }
  const context = kept.filter((_, i) => i !== idx).map((h) => h.text);
  const directive = kept[idx].text;
  const prompt = context.length
    ? `<thread context>\n${context.join("\n")}\n</thread context>\n\n${directive}`
    : directive;
  return { prompt, maxTs };
}

/** A message's prompt text. Bot/app posts often put their content in legacy
 *  `attachments` and leave `text` empty — fold those in only then, so link
 *  unfurls (attachments alongside real text) never add noise. */
function replyText(msg: SlackReply): string {
  if (msg.text?.trim()) return msg.text;
  const parts: string[] = [];
  for (const a of msg.attachments ?? []) {
    const body = [a.title, a.text].filter((s) => s?.trim()).join("\n") || a.fallback || "";
    if (body.trim()) parts.push(body);
  }
  return parts.join("\n");
}

const num = (ts: string): number => Number.parseFloat(ts) || 0;

/** Strip the bot's `<@id>` mention (or all mentions when the id is unknown).
 *  Collapses runs of spaces/tabs (the hole a removed mention leaves) but keeps
 *  newlines — multi-line messages must reach the prompt intact. */
function stripMentions(text: string, botUserId?: string): string {
  const re = botUserId ? new RegExp(`<@${botUserId}(\\|[^>]*)?>`, "g") : /<@[^>]+>/g;
  return text.replace(re, " ").replace(/[^\S\n]+/g, " ");
}

/** The slice of a `file_shared` event payload the upload path reads. */
interface SharedFile {
  artifactId: string;
  caption?: string;
  sizeBytes: number;
  mediaType: string;
}

/** Parse a `file_shared` event payload, or null if it carries no artifact id.
 *  Pure; never throws. (Shape: the coordinator's `SessionEvent::FileShared`.) */
function parseFileShared(payloadJson: string): SharedFile | null {
  try {
    const p = JSON.parse(payloadJson) as {
      artifact_id?: unknown;
      caption?: unknown;
      size_bytes?: unknown;
      media_type?: unknown;
    };
    if (typeof p.artifact_id !== "string" || !p.artifact_id) return null;
    return {
      artifactId: p.artifact_id,
      caption: typeof p.caption === "string" && p.caption ? p.caption : undefined,
      sizeBytes: typeof p.size_bytes === "number" ? p.size_bytes : Number(p.size_bytes) || 0,
      mediaType: typeof p.media_type === "string" ? p.media_type : "",
    };
  } catch {
    return null;
  }
}

/** Extension by coordinator-detected media type — Slack uses the filename's
 *  extension to choose the right inline preview. */
const EXT_BY_MEDIA: Readonly<Record<string, string>> = {
  "image/png": "png",
  "image/jpeg": "jpg",
  "image/gif": "gif",
  "image/webp": "webp",
  "video/mp4": "mp4",
  "video/webm": "webm",
  "video/quicktime": "mov",
};

/** A filename for an artifact that arrived without one. */
function synthesizeFilename(mediaType: string): string {
  return `engram-artifact.${EXT_BY_MEDIA[mediaType] ?? "bin"}`;
}

/** Build the production Slack CommunicationPolicy (client injectable for tests). */
export function makeSlackPolicy(deps: SlackPolicyDeps = {}): CommunicationPolicy {
  const getClient =
    deps.client ?? (async () => (await getSlackClient()) as unknown as SlackPolicyClient);
  const botUserId = deps.botUserId;
  const fetchArtifact: FetchArtifactFn =
    deps.fetchArtifact ??
    ((sessionId, artifactId) => fetchArtifactBytes(sessionId, artifactId, MAX_SLACK_UPLOAD_BYTES));

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

    async onProfileChoice(m, options) {
      log.info(
        { channel: m.channel, thread: m.threadRoot, options: options.length },
        "slack: asking the user to pick a profile",
      );
      // The mentioning user (m.user) is the only one whose pick is accepted;
      // the mention's eventId dedupes the ask (first selection wins).
      const blocks = buildProfilePickerBlocks(route(m), m.user, m.eventId, options);
      return post(m, "Which profile should handle this?", blocks);
    },

    async onProfileChosen(m, ref, profileName) {
      const text = `Running with ${profileName}`;
      const blocks = buildProfileChosenBlocks(profileName);
      if (ref) {
        const c = await getClient();
        await c.chat.update({ channel: m.channel, ts: ref, text, blocks });
      } else {
        await post(m, text, blocks);
      }
    },

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
      const answers = parseQuestionAnswers(ev.payloadJson);
      const blocks = buildAnsweredBlocks(answers);
      const text = "Answered";
      if (ref) {
        const c = await getClient();
        await c.chat.update({ channel: m.channel, ts: ref, text, blocks });
      } else {
        await post(m, text, blocks);
      }
    },

    async onAsset(m, ev, session) {
      // A shared file (ADR 0026 `file_shared`) is uploaded into the thread as the
      // actual image/video, so it renders in Slack the way it does in the web UI.
      // Anything else — and any file we can't fetch or that's over the size cap —
      // posts a one-line message instead.
      const file = ev.kind === "file_shared" ? parseFileShared(ev.payloadJson) : null;
      if (file && file.sizeBytes <= MAX_SLACK_UPLOAD_BYTES) {
        try {
          const art = await fetchArtifact(session.id, file.artifactId);
          const c = await getClient();
          await c.files.uploadV2({
            channel_id: m.channel,
            thread_ts: m.threadRoot,
            file: Buffer.from(art.bytes),
            filename: art.fileName || synthesizeFilename(art.mediaType || file.mediaType),
            ...(file.caption ? { initial_comment: file.caption } : {}),
          });
          return;
        } catch (err) {
          log.warn(
            { channel: m.channel, thread: m.threadRoot, artifactId: file.artifactId, err },
            "slack: artifact upload failed — falling back to a session link",
          );
          // fall through to the link fallback
        }
      }
      if (file) {
        // Couldn't upload the bytes (too big, or the upload/fetch failed) — at
        // least give a clickable link to the session, where the file renders.
        await post(m, buildAssetLine({ label: file.caption || "shared a file", url: session.webUrl }));
        return;
      }
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

    async onNeutralClose(m, message) {
      // The sandbox was reclaimed (host roll / host_lost / dev churn), not a
      // failure — post a plain informational note, no ❌ and no reaction.
      log.info({ channel: m.channel, thread: m.threadRoot }, "slack: session closed (sandbox reclaimed)");
      await post(m, message);
    },

    /** A retryable delivery hiccup — the thread is still live. ⚠️ on the mention
     *  (so the user sees their message didn't land) + an actionable note. Unlike
     *  onFail, this does NOT end the thread; the post is best-effort so a Slack
     *  blip here can never wedge the workflow. */
    async onDeliveryError(m, message) {
      log.warn({ channel: m.channel, thread: m.threadRoot, reason: message }, "slack: delivery failed (non-fatal)");
      await react(m, "warning");
      try {
        await post(m, `⚠️ ${message}`);
      } catch {
        /* best-effort — if Slack is down we can't notify, but we must not throw */
      }
    },

    async gatherThreadContext(m, since) {
      const c = await getClient();
      const res = await c.conversations.replies({
        channel: m.channel,
        ts: m.threadRoot,
        ...(since ? { oldest: since } : {}),
      });
      // m.ts is the triggering @mention — the directive; everything else in the
      // thread is context (wrapped), INCLUDING a bot-authored root. For a
      // follow-up the workflow passes the new mention, so its ts is the
      // directive for that turn (the root is behind the cursor by then).
      return foldReplies(res.messages ?? [], since, botUserId, m.ts, m.threadRoot);
    },
  };
}
