/**
 * SlackThreadWorkflow drain-loop error isolation (ADR 0060 P2 + the 2026-06-26
 * "👀 then silence" incident).
 *
 * `handleInbound` is the per-message body of the workflow's recv loop, extracted
 * so its control flow is testable WITHOUT a live DBOS engine (the engine
 * integration is deferred per the ADR): the workflow passes `DBOS.runStep`, a
 * test passes a plain `(fn) => fn()` runner.
 *
 * The invariant under test: a transient failure delivering a follow-up prompt or
 * an answer to the live session (e.g. the coord returns "sandbox not found" on a
 * just-idle session) is surfaced as a NON-FATAL `onDeliveryError` and the loop
 * continues — it must NEVER propagate, because an uncaught throw here used to
 * error the whole workflow, leaving the user with only the 👀.
 */

import { expect, test, describe } from "bun:test";
import {
  closingSummary,
  handleInbound,
  type StepRunner,
  type ThreadControlPlane,
} from "../slack-thread.ts";
import type { CommunicationPolicy, StartedSession } from "../communication-policy.ts";
import type { SourceMention, ThreadInbox } from "../thread-inbox.ts";
import type { CuratedEvent } from "../../control-plane/session-events.ts";

/** Steps run inline — no engine, no checkpoints. */
const STEP: StepRunner = (fn) => fn();

const SESSION: StartedSession = { id: "s1", webUrl: "https://e.dev/sessions/s1" };

const mention = (ts: string, eventId: string): SourceMention => ({
  team: "T1",
  channel: "C1",
  threadRoot: "100.0",
  user: "U1",
  ts,
  eventId,
});

/** A recording CommunicationPolicy. `gatherThreadContext` is overridable so a
 *  test can make the thread read throw. */
function recordingPolicy() {
  const calls: Record<string, unknown[][]> = {
    onPickup: [],
    onDeliveryError: [],
    onNeutralClose: [],
    onWorking: [],
    onIdle: [],
    onAssistantMessage: [],
    onUserQuestion: [],
    onAnswered: [],
    onAsset: [],
    onFail: [],
  };
  let gather: (m: SourceMention, since: string | null) => Promise<{ prompt: string; maxTs: string }> = async (
    _m,
    since,
  ) => ({ prompt: "folded prompt", maxTs: since === null ? "100.0" : "200.0" });

  const pol: CommunicationPolicy = {
    systemPromptAppend: "x",
    onPickup: async (m) => void calls.onPickup.push([m]),
    onStarted: async () => {},
    onWorking: async (m) => void calls.onWorking.push([m]),
    onIdle: async (m) => void calls.onIdle.push([m]),
    onAssistantMessage: async (m, text, ref) => {
      calls.onAssistantMessage.push([m, text, ref]);
      return "bubble-ts";
    },
    onUserQuestion: async (m, ev) => {
      calls.onUserQuestion.push([m, ev]);
      return "q-ts";
    },
    onAnswered: async (m, ev, ref) => void calls.onAnswered.push([m, ev, ref]),
    onAsset: async (m, ev) => void calls.onAsset.push([m, ev]),
    onComplete: async () => {},
    onFail: async (m, message) => void calls.onFail.push([m, message]),
    onDeliveryError: async (m, message) => void calls.onDeliveryError.push([m, message]),
    onNeutralClose: async (m, message) => void calls.onNeutralClose.push([m, message]),
    gatherThreadContext: (m, since) => gather(m, since),
  };
  return {
    pol,
    calls,
    setGather(fn: typeof gather) {
      gather = fn;
    },
  };
}

/** A control plane whose delivery methods are scriptable per test. */
function recordingControlPlane() {
  const calls: { sendPrompt: unknown[][]; completeToolCall: unknown[][] } = {
    sendPrompt: [],
    completeToolCall: [],
  };
  const cp: ThreadControlPlane & { sendPromptImpl: () => Promise<void>; answerImpl: () => Promise<void> } = {
    resolveUser: async () => "u1",
    getDefaultProfile: async () => ({ id: "p1" }),
    createTask: async () => SESSION,
    sendPrompt: async (sessionId, prompt, promptId) => {
      calls.sendPrompt.push([sessionId, prompt, promptId]);
      await cp.sendPromptImpl();
    },
    completeToolCall: async (sessionId, toolCallId, answers) => {
      calls.completeToolCall.push([sessionId, toolCallId, answers]);
      await cp.answerImpl();
    },
    sendPromptImpl: async () => {},
    answerImpl: async () => {},
  };
  return { cp, calls };
}

const freshState = (m: SourceMention) => ({
  questionTs: new Map<string, string>(),
  questionProtocols: new Map<string, "generic" | "legacy">(),
  assets: [],
  bubble: null,
  lastAssistantText: null,
  currentMention: m,
});

