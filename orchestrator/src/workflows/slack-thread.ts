/**
 * SlackThreadWorkflow — the brain, one per thread (ADR 0060 P1.4).
 *
 * `workflowID = task:<hash(team,channel,thread_root)>`, so a thread maps to
 * exactly one workflow (idempotent start). It is the framework half of the
 * boundary principle — *sessions propose, orchestrator decides* — driving a
 * per-source `CommunicationPolicy` for all provider mechanics and a
 * `ThreadControlPlane` for session lifecycle. Both are injected (P2 wires the
 * real Slack policy + control-plane client; tests inject fakes).
 *
 * Shape: the first `recv` is the initial @mention; ack pickup, resolve the
 * engrams user (unlinked → fail), gather the thread into a prompt, PICK the
 * profile (an LLM routes on capability cards + usage histograms; `ask_user`
 * renders a dropdown and waits durably for `trigger_profile_choice`), create
 * the task (carrying the policy's constant system prompt), ack started, bind
 * the session listener, then a single recv loop multiplexes session events ∪
 * trigger events off `THREAD_TOPIC`:
 *   - session_event   → route to the policy (question/answer-update/asset)
 *   - session_terminal → closing summary (ok) or failure, then exit
 *   - trigger_mention  → gather NEW context, SendPrompt (idempotent prompt_id)
 *   - trigger_answer   → CompleteToolCall (generic); legacy cards get an upgrade notice
 *   - trigger_profile_choice → resolves a pending profile ask (ignored once live)
 *
 * `questionTs` (tool_call_id → posted-question ref) is plain workflow-local
 * state: it is rebuilt deterministically on replay from the checkpointed
 * `onUserQuestion` step outputs, so it needs no table and no continue-as-new.
 */

import { DBOS } from "@dbos-inc/dbos-sdk";
import { Code, ConnectError } from "@connectrpc/connect";
import { config } from "../config.ts";
import { log as rootLog } from "../log.ts";
import {
  routeSessionEvent,
  summarizeAsset,
  type AssetSummary,
  type ClosingSummary,
  type CommunicationPolicy,
  type QuestionProtocol,
  type StartedSession,
} from "./communication-policy.ts";
import { THREAD_TOPIC, type ThreadInbox, type SourceMention } from "./thread-inbox.ts";
import type { PickInput, ProfileOption, ProfilePick } from "../routing/profile-picker.ts";

const log = rootLog.child({ component: "slack" });

/** Session-lifecycle ops the thread workflow needs, behind one seam so the
 *  workflow logic is tested without a live coordinator. P2 wires the real
 *  client (profile-compiled createTask, identity via Slack users.info). */
export interface CreateTaskInput {
  profileId: string;
  ownerUserId: string;
  prompt: string;
  appendSystemPrompt: string;
  /** The trigger ref recorded on the persisted task (operator-visible). */
  source: Record<string, unknown>;
  /** Stable DBOS mailbox id, persisted atomically before listener discovery. */
  threadWorkflowId: string;
}

