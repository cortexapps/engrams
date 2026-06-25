/**
 * SlackThreadWorkflow — the brain, one per thread (ADR 0059 P1.4).
 *
 * `workflowID = task:<hash(team,channel,thread_root)>`, so a thread maps to
 * exactly one workflow (idempotent start). It is the framework half of the
 * boundary principle — *sessions propose, orchestrator decides* — driving a
 * per-source `CommunicationPolicy` for all provider mechanics and a
 * `ThreadControlPlane` for session lifecycle. Both are injected (P2 wires the
 * real Slack policy + control-plane client; tests inject fakes).
 *
 * Shape: the first `recv` is the initial @mention; ack pickup, resolve the
 * engrams user (unlinked → fail), pick the org default profile, gather the
 * thread into a prompt, create the session (carrying the policy's constant
 * system prompt), ack started, start the per-session pump, then a single recv
 * loop multiplexes session events ∪ trigger events off `THREAD_TOPIC`:
 *   - session_event   → route to the policy (question/answer-update/asset)
 *   - session_terminal → closing summary (ok) or failure, then exit
 *   - trigger_mention  → gather NEW context, SendPrompt (idempotent prompt_id)
 *   - trigger_answer   → AnswerQuestion
 *
 * `questionTs` (tool_call_id → posted-question ref) is plain workflow-local
 * state: it is rebuilt deterministically on replay from the checkpointed
 * `onUserQuestion` step outputs, so it needs no table and no continue-as-new.
 */

import { DBOS } from "@dbos-inc/dbos-sdk";
import {
  routeSessionEvent,
  summarizeAsset,
  type AssetSummary,
  type CommunicationPolicy,
  type StartedSession,
} from "./communication-policy.ts";
import { THREAD_TOPIC, type ThreadInbox, type SourceMention } from "./thread-inbox.ts";
import { sessionIngestWorkflow } from "./session-ingest.ts";

/** Session-lifecycle ops the thread workflow needs, behind one seam so the
 *  workflow logic is tested without a live coordinator. P2 wires the real
 *  client (profile-compiled createSession, identity via Slack users.info). */
export interface CreateSessionInput {
  profileId: string;
  ownerUserId: string;
  prompt: string;
  appendSystemPrompt: string;
  /** The trigger ref recorded on the persisted task (operator-visible). */
  source: Record<string, unknown>;
}

export interface ThreadControlPlane {
  /** Map a provider user to an engrams user id (email match); null = unlinked. */
  resolveUser(provider: string, externalUserId: string): Promise<string | null>;
  /** The org's `is_default` profile, or null if none is configured. */
  getDefaultProfile(): Promise<{ id: string } | null>;
  createSession(input: CreateSessionInput): Promise<StartedSession>;
  /** Deliver a follow-up prompt; `promptId` is the dedupe key (Decision 9). */
  sendPrompt(sessionId: string, prompt: string, promptId: string): Promise<void>;
  answerQuestion(
    sessionId: string,
    toolCallId: string,
    answers: Record<string, string[]>,
  ): Promise<void>;
}

// Injected dependencies (P2 wires real impls at module init; tests inject fakes).
let policy: CommunicationPolicy | undefined;
let controlPlane: ThreadControlPlane | undefined;

export function setThreadPolicy(p: CommunicationPolicy | undefined): void {
  policy = p;
}
export function setThreadControlPlane(cp: ThreadControlPlane | undefined): void {
  controlPlane = cp;
}

function requirePolicy(): CommunicationPolicy {
  if (!policy) throw new Error("thread CommunicationPolicy not configured (wired in P2)");
  return policy;
}
function requireControlPlane(): ThreadControlPlane {
  if (!controlPlane) throw new Error("thread ThreadControlPlane not configured (wired in P2)");
  return controlPlane;
}

/** Recv timeout (seconds). A timeout just re-loops — the session may be idle
 *  for a long time between events; the pump always sends the terminal. */
const RECV_TIMEOUT_S = 3_600;

const NO_USER_MSG =
  "You don't have a user in engrams — log in first, then try again.";
