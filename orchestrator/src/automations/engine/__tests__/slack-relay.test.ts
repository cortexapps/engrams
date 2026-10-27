import { describe, expect, test } from "bun:test";

import type { CuratedEvent } from "../../../control-plane/session-events.ts";
import type { CommunicationPolicy } from "../../../workflows/communication-policy.ts";
import {
  MAX_BUBBLE_CHARS,
  closeSessionRelay,
  deliverSessionAnswer,
  installSessionRelay,
  relayFactsOf,
  renderSessionEvent,
  type SessionRelayDeps,
} from "../../session-relay.ts";
import { registerEngineBlocks } from "../blocks/index.ts";
import { getBlock } from "../blocks/registry.ts";
import { RELAY_SESSION_TYPE, type SlackRelayConfig } from "../blocks/relay.ts";
import { RELAY_CLOSE_TYPE } from "../blocks/relay-close.ts";
import { setSlackPolicyFactoryForTest } from "../../../integrations/slack-policy.ts";
import { buildRunContext, type RunSnapshot } from "../context.ts";
import type { EngineDeps, EngineRunStore, EngineSessionOps } from "../deps.ts";
import { makeFakeRelayStore } from "./fake-relay-store.ts";

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
    onComplete: [],
    onFail: [],
    onNeutralClose: [],
    onDeliveryError: [],
  };
  let bubbleN = 0;
  let throwOnMessage = false;
  const pol: CommunicationPolicy = {
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
    onChoice: async () => "c-ts",
    onAnswered: async (m, e, ref) => void calls.onAnswered.push([m, e, ref]),
    onAsset: async (m, e) => void calls.onAsset.push([m, e]),
    onComplete: async (m, s, summary) => void calls.onComplete.push([m, s, summary]),
    onFail: async (m, message) => void calls.onFail.push([m, message]),
    onDeliveryError: async (m, message) => void calls.onDeliveryError.push([m, message]),
    onNeutralClose: async (m, message) => void calls.onNeutralClose.push([m, message]),
  };
  return {
    pol,
    calls,
    setThrow(v: boolean) {
      throwOnMessage = v;
    },
  };
}

const SESSION = "s-launch";
const MENTION = { team: "T1", channel: "C1", threadRoot: "100.0", user: "U1", ts: "100.0", eventId: "E1" };

function harness() {
  const policy = recordingPolicy();
  const store = makeFakeRelayStore();
  const completed: Array<{ sessionId: string; toolCallId: string; result: unknown }> = [];
  let failComplete = false;
  const deps: SessionRelayDeps = {
    store,
    policy: () => policy.pol,
    completeToolCall: async (sessionId, toolCallId, result) => {
      if (failComplete) throw new Error("session gone");
      completed.push({ sessionId, toolCallId, result });
    },
    sessionWebUrl: (id) => `https://x/sessions/${id}`,
  };
  const install = (target = MENTION) =>
    installSessionRelay({ sessionId: SESSION, automationId: "auto-1", target }, deps);
  const render = (event: CuratedEvent) => renderSessionEvent(SESSION, event, deps);
  return {
    policy,
    store,
    deps,
    completed,
    install,
    render,
    setFailComplete(v: boolean) {
      failComplete = v;
    },
  };
}

