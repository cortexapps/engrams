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
  // `questionTs` and `assets` are plain workflow-local state, rebuilt
  // deterministically on replay from the checkpointed recv'd messages.
  const questionTs = new Map<string, string>();
  const assets: AssetSummary[] = [];
  let lastTs = ctx0.maxTs;

  for (;;) {
    const msg = await DBOS.recv<ThreadInbox>(THREAD_TOPIC, RECV_TIMEOUT_S);
    if (msg === null) continue; // idle — keep waiting for the terminal

    switch (msg.kind) {
      case "session_terminal": {
        if (msg.ok) {
          const summary = { lastMessage: msg.lastMessage ?? null, assets };
          await DBOS.runStep(() => pol.onComplete(m, session, summary), { name: "onComplete" });
        } else {
          await DBOS.runStep(() => pol.onFail(m, SESSION_FAILED_MSG), { name: "onFail" });
        }
        return;
      }
      case "session_event": {
        await dispatchSessionEvent(pol, m, msg, questionTs, assets);
        break;
      }
      case "trigger_mention": {
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

/** Route one curated session event to the policy, tracking question refs so a
 *  later `question_answered` can update the posted message. */
async function dispatchSessionEvent(
  pol: CommunicationPolicy,
  m: SourceMention,
  msg: Extract<ThreadInbox, { kind: "session_event" }>,
  questionTs: Map<string, string>,
  assets: AssetSummary[],
): Promise<void> {
  const effect = routeSessionEvent(msg.event);
  switch (effect.kind) {
    case "question": {
      const ref = await DBOS.runStep(() => pol.onUserQuestion(m, msg.event), {
        name: "onUserQuestion",
      });
      if (effect.toolCallId) questionTs.set(effect.toolCallId, ref);
      break;
    }
    case "answered": {
      const ref = effect.toolCallId ? questionTs.get(effect.toolCallId) : undefined;
      await DBOS.runStep(() => pol.onAnswered(m, msg.event, ref), { name: "onAnswered" });
      break;
    }
    case "asset": {
      // Render it live, and accumulate durable assets for the closing recap
      // (transient actions summarize to null and are skipped).
      const recap = summarizeAsset(msg.event);
      if (recap) assets.push(recap);
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