const NO_PROFILE_MSG =
  "No default profile is configured — set one in engrams first.";
const CREATE_FAIL_MSG = "Couldn't start a session for this request.";
const SESSION_FAILED_MSG = "The session ended in failure.";

async function slackThreadWorkflowImpl(): Promise<void> {
  const pol = requirePolicy();
  const cp = requireControlPlane();

  // 1) Initial @mention (the start-then-send handshake from the HTTP handler).
  const first = await DBOS.recv<ThreadInbox>(THREAD_TOPIC, RECV_TIMEOUT_S);
  if (first === null || first.kind !== "trigger_mention") return;
  const m = first.mention;

  await DBOS.runStep(() => pol.onPickup(m), { name: "onPickup" });

  const userId = await DBOS.runStep(() => cp.resolveUser("slack", m.user), {
    name: "resolveUser",
  });
  if (!userId) {
    await DBOS.runStep(() => pol.onFail(m, NO_USER_MSG), { name: "onFail" });
    return;
  }

  const profile = await DBOS.runStep(() => cp.getDefaultProfile(), { name: "getDefaultProfile" });
  if (!profile) {
    await DBOS.runStep(() => pol.onFail(m, NO_PROFILE_MSG), { name: "onFail" });
    return;
  }

  const ctx0 = await DBOS.runStep(() => pol.gatherThreadContext(m, null), {
    name: "gatherThreadContext",
  });

  let session: StartedSession;
  try {
    session = await DBOS.runStep(
      () =>
        cp.createSession({
          profileId: profile.id,
          ownerUserId: userId,
          prompt: ctx0.prompt,
          appendSystemPrompt: pol.systemPromptAppend,
          source: {
            provider: "slack",
            team: m.team,
            channel: m.channel,
            threadRoot: m.threadRoot,
          },
        }),
      { name: "createSession" },
    );
  } catch {
    await DBOS.runStep(() => pol.onFail(m, CREATE_FAIL_MSG), { name: "onFail" });
    return;
  }

  await DBOS.runStep(() => pol.onStarted(m, session), { name: "onStarted" });

  // 2) Start the per-session pump; it sends curated events back to us.
  await DBOS.startWorkflow(sessionIngestWorkflow, {
    workflowID: `ingest:${session.id}`,
  })({ sessionId: session.id, threadWfId: DBOS.workflowID! });

  // 3) Drain loop — one recv multiplexes session events ∪ trigger events.
  // `st` is plain workflow-local render state, rebuilt deterministically on
  // replay from the checkpointed recv'd messages + step outputs (the bubble ts
  // is a checkpointed `onAssistantMessage` output; the accumulated text is
  // recomputed from the recv'd message events). `currentMention` is the mention
  // driving the live turn, so the run lifecycle reacts on the right message.
  const st: ThreadRender = {
    questionTs: new Map<string, string>(),
    assets: [],
    bubble: null,
    currentMention: m,
  };
  let lastTs = ctx0.maxTs;

  for (;;) {
    const msg = await DBOS.recv<ThreadInbox>(THREAD_TOPIC, RECV_TIMEOUT_S);
    if (msg === null) continue; // idle — keep waiting for the terminal

    switch (msg.kind) {
      case "session_terminal": {
        if (msg.ok) {
          const summary = { lastMessage: msg.lastMessage ?? null, assets: st.assets };
          await DBOS.runStep(() => pol.onComplete(m, session, summary), { name: "onComplete" });
        } else {
          await DBOS.runStep(() => pol.onFail(m, SESSION_FAILED_MSG), { name: "onFail" });
        }
        return;
      }
      case "session_event": {
        await dispatchSessionEvent(pol, m, msg, st);
        break;
      }
      case "trigger_mention": {
        // Acknowledge the new message itself (👀), like the initial mention; the
        // ⏳→✅ working/done indicator rides this turn's run lifecycle, reacting
        // on the new message (currentMention).
        await DBOS.runStep(() => pol.onPickup(msg.mention), { name: "onPickup" });
        st.currentMention = msg.mention;
        st.bubble = null; // a new turn — the next response starts a fresh message
        const ctx = await DBOS.runStep(() => pol.gatherThreadContext(m, lastTs), {
          name: "gatherThreadContext",
        });
        await DBOS.runStep(
          () => cp.sendPrompt(session.id, ctx.prompt, `slack:${msg.mention.eventId}`),
          { name: "sendPrompt" },
        );
        lastTs = ctx.maxTs;
        break;
      }
      case "trigger_answer": {
        await DBOS.runStep(
          () => cp.answerQuestion(session.id, msg.answer.toolCallId, msg.answer.answers),
          { name: "answerQuestion" },
        );
        break;
      }
    }
  }
}

