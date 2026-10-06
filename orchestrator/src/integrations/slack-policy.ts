/**
 * Slack CommunicationPolicy (ADR 0060 P2.10) — the provider-mechanics impl
 * behind the relay block's policy seam (ADR 0119). Outbound Block Kit shaping
 * lives in slack-blocks.ts; this module is the thin layer that drives the
 * Slack WebClient (reactions, posts, updates, uploads). Every method is
 * invoked by the framework as a checkpointed DBOS step, so it runs once per
 * effect.
 *
 * The Slack client is injected (default = the authed WebClient from slack.ts),
 * which keeps the policy unit-testable without a network or the engine.
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
  type ThreadRoute,
} from "./slack-blocks.ts";
import {
  summarizeAsset,
  type CommunicationPolicy,
  type SourceMention,
  type StartedSession,
} from "../workflows/communication-policy.ts";
import { fetchArtifactBytes, type FetchedArtifact } from "../control-plane/artifact-fetch.ts";

const log = rootLog.child({ component: "slack" });

/** Inline-upload a shared file to Slack up to this size; a larger artifact posts
 *  a link to the session instead, so the orchestrator never buffers a huge blob
 *  in memory just to forward it. */
const MAX_SLACK_UPLOAD_BYTES = 50 * 1024 * 1024; // 50 MiB

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
  /** Extra route fields stamped into every Block Kit value this policy posts
   * (the automation relay sets `{runId}` so answers route to its run). */
  routeExtras?: Partial<Pick<ThreadRoute, "runId">>;
  /** Fetch a shared artifact's bytes (injectable for tests). */
  fetchArtifact?: FetchArtifactFn;
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
  const fetchArtifact: FetchArtifactFn =
    deps.fetchArtifact ??
    ((sessionId, artifactId) => fetchArtifactBytes(sessionId, artifactId, MAX_SLACK_UPLOAD_BYTES));

  const route = (m: SourceMention): ThreadRoute => ({
    team: m.team,
    channel: m.channel,
    threadRoot: m.threadRoot,
    ...(deps.routeExtras ?? {}),
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
  };
}