describe("handleInbound() — follow-up @mention", () => {
  test("happy path: 👀, gather NEW context, sendPrompt, advance the cursor", async () => {
    const { pol, calls } = recordingPolicy();
    const { cp, calls: cpCalls } = recordingControlPlane();
    const m0 = mention("100.0", "Ev0");
    const st = freshState(m0);
    const msg: ThreadInbox = { kind: "trigger_mention", mention: mention("150.0", "Ev1") };

    const next = await handleInbound(STEP, pol, cp, SESSION, st, "100.0", msg);

    expect(calls.onPickup).toHaveLength(1);
    expect(cpCalls.sendPrompt).toEqual([["s1", "folded prompt", "slack:Ev1"]]);
    expect(next).toBe("200.0"); // cursor advanced to the gathered maxTs
    expect(calls.onDeliveryError).toHaveLength(0);
    expect(st.currentMention.ts).toBe("150.0"); // the new turn drives the lifecycle
  });

  test("sendPrompt failure is NON-FATAL: ⚠️ onDeliveryError, cursor held, no throw", async () => {
    const { pol, calls } = recordingPolicy();
    const { cp, calls: cpCalls } = recordingControlPlane();
    cp.sendPromptImpl = async () => {
      throw new Error("[internal] forward prompt to harness: sandbox not found");
    };
    const m1 = mention("150.0", "Ev1");
    const st = freshState(mention("100.0", "Ev0"));
    const msg: ThreadInbox = { kind: "trigger_mention", mention: m1 };

    // The whole point: this resolves, it does not reject.
    const next = await handleInbound(STEP, pol, cp, SESSION, st, "100.0", msg);

    expect(cpCalls.sendPrompt).toHaveLength(1); // we did attempt delivery
    expect(calls.onPickup).toHaveLength(1); // 👀 still went out
    expect(calls.onDeliveryError).toHaveLength(1);
    expect(calls.onDeliveryError[0][0]).toBe(m1); // ⚠️ on the new mention
    // ADR 0067: the "mention me again to retry" apology is retired —
    // enqueue failures are hard failures (session ended / coord down).
    expect(String(calls.onDeliveryError[0][1])).toContain("session");
    expect(next).toBe("100.0"); // cursor UNCHANGED — undelivered messages re-gather next time
  });

  test("a thread-read (gatherThreadContext) failure is also non-fatal", async () => {
    const { pol, calls, setGather } = recordingPolicy();
    const { cp, calls: cpCalls } = recordingControlPlane();
    setGather(async () => {
      throw new Error("missing_scope");
    });
    const st = freshState(mention("100.0", "Ev0"));
    const msg: ThreadInbox = { kind: "trigger_mention", mention: mention("150.0", "Ev1") };

    const next = await handleInbound(STEP, pol, cp, SESSION, st, "100.0", msg);

    expect(cpCalls.sendPrompt).toHaveLength(0); // never got to delivery
    expect(calls.onDeliveryError).toHaveLength(1);
    expect(next).toBe("100.0");
  });
});

describe("handleInbound() — answer", () => {
  const answerMsg: ThreadInbox = {
    kind: "trigger_answer",
    answer: { toolCallId: "tc", answers: { "Ship?": ["Yes"] } },
  };

  test("a pre-upgrade question cannot be answered and posts a graceful notice", async () => {
    const { pol, calls } = recordingPolicy();
    const { cp, calls: cpCalls } = recordingControlPlane();
    const st = freshState(mention("100.0", "Ev0"));

    const next = await handleInbound(STEP, pol, cp, SESSION, st, "180.0", answerMsg);

    expect(cpCalls.completeToolCall).toHaveLength(0);
    expect(calls.onDeliveryError).toEqual([
      [st.currentMention, "This question predates an upgrade and can no longer be answered."],
    ]);
    expect(next).toBe("180.0");
  });

  test("a generic question answer uses CompleteToolCall", async () => {
    const { pol } = recordingPolicy();
    const { cp, calls: cpCalls } = recordingControlPlane();
    const st = freshState(mention("100.0", "Ev0"));
    await handleInbound(STEP, pol, cp, SESSION, st, "180.0", {
      kind: "session_event",
      event: {
        idx: 1n,
        kind: "tool_call_requested",
        payloadJson: JSON.stringify({
          run_id: "r1",
          tool_call_id: "tc",
          name: "ask_user_question",
          args_json: JSON.stringify({
            questions: [
              {
                question: "Ship?",
                header: "Ship",
                multiSelect: false,
                options: [{ label: "Yes", description: "Deploy" }],
              },
            ],
          }),
        }),
      },
    });

    await handleInbound(STEP, pol, cp, SESSION, st, "180.0", answerMsg);

    expect(cpCalls.completeToolCall).toEqual([["s1", "tc", { "Ship?": ["Yes"] }]]);
  });

});