describe("session relay (contract 5)", () => {
  test("install creates the row with a fresh render state; the same mention again is a no-op", async () => {
    const h = harness();
    expect(await h.install()).toEqual({ created: true });
    const row = h.store.rows.get(SESSION)!;
    expect(row.provider).toBe("slack");
    expect(row.target).toMatchObject({ channel: "C1", ts: "100.0" });
    expect(row.state).toMatchObject({ bubble: null, lastAssistantText: null, assets: [] });
    expect(await h.install()).toEqual({ created: false });
    expect(h.store.writes).toEqual(["install"]);
  });

  test("a render with no relay says so and posts nothing", async () => {
    const h = harness();
    expect(await h.render(ev("agent_message", { role: "assistant", text: "one" }))).toBe("no_relay");
    expect(h.policy.calls.onAssistantMessage).toEqual([]);
  });

  test("assistant messages coalesce into one bubble across renders and roll past the cap", async () => {
    const h = harness();
    await h.install();
    await h.render(ev("run_started", {}));
    await h.render(ev("agent_message", { role: "assistant", text: "one" }));
    await h.render(ev("agent_message", { role: "assistant", text: "two" }));
    // The second edits the first bubble in place with the joined text: the
    // bubble rode the row between the two renders.
    expect(h.policy.calls.onAssistantMessage).toEqual([
      [expect.anything(), "one", undefined],
      [expect.anything(), "one\n\ntwo", "bubble-1"],
    ]);
    // A user-role echo never posts.
    await h.render(ev("agent_message", { role: "user", text: "echo" }));
    expect(h.policy.calls.onAssistantMessage).toHaveLength(2);
    // Past the cap, a fresh bubble opens.
    const huge = "x".repeat(MAX_BUBBLE_CHARS);
    await h.render(ev("agent_message", { role: "assistant", text: huge }));
    expect(h.policy.calls.onAssistantMessage.at(-1)![2]).toBeUndefined();
    expect(h.store.rows.get(SESSION)!.version).toBe(6);
  });

  test("run lifecycle reacts on the mention and seals the bubble", async () => {
    const h = harness();
    await h.install();
    expect(await h.render(ev("run_started", {}))).toBe("rendered");
    expect(await h.render(ev("run_completed", { ok: true }))).toBe("rendered");
    expect(h.policy.calls.onWorking).toHaveLength(1);
    expect(h.policy.calls.onIdle).toHaveLength(1);
    expect((h.policy.calls.onIdle[0]![0] as { ts: string }).ts).toBe("100.0");
  });

  test("a generic AskUserQuestion posts a card, and its answer completes the tool call", async () => {
    const h = harness();
    await h.install();
    await h.render(
      ev("tool_call_requested", {
        tool_call_id: "tc-1",
        name: "ask_user_question",
        args_json: JSON.stringify({
          questions: [{ question: "Proceed?", header: "Go", options: [{ label: "Yes", description: "go" }], multiSelect: false }],
        }),
      }),
    );
    expect(h.policy.calls.onUserQuestion).toHaveLength(1);

    const result = await deliverSessionAnswer(
      SESSION,
      { toolCallId: "tc-1", answers: { "Proceed?": ["Yes"] } },
      h.deps,
    );
    expect(result).toBe("delivered");
    expect(h.completed).toEqual([{ sessionId: SESSION, toolCallId: "tc-1", result: { "Proceed?": ["Yes"] } }]);
    // The session's own "answered" event updates the card by its ts.
    await h.render(
      ev("tool_result_submitted", { tool_call_id: "tc-1", result_json: JSON.stringify({ "Proceed?": ["Yes"] }) }),
    );
    expect(h.policy.calls.onAnswered[0]![2]).toBe("q-ts");
  });

  test("an answer to a legacy-protocol or unknown question posts the legacy notice", async () => {
    const h = harness();
    await h.install();
    const result = await deliverSessionAnswer(SESSION, { toolCallId: "never-posted", answers: {} }, h.deps);
    expect(result).toBe("legacy");
    expect(h.completed).toEqual([]);
    expect(h.policy.calls.onDeliveryError[0]![1]).toMatch(/predates an upgrade/);
  });

  test("an answer for a session with no relay is reported, not posted", async () => {
    const h = harness();
    expect(await deliverSessionAnswer(SESSION, { toolCallId: "tc-1", answers: {} }, h.deps)).toBe("no_relay");
    expect(h.policy.calls.onDeliveryError).toEqual([]);
  });

  test("a failed tool completion keeps the thread alive with a ⚠️ note", async () => {
    const h = harness();
    await h.install();
    await h.render(
      ev("tool_call_requested", {
        tool_call_id: "tc-2",
        name: "ask_user_question",
        args_json: JSON.stringify({
          questions: [{ question: "Q", header: "H", options: [{ label: "A", description: "a" }], multiSelect: false }],
        }),
      }),
    );
    h.setFailComplete(true);
    expect(await deliverSessionAnswer(SESSION, { toolCallId: "tc-2", answers: { Q: ["A"] } }, h.deps)).toBe("failed");
    expect(h.policy.calls.onDeliveryError[0]![1]).toMatch(/couldn't record that answer/);
  });

  test("assets post their own message and accumulate into the closing recap", async () => {
    const h = harness();
    await h.install();
    await h.render(ev("agent_message", { role: "assistant", text: "done" }));
    await h.render(
      ev("integration_asset", { provider: "github", kind: "pull_request", url: "https://github.com/a/b/pull/1", title: "Fix it" }),
    );
    expect(h.policy.calls.onAsset).toHaveLength(1);
    const facts = relayFactsOf(h.store.rows.get(SESSION)!, h.deps);
    expect(facts.summary.lastMessage).toBe("done");
    expect(facts.summary.assets.length).toBe(1);
    expect(facts.session.webUrl).toBe(`https://x/sessions/${SESSION}`);
  });

  test("a render failure is dropped, never thrown, and the event still counts as rendered", async () => {
    const h = harness();
    await h.install();
    h.policy.setThrow(true);
    expect(await h.render(ev("agent_message", { role: "assistant", text: "boom" }))).toBe("rendered");
  });

  test("a render that loses the CAS race re-reads and writes again", async () => {
    const h = harness();
    await h.install();
    const update = h.store.update.bind(h.store);
    let raced = false;
    h.store.update = async (sessionId, expectVersion, patch) => {
      if (!raced) {
        // Someone else advanced the row between our read and our write.
        raced = true;
        await update(sessionId, expectVersion, { state: { ...h.store.rows.get(sessionId)!.state } });
      }
      return update(sessionId, expectVersion, patch);
    };
    expect(await h.render(ev("agent_message", { role: "assistant", text: "one" }))).toBe("rendered");
    expect(h.store.rows.get(SESSION)!.state["lastAssistantText"]).toBe("one");
    expect(h.store.rows.get(SESSION)!.version).toBe(3);
  });

  test("a re-point keeps the conversation and opens a fresh bubble under the new mention", async () => {
    const h = harness();
    await h.install();
    await h.render(ev("agent_message", { role: "assistant", text: "one" }));
    expect(await h.install({ ...MENTION, ts: "200.0", eventId: "E2" })).toEqual({ created: false });
    await h.render(ev("agent_message", { role: "assistant", text: "two" }));
    // Fresh bubble (no ref) after the re-point, reacting on the new mention.
    expect(h.policy.calls.onAssistantMessage[1]![2]).toBeUndefined();
    await h.render(ev("run_completed", { ok: true }));
    expect((h.policy.calls.onIdle[0]![0] as { ts: string }).ts).toBe("200.0");
    expect(h.store.rows.get(SESSION)!.state["lastAssistantText"]).toBe("two");
    expect(h.store.writes).toEqual(["install", "render", "repoint", "render", "render"]);
  });

  test("the session's end posts the closing message for its outcome and removes the relay", async () => {
    const h = harness();
    await h.install();
    await h.render(ev("agent_message", { role: "assistant", text: "all done" }));
    expect(await closeSessionRelay(SESSION, "completed", h.deps)).toBe("closed");
    expect(h.policy.calls.onComplete[0]![2]).toMatchObject({ lastMessage: "all done" });
    expect(h.store.rows.has(SESSION)).toBe(false);
    expect(await closeSessionRelay(SESSION, "completed", h.deps)).toBe("no_relay");

    await h.install();
    await closeSessionRelay(SESSION, "failed", h.deps);
    expect(h.policy.calls.onFail).toHaveLength(1);
    // A neutral end (the sandbox reclaimed, a kept session swept) is
    // silent: the relay goes, the thread hears nothing.
    await h.install();
    expect(await closeSessionRelay(SESSION, "neutral", h.deps)).toBe("closed");
    expect(h.policy.calls.onNeutralClose).toEqual([]);
    expect(h.store.rows.size).toBe(0);
  });
});

// ---------------------------------------------------------------------------
// The blocks over the store
// ---------------------------------------------------------------------------

const RUN_ID = "autorun:auto-1:slack:E1";
const CONFIG: SlackRelayConfig = {
  session: { blockId: "launch" },
  provider: "slack",
  team: "T1",
  channel: "C1",
  threadTs: "100.0",
  mentionTs: "100.0",
  userId: "U1",
  eventId: "E1",
};

function blockHarness(options: { relays?: boolean; dryRun?: boolean } = {}) {
  const store = makeFakeRelayStore();
  const sessions: EngineSessionOps = {
    createSession: async () => ({ sessionId: SESSION, taskId: "t" }),
    sendPrompt: async () => {},
    endSession: async () => {},
    exec: async () => ({ exitStatus: 0, stdout: "", stderr: "" }),
    writeFiles: async () => [],
    getSession: async () => ({ found: false }),
  };
  const runStore: EngineRunStore = {
    loadSnapshot: () => Promise.reject(new Error("unused")),
    markRunning: async () => {},
    recordStep: async () => {},
    finalizeRun: async () => {},
    latestKeptInstanceSession: async () => null,
    listRunSessions: async () => [{ sessionId: SESSION, keep: true }],
    releaseConcurrency: async () => null,
    adoptSession: async () => "already_ours",
    getSessionBinding: async () => null,
  };
  const deps: EngineDeps = {
    step: async (fn) => fn(),
    recv: async () => null,
    store: runStore,
    sessions,
    clock: { nowMs: () => 0 },
    ...(options.relays === false ? {} : { relays: store }),
  };
  const snapshot: RunSnapshot = {
    definition: { engine: 1, trigger: { kind: "manual" }, blocks: [], inputsSchema: [], settings: { endSessionsOnFinish: false } },
    inputs: {},
    automationId: "auto-1",
    automationName: "Slack relay test",
    version: 1,
    trigger: { kind: "manual", receivedAt: "2026-08-22T00:00:00Z" },
    aliases: [],
    startedAtMs: 0,
    ...(options.dryRun ? { dryRun: true } : {}),
  };
  const ctx = buildRunContext(RUN_ID, snapshot, deps);
  ctx.steps["launch"] = { session_id: SESSION };
  ctx.currentBlockId = "relay";
  return { store, ctx };
}

describe("relay_session block", () => {
  test("installs the relay for the resolved session inside the run", async () => {
    const h = blockHarness();
    const block = getBlock(RELAY_SESSION_TYPE)!;
    const out = await block.execute!(CONFIG as never, h.ctx);
    expect(out).toEqual({
      kind: "ok",
      outputs: { session_id: SESSION, installed: true, created: true, mention_ts: "100.0" },
    });
    expect(h.store.rows.get(SESSION)!.target).toMatchObject({ threadRoot: "100.0", user: "U1", eventId: "E1" });
    // A second execution with a new mention is a re-point, not a new row.
    const again = await block.execute!({ ...CONFIG, mentionTs: "200.0", eventId: "E2" } as never, h.ctx);
    expect(again).toMatchObject({ kind: "ok", outputs: { installed: true, created: false, mention_ts: "200.0" } });
    expect(h.store.rows.size).toBe(1);
  });

  test("a dry run installs nothing and reports what it would target", async () => {
    const h = blockHarness({ dryRun: true });
    const out = await getBlock(RELAY_SESSION_TYPE)!.execute!(CONFIG as never, h.ctx);
    expect(out).toMatchObject({ kind: "ok", outputs: { installed: false, dry_run: true } });
    expect(h.store.rows.size).toBe(0);
  });

  test("without a relay store the block fails plainly", async () => {
    const h = blockHarness({ relays: false });
    const out = await getBlock(RELAY_SESSION_TYPE)!.execute!(CONFIG as never, h.ctx);
    expect(out).toMatchObject({ kind: "error", code: "relay_store_unavailable" });
  });
});

describe("relay_close block", () => {
  test("posts the closing message for the run's status and removes the relay", async () => {
    const h = blockHarness();
    const policy = recordingPolicy();
    setSlackPolicyFactoryForTest(() => policy.pol);
    try {
      await getBlock(RELAY_SESSION_TYPE)!.execute!(CONFIG as never, h.ctx);
      const close = getBlock(RELAY_CLOSE_TYPE)!;
      const out = await close.execute!({ session: { blockId: "launch" }, status: "completed" } as never, h.ctx);
      expect(out).toEqual({ kind: "ok", outputs: { posted: true, rendered_as: "complete" } });
      expect(policy.calls.onComplete).toHaveLength(1);
      expect(h.store.rows.size).toBe(0);
      // Nothing to close a second time.
      const none = await close.execute!({ session: { blockId: "launch" }, status: "failed" } as never, h.ctx);
      expect(none).toEqual({ kind: "ok", outputs: { posted: false, rendered_as: "none" } });
      expect(policy.calls.onFail).toHaveLength(0);
    } finally {
      setSlackPolicyFactoryForTest(null);
    }
  });

  test("failed and neutral statuses pick their message; a template session ref works too", async () => {
    const h = blockHarness();
    const policy = recordingPolicy();
    setSlackPolicyFactoryForTest(() => policy.pol);
    try {
      const close = getBlock(RELAY_CLOSE_TYPE)!;
      await getBlock(RELAY_SESSION_TYPE)!.execute!(CONFIG as never, h.ctx);
      await close.execute!({ session: { template: SESSION }, status: "failed", message: "boom" } as never, h.ctx);
      expect(policy.calls.onFail[0]![1]).toBe("boom");
      await getBlock(RELAY_SESSION_TYPE)!.execute!(CONFIG as never, h.ctx);
      const out = await close.execute!({ session: { template: SESSION }, status: "halted" } as never, h.ctx);
      expect(out).toMatchObject({ outputs: { rendered_as: "neutral" } });
      expect(policy.calls.onNeutralClose[0]![1]).toMatch(/halted/);
    } finally {
      setSlackPolicyFactoryForTest(null);
    }
  });
});
