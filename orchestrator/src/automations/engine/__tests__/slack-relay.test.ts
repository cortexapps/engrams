import { afterEach, beforeEach, describe, expect, test } from "bun:test";

import type { CuratedEvent } from "../../../control-plane/session-events.ts";
import type { CommunicationPolicy } from "../../../workflows/communication-policy.ts";
import { registerEngineBlocks } from "../blocks/index.ts";
import { getBlock } from "../blocks/registry.ts";
import {
  MAX_BUBBLE_CHARS,
  SLACK_ANSWER_SIGNAL,
  resetSlackRelayStateForTest,
  setSlackRelayDeps,
  slackRelayClosingSummary,
  type SlackRelayConfig,
} from "../blocks/system/slack-relay.ts";
import { buildRunContext, type RunSnapshot } from "../context.ts";
import type { EngineDeps, EngineRunStore, EngineSessionOps } from "../deps.ts";
import type { AutomationInbox } from "../inbox.ts";

registerEngineBlocks();

const ev = (kind: string, payload: unknown, idx = 0n): CuratedEvent => ({
  idx,
  kind,
  payloadJson: JSON.stringify(payload),
});

function recordingPolicy() {
  const calls: Record<string, unknown[][]> = {
    onWorking: [],
    onIdle: [],
    onAssistantMessage: [],
    onUserQuestion: [],
    onAnswered: [],
    onAsset: [],
    onDeliveryError: [],
  };
  let bubbleN = 0;
  let throwOnMessage = false;
  const pol: CommunicationPolicy = {
    systemPromptAppend: "x",
    onPickup: async () => {},
    onProfileChoice: async () => "p",
    onProfileChosen: async () => {},
    onStarted: async () => {},
    onWorking: async (m) => void calls.onWorking.push([m]),
    onIdle: async (m) => void calls.onIdle.push([m]),
    onAssistantMessage: async (m, text, ref) => {
      if (throwOnMessage) throw new Error("slack down");
      calls.onAssistantMessage.push([m, text, ref]);
      return ref ?? `bubble-${++bubbleN}`;
    },
    onUserQuestion: async (m, e) => {
      calls.onUserQuestion.push([m, e]);
      return "q-ts";
    },
    onAnswered: async (m, e, ref) => void calls.onAnswered.push([m, e, ref]),
    onAsset: async (m, e) => void calls.onAsset.push([m, e]),
    onComplete: async () => {},
    onFail: async () => {},
    onDeliveryError: async (m, message) => void calls.onDeliveryError.push([m, message]),
    onNeutralClose: async () => {},
    gatherThreadContext: async () => ({ prompt: "", maxTs: "0" }),
  };
  return {
    pol,
    calls,
    setThrow(v: boolean) {
      throwOnMessage = v;
    },
  };
}

const RUN_ID = "autorun:auto-1:slack:E1";
const CONFIG: SlackRelayConfig = {
  session: { blockId: "launch" },
  team: "T1",
  channel: "C1",
  threadTs: "100.0",
  mentionTs: "100.0",
  userId: "U1",
  eventId: "E1",
};

function harness() {
  const policy = recordingPolicy();
  const completed: Array<{ sessionId: string; toolCallId: string; result: unknown }> = [];
  const relayFlags: Array<{ sessionId: string; relay: boolean }> = [];
  let failComplete = false;
  setSlackRelayDeps({
    policy: () => policy.pol,
    completeToolCall: async (sessionId, toolCallId, result) => {
      if (failComplete) throw new Error("session gone");
      completed.push({ sessionId, toolCallId, result });
    },
    sessionWebUrl: (id) => `https://x/sessions/${id}`,
  });
  const sessions: EngineSessionOps = {
    createSession: async () => ({ sessionId: "s-launch", taskId: "t" }),
    setSessionRelay: async (sessionId, relay) => {
      relayFlags.push({ sessionId, relay });
    },
    sendPrompt: async () => {},
    endSession: async () => {},
    exec: async () => ({ exitStatus: 0, stdout: "", stderr: "" }),
    writeFiles: async () => [],
  };
  const store: EngineRunStore = {
    loadSnapshot: () => Promise.reject(new Error("unused")),
    markRunning: async () => {},
    recordStep: async () => {},
    finalizeRun: async () => {},
    listRunSessions: async () => [],
    releaseConcurrency: async () => null,
  };
  const deps: EngineDeps = {
    step: async (fn) => fn(),
    recv: async () => null,
    store,
    sessions,
    clock: { nowMs: () => 0 },
  };
  // A real RunContext over a minimal snapshot: the relay resolves its session
  // from the `launch` step's checkpointed output, exactly as in a run.
  const snapshot: RunSnapshot = {
    definition: {
      engine: 1,
      trigger: { kind: "manual" },
      blocks: [],
      inputsSchema: [],
      settings: { endSessionsOnFinish: false },
    },
    inputs: {},
    automationId: "auto-1",
    automationName: "Slack relay test",
    version: 1,
    trigger: { kind: "manual", receivedAt: "2026-08-22T00:00:00Z" },
    aliases: [],
    startedAtMs: 0,
  };
  const ctx = buildRunContext(RUN_ID, snapshot, deps);
  ctx.steps["launch"] = { session_id: "s-launch" };
  ctx.currentBlockId = "relay";
  const block = getBlock("system.slack_thread_relay")!;
  const send = (msg: AutomationInbox) => block.onMessage!(msg, CONFIG as never, ctx);
  return {
    policy,
    completed,
    relayFlags,
    ctx,
    block,
    send,
    setFailComplete(v: boolean) {
      failComplete = v;
    },
  };
}