export interface ThreadControlPlane {
  /** Map a provider user to an engrams user id (email match); null = unlinked. */
  resolveUser(provider: string, externalUserId: string): Promise<string | null>;
  /** Route the mention to a profile (LLM over capability cards + histograms).
   *  `ask_user` carries the ranked dropdown options; `none` = no profiles. */
  pickProfile(input: PickInput): Promise<ProfilePick>;
  /** Create the task (and its primary session) the thread drives — the same
   *  create path as a UI chat task; never a bare session (ADR 0060). */
  createTask(input: CreateTaskInput): Promise<StartedSession>;
  /** Deliver a follow-up prompt; `promptId` is the dedupe key (Decision 9). */
  sendPrompt(sessionId: string, prompt: string, promptId: string): Promise<void>;
  /** Complete an ADR 0089 session-handled tool through its registered schema. */
  completeToolCall(
    sessionId: string,
    toolCallId: string,
    result: Record<string, string[]>,
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
 *  for a long time between events; the listener sends the terminal. */
const RECV_TIMEOUT_S = 3_600;

const NO_USER_MSG =
  "You don't have a user in engrams — log in first, then try again.";
const NO_PROFILES_MSG = "No profiles are configured — create one in engrams first.";
const CREATE_FAIL_MSG = "Couldn't start a session for this request.";
const SESSION_FAILED_MSG = "The session ended in failure.";
// A neutral close: the sandbox was reclaimed (host roll / `host_lost` / dev-stack
// churn), which is not a task failure — the work stands, the thread just can't
// continue. See `TerminalOutcome` in session-events.ts.
const SESSION_CLOSED_MSG = "This session is complete. Start a new session if you'd like to continue.";
// Non-fatal: the thread stays alive after these so the user can retry.
// ADR 0067: SendPrompt and CompleteToolCall durably
// ENQUEUE on the coordinator (202) — a resuming/idle session is no longer a
// delivery failure, so there is no "mention me again" apology arm. These fire
// only for hard enqueue failures (session gone / coord unreachable).
const DELIVER_FAIL_MSG =
  "I couldn't queue that for your session (it may have ended). Start a new session to continue.";
const ANSWER_FAIL_MSG = "I couldn't record that answer — the session may have ended.";
const LEGACY_QUESTION_MSG =
  "This question predates an upgrade and can no longer be answered.";

/** Run an effect as a checkpointed step. The workflow passes `DBOS.runStep`; a
 *  test passes a plain runner so the drain-loop control flow is unit-testable
 *  without a live DBOS engine (the engine integration is deferred — ADR 0060). */
export type StepRunner = <T>(fn: () => Promise<T>, name: string) => Promise<T>;

async function slackThreadWorkflowImpl(): Promise<void> {
  const pol = requirePolicy();
  const cp = requireControlPlane();

  // 1) Initial @mention (the start-then-send handshake from the HTTP handler).
  const first = await DBOS.recv<ThreadInbox>(THREAD_TOPIC, RECV_TIMEOUT_S);
  if (first === null || first.kind !== "trigger_mention") return;
  const m = first.mention;

  const step: StepRunner = (fn, name) => DBOS.runStep(fn, { name });
  const threadWorkflowId = DBOS.workflowID;
  if (!threadWorkflowId) throw new Error("Slack thread workflow ID is unavailable");

  await step(() => pol.onPickup(m), "onPickup");

  // Everything up to (and including) the session create is FATAL-on-failure:
  // there is no session to keep alive yet, so any throw — or an unresolved user
  // / zero configured profiles — ends the thread with an actionable ❌. (A throw
  // here used to escape the workflow uncaught, leaving only the 👀 and silence.)
  // `mention` is the mention driving the pre-session phase — it advances if new
  // mentions arrive while a profile ask is pending.
  let session: StartedSession;
  let mention = m;
  let ctx0: { prompt: string; maxTs: string };
  try {
    const userId = await step(() => cp.resolveUser("slack", mention.user), "resolveUser");
    if (!userId) {
      await step(() => pol.onFail(mention, NO_USER_MSG), "onFail");
      return;
    }
    ctx0 = await step(() => pol.gatherThreadContext(mention, null), "gatherThreadContext");
    const pickInput = (prompt: string): PickInput => ({
      team: m.team,
      channel: m.channel,
      ownerUserId: userId,
      prompt,
    });
    let pick = await step(() => cp.pickProfile(pickInput(ctx0.prompt)), "pickProfile");
    if (pick.decision === "none") {
      await step(() => pol.onFail(mention, NO_PROFILES_MSG), "onFail");
      return;
    }
    let profile: ProfileOption;
    if (pick.decision === "route") {
      profile = pick.profile;
    } else {
      // ask_user: post the dropdown and wait durably — a human decision was
      // requested (explicitly, or the model abstained), so there is no
      // auto-start timeout. A new mention while waiting re-evaluates (the
      // extra context may be decisive); a decisive re-pick resolves the ask.
      let options = pick.options;
      const pickerRef = await step(() => pol.onProfileChoice(mention, options), "onProfileChoice");
      let chosen: ProfileOption | undefined;
      while (!chosen) {
        const msg = await DBOS.recv<ThreadInbox>(THREAD_TOPIC, RECV_TIMEOUT_S);
        if (msg === null) continue; // idle — keep waiting for the pick
        if (msg.kind === "trigger_profile_choice") {
          // Accept only an offered option — a stale/foreign id is ignored.
          chosen = options.find((o) => o.id === msg.choice.profileId);
        } else if (msg.kind === "trigger_mention") {
          await step(() => pol.onPickup(msg.mention), "onPickup").catch(() => {});
          mention = msg.mention;
          ctx0 = await step(() => pol.gatherThreadContext(mention, null), "gatherThreadContext");
          const re = await step(() => cp.pickProfile(pickInput(ctx0.prompt)), "pickProfile");
          if (re.decision === "route") chosen = re.profile;
          else if (re.decision === "ask_user") options = re.options;
        }
        // session_* / trigger_answer: impossible before a session exists — drop.
      }
      await step(() => pol.onProfileChosen(mention, pickerRef, chosen.name), "onProfileChosen");
      profile = chosen;
    }
    session = await step(
      () =>
        cp.createTask({
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
          threadWorkflowId,
        }),
      "createTask",
    );
  } catch (err) {
    // Surface WHY it failed — this path used to swallow the cause, leaving only
    // the generic "Couldn't start a session" with no way to diagnose.
    log.error({ channel: m.channel, thread: m.threadRoot, err }, "slack: failed to start session");
    // A FailedPrecondition's message is written to be user-facing (missing
    // harness or personal integration credentials, disabled image/connection):
    // post it instead of a dead-end generic failure, with a direct link when
    // it points at the credentials page.
    let message = CREATE_FAIL_MSG;
    if (err instanceof ConnectError && err.code === Code.FailedPrecondition) {
      message = err.rawMessage;
      if (message.includes("Settings → Credentials")) {
        message += ` ${config.baseUrl.replace(/\/$/, "")}/settings/credentials`;
      }
    }
    await step(() => pol.onFail(mention, message), "onFail");
    return;
  }

  await step(() => pol.onStarted(mention, session), "onStarted");

  // 2) Drain loop — one recv multiplexes session events ∪ trigger events.
  // `st` is plain workflow-local render state, rebuilt deterministically on
  // replay from the checkpointed recv'd messages + step outputs (the bubble ts
  // is a checkpointed `onAssistantMessage` output; the accumulated text is
  // recomputed from the recv'd message events). `currentMention` is the mention
  // driving the live turn, so the run lifecycle reacts on the right message.
  const st: ThreadRender = {
    questionTs: new Map<string, string>(),
    questionProtocols: new Map<string, QuestionProtocol>(),
    assets: [],
    bubble: null,
    lastAssistantText: null,
    currentMention: mention,
  };
  let lastTs = ctx0.maxTs;

  for (;;) {
    const msg = await DBOS.recv<ThreadInbox>(THREAD_TOPIC, RECV_TIMEOUT_S);
    if (msg === null) continue; // idle — keep waiting for the terminal

    if (msg.kind === "session_terminal") {
      switch (msg.outcome) {
        case "completed": {
          const summary = closingSummary(st);
          await step(() => pol.onComplete(m, session, summary), "onComplete");
          break;
        }
        case "failed":
          await step(() => pol.onFail(m, SESSION_FAILED_MSG), "onFail");
          break;
        case "neutral":
          await step(() => pol.onNeutralClose(m, SESSION_CLOSED_MSG), "onNeutralClose");
          break;
      }
      return;
    }
    // A non-terminal turn (session event / follow-up @mention / answer) is
    // handled with per-turn error isolation: a transient failure surfaces as a
    // NON-FATAL notice and the loop continues, so one bad turn can't brick an
    // otherwise-healthy thread (the 2026-06-26 "👀 then silence" incident).
    lastTs = await handleInbound(step, pol, cp, session, st, lastTs, msg);
  }
}

/** Inbound the drain loop handles after the session is live — everything except
 *  the terminal, which the loop handles inline. */
type InboundTurn = Exclude<ThreadInbox, { kind: "session_terminal" }>;

/**
 * Handle one non-terminal inbound message; return the (possibly advanced)
 * `lastTs` cursor. **Never throws** — that is the whole point:
 *
 * - An ENQUEUE failure (`sendPrompt` / `completeToolCall` — ADR 0067: these
 *   202-enqueue on the coordinator's durable outbox, so a resuming/idle
 *   session is never an error; only hard failures like a terminated
 *   session or an unreachable coord land here) is caught and surfaced via
 *   `onDeliveryError`; the cursor does NOT advance, so the dropped
 *   messages are re-gathered on the next mention. The thread stays alive.
 * - A render failure (a session event) drops that single render and logs — it's
 *   an agent OUTPUT, not the user's input, so no user-facing notice.
 *
 * Previously these ran unguarded, so an uncaught throw errored the entire
 * `SlackThreadWorkflow`, leaving the user with only the 👀 and no ❌ while the
 * underlying session kept working invisibly.
 */
export async function handleInbound(
  step: StepRunner,
  pol: CommunicationPolicy,
  cp: ThreadControlPlane,
  session: StartedSession,
  st: ThreadRender,
  lastTs: string,
  msg: InboundTurn,
): Promise<string> {
  switch (msg.kind) {
    case "session_event": {
      try {
        await dispatchSessionEvent(step, pol, st.currentMention, msg, st, session);
      } catch (err) {
        log.error(
          { sessionId: session.id, kind: msg.event.kind, err },
          "slack: failed to render a session event; dropping it",
        );
      }
      return lastTs;
    }
    case "trigger_mention": {
      // Acknowledge the new message itself (👀), like the initial mention; the
      // ⏳→✅ indicator rides this turn's run lifecycle on the new message.
      await step(() => pol.onPickup(msg.mention), "onPickup").catch(() => {});
      st.currentMention = msg.mention;
      st.bubble = null; // a new turn — the next response starts a fresh message
      try {
        // Gather as the NEW mention so it (not the original) is the directive at
        // the bottom of the prompt; the rest of the new messages are context.
        const ctx = await step(() => pol.gatherThreadContext(msg.mention, lastTs), "gatherThreadContext");
        await step(() => cp.sendPrompt(session.id, ctx.prompt, `slack:${msg.mention.eventId}`), "sendPrompt");
        return ctx.maxTs;
      } catch (err) {
        log.error(
          { sessionId: session.id, channel: msg.mention.channel, thread: msg.mention.threadRoot, err },
          "slack: failed to enqueue follow-up prompt — keeping the thread alive",
        );
        await step(() => pol.onDeliveryError(msg.mention, DELIVER_FAIL_MSG), "onDeliveryError").catch(() => {});
        return lastTs; // cursor unchanged: those messages were NOT delivered
      }
    }
    case "trigger_profile_choice":
      // The session is already live — the routing decision was made; a late
      // dropdown click has nothing left to resolve.
      return lastTs;
    case "trigger_answer": {
      const via = st.questionProtocols.get(msg.answer.toolCallId) ?? "legacy";
      if (via === "legacy") {
        await step(
          () => pol.onDeliveryError(st.currentMention, LEGACY_QUESTION_MSG),
          "onDeliveryError",
        ).catch(() => {});
        return lastTs;
      }
      try {
        await step(
          () => cp.completeToolCall(session.id, msg.answer.toolCallId, msg.answer.answers),
          "completeToolCall",
        );
      } catch (err) {
        log.error({ sessionId: session.id, err }, "slack: failed to enqueue answer — keeping the thread alive");
        await step(() => pol.onDeliveryError(st.currentMention, ANSWER_FAIL_MSG), "onDeliveryError").catch(() => {});
      }
      return lastTs;
    }
  }
}

/** Per-thread render state the drain loop threads through `dispatchSessionEvent`.
 *  All fields are workflow-local and replay-deterministic. */
export interface ThreadRender {
  /** tool_call_id → posted question `ts`, so an answer updates that message. */
  questionTs: Map<string, string>;
  /** tool_call_id → originating protocol, which selects the completion RPC. */
  questionProtocols: Map<string, QuestionProtocol>;
  /** Durable assets, accumulated for the closing recap. */
  assets: AssetSummary[];
  /** The active assistant message consecutive responses coalesce into, or null
   *  when the next response should open a fresh message. */
  bubble: { ts: string; text: string } | null;
  /** Most recent individual assistant message, retained for closing summary. */
  lastAssistantText: string | null;
  /** The mention driving the live turn — the run lifecycle reacts on it. */
  currentMention: SourceMention;
}

export function closingSummary(
  st: Pick<ThreadRender, "lastAssistantText" | "assets">,
): ClosingSummary {
  return { lastMessage: st.lastAssistantText, assets: st.assets };
}

/** Cap an assistant bubble's accumulated text; past this a new response opens a
 *  fresh message rather than re-sending an ever-growing `chat.update` payload. */
const MAX_BUBBLE_CHARS = 8000;

/** Route one curated session event to the policy. Assistant responses coalesce
 *  into `st.bubble`; questions/assets get their own message (and seal the bubble
 *  so thread ordering is preserved); the run lifecycle drives the ⏳→✅ indicator. */
async function dispatchSessionEvent(
  step: StepRunner,
  pol: CommunicationPolicy,
  m: SourceMention,
  msg: Extract<ThreadInbox, { kind: "session_event" }>,
  st: ThreadRender,
  session: StartedSession,
): Promise<void> {
  const effect = routeSessionEvent(msg.event, st.questionProtocols);
  switch (effect.kind) {
    case "message": {
      if (!effect.text) break;
      // Append to the live bubble unless it would overflow — then roll to a new
      // one. `ref` undefined posts a fresh message; set edits in place.
      const append =
        st.bubble !== null && st.bubble.text.length + effect.text.length + 2 <= MAX_BUBBLE_CHARS;
      const text = append ? `${st.bubble!.text}\n\n${effect.text}` : effect.text;
      const ref = append ? st.bubble!.ts : undefined;
      const ts = await step(() => pol.onAssistantMessage(m, text, ref), "onAssistantMessage");
      st.bubble = { ts, text };
      st.lastAssistantText = effect.text;
      break;
    }
    case "working": {
      st.bubble = null; // a new run — its first response opens a fresh message
      await step(() => pol.onWorking(st.currentMention), "onWorking");
      break;
    }
    case "idle": {
      st.bubble = null;
      await step(() => pol.onIdle(st.currentMention), "onIdle");
      break;
    }
    case "question": {
      st.bubble = null; // the question is its own message
      const ref = await step(() => pol.onUserQuestion(m, msg.event), "onUserQuestion");
      if (effect.toolCallId) {
        st.questionTs.set(effect.toolCallId, ref);
        st.questionProtocols.set(effect.toolCallId, effect.via);
      }
      break;
    }
    case "answered": {
      const ref = effect.toolCallId ? st.questionTs.get(effect.toolCallId) : undefined;
      await step(() => pol.onAnswered(m, msg.event, ref), "onAnswered");
      break;
    }
    case "asset": {
      // The asset is its own message; accumulate durable ones for the closing
      // recap (transient actions summarize to null and are skipped).
      st.bubble = null;
      const recap = summarizeAsset(msg.event);
      if (recap) st.assets.push(recap);
      await step(() => pol.onAsset(m, msg.event, session), "onAsset");
      break;
    }
    case "ignore":
      break;
  }
}

export const slackThreadWorkflow = DBOS.registerWorkflow(slackThreadWorkflowImpl, {
  name: "SlackThreadWorkflow",
});