/** Per-thread render state the drain loop threads through `dispatchSessionEvent`.
 *  All fields are workflow-local and replay-deterministic. */
interface ThreadRender {
  /** tool_call_id → posted question `ts`, so an answer updates that message. */
  questionTs: Map<string, string>;
  /** Durable assets, accumulated for the closing recap. */
  assets: AssetSummary[];
  /** The active assistant message consecutive responses coalesce into, or null
   *  when the next response should open a fresh message. */
  bubble: { ts: string; text: string } | null;
  /** The mention driving the live turn — the run lifecycle reacts on it. */
  currentMention: SourceMention;
}

/** Cap an assistant bubble's accumulated text; past this a new response opens a
 *  fresh message rather than re-sending an ever-growing `chat.update` payload. */
const MAX_BUBBLE_CHARS = 8000;

/** Route one curated session event to the policy. Assistant responses coalesce
 *  into `st.bubble`; questions/assets get their own message (and seal the bubble
 *  so thread ordering is preserved); the run lifecycle drives the ⏳→✅ indicator. */
async function dispatchSessionEvent(
  pol: CommunicationPolicy,
  m: SourceMention,
  msg: Extract<ThreadInbox, { kind: "session_event" }>,
  st: ThreadRender,
): Promise<void> {
  const effect = routeSessionEvent(msg.event);
  switch (effect.kind) {
    case "message": {
      if (!effect.text) break;
      // Append to the live bubble unless it would overflow — then roll to a new
      // one. `ref` undefined posts a fresh message; set edits in place.
      const append =
        st.bubble !== null && st.bubble.text.length + effect.text.length + 2 <= MAX_BUBBLE_CHARS;
      const text = append ? `${st.bubble!.text}\n\n${effect.text}` : effect.text;
      const ref = append ? st.bubble!.ts : undefined;
      const ts = await DBOS.runStep(() => pol.onAssistantMessage(m, text, ref), {
        name: "onAssistantMessage",
      });
      st.bubble = { ts, text };
      break;
    }
    case "working": {
      st.bubble = null; // a new run — its first response opens a fresh message
      await DBOS.runStep(() => pol.onWorking(st.currentMention), { name: "onWorking" });
      break;
    }
    case "idle": {
      st.bubble = null;
      await DBOS.runStep(() => pol.onIdle(st.currentMention), { name: "onIdle" });
      break;
    }
    case "question": {
      st.bubble = null; // the question is its own message
      const ref = await DBOS.runStep(() => pol.onUserQuestion(m, msg.event), {
        name: "onUserQuestion",
      });
      if (effect.toolCallId) st.questionTs.set(effect.toolCallId, ref);
      break;
    }
    case "answered": {
      const ref = effect.toolCallId ? st.questionTs.get(effect.toolCallId) : undefined;
      await DBOS.runStep(() => pol.onAnswered(m, msg.event, ref), { name: "onAnswered" });
      break;
    }
    case "asset": {
      // The asset is its own message; accumulate durable ones for the closing
      // recap (transient actions summarize to null and are skipped).
      st.bubble = null;
      const recap = summarizeAsset(msg.event);
      if (recap) st.assets.push(recap);
      await DBOS.runStep(() => pol.onAsset(m, msg.event), { name: "onAsset" });
      break;
    }
    case "ignore":
      break;
  }
}

export const slackThreadWorkflow = DBOS.registerWorkflow(slackThreadWorkflowImpl, {
  name: "SlackThreadWorkflow",
});