const sessionEvent = (event: CuratedEvent): AutomationInbox => ({
  kind: "session_event",
  sessionId: "s-launch",
  event,
});

describe("system.slack_thread_relay", () => {
  beforeEach(() => resetSlackRelayStateForTest());
  afterEach(() => setSlackRelayDeps(null));

  test("install flips the session's relay flag and returns the session id", async () => {
    const h = harness();
    const out = await h.block.execute!(CONFIG as never, h.ctx);
    expect(out).toEqual({ kind: "ok", outputs: { session_id: "s-launch", installed: true } });
    expect(h.relayFlags).toEqual([{ sessionId: "s-launch", relay: true }]);
  });

  test("assistant messages coalesce into one bubble and roll past the cap", async () => {
    const h = harness();
    await h.block.execute!(CONFIG as never, h.ctx);
    await h.send(sessionEvent(ev("run_started", {})));
    await h.send(sessionEvent(ev("agent_message", { role: "assistant", text: "one" })));
    await h.send(sessionEvent(ev("agent_message", { role: "assistant", text: "two" })));
    // The second edits the first bubble in place with the joined text.
    expect(h.policy.calls.onAssistantMessage).toEqual([
      [expect.anything(), "one", undefined],
      [expect.anything(), "one\n\ntwo", "bubble-1"],
    ]);
    // A user-role echo never posts.
    await h.send(sessionEvent(ev("agent_message", { role: "user", text: "echo" })));
    expect(h.policy.calls.onAssistantMessage).toHaveLength(2);
    // Past the cap, a fresh bubble opens.
    const huge = "x".repeat(MAX_BUBBLE_CHARS);
    await h.send(sessionEvent(ev("agent_message", { role: "assistant", text: huge })));
    expect(h.policy.calls.onAssistantMessage.at(-1)![2]).toBeUndefined();
  });

  test("run lifecycle reacts on the mention and seals the bubble", async () => {
    const h = harness();
    await h.block.execute!(CONFIG as never, h.ctx);
    expect(await h.send(sessionEvent(ev("run_started", {})))).toBe("consumed");
    expect(await h.send(sessionEvent(ev("run_completed", { ok: true })))).toBe("consumed");
    expect(h.policy.calls.onWorking).toHaveLength(1);
    expect(h.policy.calls.onIdle).toHaveLength(1);
    expect((h.policy.calls.onIdle[0]![0] as { ts: string }).ts).toBe("100.0");
  });

  test("a generic AskUserQuestion posts a card, and its answer completes the tool call", async () => {
    const h = harness();
    await h.block.execute!(CONFIG as never, h.ctx);
    const request = ev("tool_call_requested", {
      tool_call_id: "tc-1",
      name: "ask_user_question",
      args_json: JSON.stringify({
        questions: [{ question: "Proceed?", header: "Go", options: [{ label: "Yes", description: "go" }], multiSelect: false }],
      }),
    });
    await h.send(sessionEvent(request));
    expect(h.policy.calls.onUserQuestion).toHaveLength(1);

    const verdict = await h.send({
      kind: "signal",
      name: SLACK_ANSWER_SIGNAL,
      payload: { toolCallId: "tc-1", answers: { "Proceed?": ["Yes"] } },
    });
    expect(verdict).toBe("consumed");
    expect(h.completed).toEqual([
      { sessionId: "s-launch", toolCallId: "tc-1", result: { "Proceed?": ["Yes"] } },
    ]);
    // The session's own "answered" event updates the card by its ts.
    await h.send(
      sessionEvent(
        ev("tool_result_submitted", {
          tool_call_id: "tc-1",
          result_json: JSON.stringify({ "Proceed?": ["Yes"] }),
        }),
      ),
    );
    expect(h.policy.calls.onAnswered[0]![2]).toBe("q-ts");
  });

  test("an answer to a legacy-protocol or unknown question posts the legacy notice", async () => {
    const h = harness();
    await h.block.execute!(CONFIG as never, h.ctx);
    await h.send({
      kind: "signal",
      name: SLACK_ANSWER_SIGNAL,
      payload: { toolCallId: "never-posted", answers: {} },
    });
    expect(h.completed).toEqual([]);
    expect(h.policy.calls.onDeliveryError[0]![1]).toMatch(/predates an upgrade/);
  });

  test("a failed tool completion keeps the thread alive with a ⚠️ note", async () => {
    const h = harness();
    await h.block.execute!(CONFIG as never, h.ctx);
    await h.send(
      sessionEvent(
        ev("tool_call_requested", {
          tool_call_id: "tc-2",
          name: "ask_user_question",
          args_json: JSON.stringify({
            questions: [{ question: "Q", header: "H", options: [{ label: "A", description: "a" }], multiSelect: false }],
          }),
        }),
      ),
    );
    h.setFailComplete(true);
    const verdict = await h.send({
      kind: "signal",
      name: SLACK_ANSWER_SIGNAL,
      payload: { toolCallId: "tc-2", answers: { Q: ["A"] } },
    });
    expect(verdict).toBe("consumed");
    expect(h.policy.calls.onDeliveryError[0]![1]).toMatch(/couldn't record that answer/);
  });

  test("assets post their own message and accumulate into the closing recap", async () => {
    const h = harness();
    await h.block.execute!(CONFIG as never, h.ctx);
    await h.send(sessionEvent(ev("agent_message", { role: "assistant", text: "done" })));
    await h.send(
      sessionEvent(
        ev("integration_asset", {
          provider: "github",
          kind: "pull_request",
          url: "https://github.com/a/b/pull/1",
          title: "Fix it",
        }),
      ),
    );
    expect(h.policy.calls.onAsset).toHaveLength(1);
    const summary = slackRelayClosingSummary(RUN_ID);
    expect(summary?.lastMessage).toBe("done");
    expect(summary?.assets.length).toBe(1);
  });

  test("a render failure is dropped, never thrown, and the message still counts as consumed", async () => {
    const h = harness();
    await h.block.execute!(CONFIG as never, h.ctx);
    h.policy.setThrow(true);
    const verdict = await h.send(sessionEvent(ev("agent_message", { role: "assistant", text: "boom" })));
    expect(verdict).toBe("consumed");
  });

  test("messages for other sessions and non-relay signals pass through", async () => {
    const h = harness();
    await h.block.execute!(CONFIG as never, h.ctx);
    expect(
      await h.send({ kind: "session_event", sessionId: "s-other", event: ev("agent_message", {}) }),
    ).toBe("pass");
    expect(await h.send({ kind: "session_idle", sessionId: "s-launch" })).toBe("pass");
    expect(await h.send({ kind: "signal", name: "finder_done", sessionId: "s-launch" })).toBe("pass");
  });

  test("re-executing the block re-points the turn's mention and opens a fresh bubble", async () => {
    const h = harness();
    await h.block.execute!(CONFIG as never, h.ctx);
    await h.send(sessionEvent(ev("agent_message", { role: "assistant", text: "one" })));
    await h.block.execute!({ ...CONFIG, mentionTs: "200.0", eventId: "E2" } as never, h.ctx);
    await h.send(sessionEvent(ev("agent_message", { role: "assistant", text: "two" })));
    // Fresh bubble (no ref) after the re-point, reacting on the new mention.
    expect(h.policy.calls.onAssistantMessage[1]![2]).toBeUndefined();
    await h.send(sessionEvent(ev("run_completed", { ok: true })));
    expect((h.policy.calls.onIdle[0]![0] as { ts: string }).ts).toBe("200.0");
    // Only one relay flip: a re-point is not a second install.
    expect(h.relayFlags).toHaveLength(1);
  });
});