describe("handleInbound() — session event", () => {
  const ev = (kind: string, payloadJson: string): CuratedEvent => ({ idx: 0n, kind, payloadJson });

  test("an assistant message routes to onAssistantMessage; cursor unchanged", async () => {
    const { pol, calls } = recordingPolicy();
    const { cp } = recordingControlPlane();
    const st = freshState(mention("100.0", "Ev0"));
    const msg: ThreadInbox = {
      kind: "session_event",
      event: ev("agent_message", JSON.stringify({ role: "assistant", text: "hi" })),
    };

    const next = await handleInbound(STEP, pol, cp, SESSION, st, "180.0", msg);

    expect(calls.onAssistantMessage).toHaveLength(1);
    expect(next).toBe("180.0");
  });

  test("a render failure drops the one event WITHOUT a user-facing notice (it's output, not input)", async () => {
    const { pol, calls } = recordingPolicy();
    const { cp } = recordingControlPlane();
    pol.onUserQuestion = async () => {
      throw new Error("slack 500");
    };
    const st = freshState(mention("100.0", "Ev0"));
    const msg: ThreadInbox = {
      kind: "session_event",
      event: ev("user_question", JSON.stringify({ tool_call_id: "tc", questions: [] })),
    };

    const next = await handleInbound(STEP, pol, cp, SESSION, st, "180.0", msg);

    expect(next).toBe("180.0");
    expect(calls.onDeliveryError).toHaveLength(0); // render drops are silent (logged only)
  });

  test("generic request posts a card and its submitted result locks that same card", async () => {
    const { pol, calls } = recordingPolicy();
    const { cp } = recordingControlPlane();
    const st = freshState(mention("100.0", "Ev0"));
    const request = ev(
      "tool_call_requested",
      JSON.stringify({
        run_id: "r1",
        tool_call_id: "tc-generic",
        name: "ask_user_question",
        args_json: JSON.stringify({
          questions: [
            {
              question: "Ship?",
              header: "Ship",
              multiSelect: false,
              options: [{ label: "Yes", description: "Deploy" }],
            },
          ],
        }),
      }),
    );
    await handleInbound(STEP, pol, cp, SESSION, st, "180.0", {
      kind: "session_event",
      event: request,
    });
    const submitted = ev(
      "tool_result_submitted",
      JSON.stringify({
        tool_call_id: "tc-generic",
        result_json: JSON.stringify({ "Ship?": ["Yes"] }),
      }),
    );
    await handleInbound(STEP, pol, cp, SESSION, st, "180.0", {
      kind: "session_event",
      event: submitted,
    });

    expect(calls.onUserQuestion).toEqual([[st.currentMention, request]]);
    expect(calls.onAnswered).toEqual([[st.currentMention, submitted, "q-ts"]]);
    expect(st.questionProtocols.get("tc-generic")).toBe("generic");
  });

  test("legacy user_question and question_answered still post and lock a card", async () => {
    const { pol, calls } = recordingPolicy();
    const { cp } = recordingControlPlane();
    const st = freshState(mention("100.0", "Ev0"));
    const question = ev(
      "user_question",
      JSON.stringify({ tool_call_id: "tc-legacy", questions: [] }),
    );
    const answered = ev(
      "question_answered",
      JSON.stringify({ tool_call_id: "tc-legacy", answers: { "Ship?": ["Yes"] } }),
    );
    await handleInbound(STEP, pol, cp, SESSION, st, "180.0", {
      kind: "session_event",
      event: question,
    });
    await handleInbound(STEP, pol, cp, SESSION, st, "180.0", {
      kind: "session_event",
      event: answered,
    });

    expect(calls.onUserQuestion).toEqual([[st.currentMention, question]]);
    expect(calls.onAnswered).toEqual([[st.currentMention, answered, "q-ts"]]);
    expect(st.questionProtocols.get("tc-legacy")).toBe("legacy");
  });
});

describe("Slack thread ingest-v2 state", () => {
  test("closing summary comes from ThreadRender.lastAssistantText", async () => {
    const { pol } = recordingPolicy();
    const { cp } = recordingControlPlane();
    const st = freshState(mention("100.0", "Ev0"));

    await handleInbound(STEP, pol, cp, SESSION, st, "100.0", {
      kind: "session_event",
      event: {
        idx: 1n,
        kind: "agent_message",
        payloadJson: JSON.stringify({ role: "assistant", text: "first" }),
      },
    });
    await handleInbound(STEP, pol, cp, SESSION, st, "100.0", {
      kind: "session_event",
      event: {
        idx: 2n,
        kind: "agent_message",
        payloadJson: JSON.stringify({ role: "assistant", text: "final answer" }),
      },
    });

    expect(closingSummary(st)).toEqual({
      lastMessage: "final answer",
      assets: [],
    });
  });
});
