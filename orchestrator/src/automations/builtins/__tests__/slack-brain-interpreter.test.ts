/** The Slack thread brain driven through the REAL interpreter (ADR 0119 phase
 * 4.6): scripted recv, fake session ops, a recording relay policy. These
 * cases are the Slack parity checklist the window is judged against.
 */

import { afterEach, describe, expect, test } from "bun:test";

import type { CuratedEvent } from "../../../control-plane/session-events.ts";
import { makeCodeBlockRuntime } from "../../code/runtime.ts";
import { makeReplayRunner, type ReplayRunner, type ReplayScript } from "../../engine/__tests__/replay-step.ts";
import { registerEngineBlocks } from "../../engine/blocks/index.ts";
import {
  setSlackRelayDeps,
} from "../../engine/blocks/system/slack-relay.ts";
import type { RunSnapshot } from "../../engine/context.ts";
import type { EngineDeps, EngineSessionOps, EngineStepRecord } from "../../engine/deps.ts";
import type { AutomationInbox } from "../../engine/inbox.ts";
import { interpretAutomation } from "../../engine/interpreter.ts";
import type { CommunicationPolicy } from "../../../workflows/communication-policy.ts";
import { SLACK_BRAIN_DEFINITION } from "../slack-brain.ts";

registerEngineBlocks();

const RUN = { runId: "autorun:slack-1:slack:Ev1", automationId: "slack-1" };

function mentionPayload(text = "<@UBOT> summarize the incident"): Record<string, unknown> {
  return {
    team_id: "T1",
    event_id: "Ev1",
    event: { type: "app_mention", channel: "C1", user: "U1", ts: "100.1", text },
  };
}

function replyPayload(text: string, ts: string): Record<string, unknown> {
  return {
    team_id: "T1",
    event_id: `Ev-${ts}`,
    event: { type: "message", channel: "C1", user: "U2", ts, thread_ts: "100.1", text },
  };
}

/** A joined follow-up delivered into the run's mailbox (policy: join). */
function joined(text: string, ts: string): AutomationInbox {
  return {
    kind: "event",
    eventKey: "message",
    deliveryKey: `slack:Ev-${ts}`,
    payload: replyPayload(text, ts),
    receivedAt: "2026-08-22T10:00:05Z",
  };
}

/** A curated session event as the automation consumer forwards it. */
function curated(n: number, kind: string, payload: unknown): AutomationInbox {
  const event: CuratedEvent = { idx: BigInt(n), kind, payloadJson: JSON.stringify(payload) };
  return { kind: "session_event", sessionId: "s-1", event };
}

interface Harness {
  deps: EngineDeps;
  /** Set when the harness was built with `replay`. */
  runner: ReplayRunner | null;
  names: string[];
  records: Array<{ path: string; attempt: number; record: EngineStepRecord }>;
  sessions: Array<{ id: string; profileId: string; prompt: string; keep: boolean; title: string | null }>;
  prompts: Array<{ sessionId: string; text: string }>;
  relayFlags: Array<{ sessionId: string; relay: boolean }>;
  policyCalls: string[];
  ended: string[];
  finalized: Array<{ status: string; error?: string }>;
}

function harness(options: {
  eventKey?: string;
  payload?: Record<string, unknown>;
  inputs?: Record<string, unknown>;
  /** null = a recv timeout (the wait's deadline). */
  recv?: Array<AutomationInbox | null>;
  /** Drive step + recv through the DBOS-recovery simulator instead
   * (`"crash"` entries kill the pod; `runner.restart()` brings it back). */
  replay?: ReplayScript;
}): Harness {
  const runner = options.replay ? makeReplayRunner(options.replay) : null;
  const names: string[] = runner ? runner.names : [];
  const records: Harness["records"] = [];
  const sessions: Harness["sessions"] = [];
  const prompts: Harness["prompts"] = [];
  const relayFlags: Harness["relayFlags"] = [];
  const policyCalls: string[] = [];
  const ended: string[] = [];
  const finalized: Harness["finalized"] = [];
  const recvQueue = [...(options.recv ?? [])];
  const runSessions: Array<{ sessionId: string; keep: boolean }> = [];
  let clock = 1_000_000;

  const policy: CommunicationPolicy = {
    systemPromptAppend: "",
    async onPickup(m) { policyCalls.push(`pickup:${m.ts}`); },
    async onProfileChoice() { return "p1"; },
    async onProfileChosen() {},
    async onStarted() { policyCalls.push("started"); },
    async onWorking() { policyCalls.push("working"); },
    async onIdle() { policyCalls.push("idle"); },
    async onAssistantMessage(_m, text) { policyCalls.push(`msg:${text}`); return "b1"; },
    async onUserQuestion() { policyCalls.push("question"); return "q1"; },
    async onAnswered() { policyCalls.push("answered"); },
    async onAsset() { policyCalls.push("asset"); },
    async onComplete(_m, _s, summary) { policyCalls.push(`complete:${summary.lastMessage ?? ""}`); },
    async onFail(_m, message) { policyCalls.push(`fail:${message}`); },
    async onNeutralClose(_m, message) { policyCalls.push(`neutral:${message}`); },
    async onDeliveryError() { policyCalls.push("delivery-error"); },
    async gatherThreadContext() { return { prompt: "", maxTs: "0" }; },
  };
  setSlackRelayDeps({
    policy: () => policy,
    completeToolCall: async () => {},
    sessionWebUrl: (id) => `https://engrams.test/sessions/${id}`,
  });

  const snapshot: RunSnapshot = {
    definition: {
      ...SLACK_BRAIN_DEFINITION,
      trigger:
        SLACK_BRAIN_DEFINITION.trigger.kind === "integration"
          ? { ...SLACK_BRAIN_DEFINITION.trigger, connectionId: "conn-slack" }
          : SLACK_BRAIN_DEFINITION.trigger,
    },
    inputs: options.inputs ?? { channels: { C1: "prof-a" }, default_profile: "", idle_timeout: 600, max_turns: 5 },
    automationId: RUN.automationId,
    automationName: "Slack thread brain",
    version: 1,
    trigger: {
      kind: "integration",
      receivedAt: "2026-08-22T10:00:00Z",
      eventKey: options.eventKey ?? "app_mention",
      deliveryKey: "slack:Ev1",
      payload: options.payload ?? mentionPayload(),
    },
    aliases: [],
    startedAtMs: clock,
  };

  const sessionOps: EngineSessionOps = {
    async createSession(input) {
      const id = `s-${sessions.length + 1}`;
      sessions.push({ id, profileId: input.profileId, prompt: input.prompt, keep: input.keep, title: input.title });
      runSessions.push({ sessionId: id, keep: input.keep });
      return { sessionId: id, taskId: `t-${id}` };
    },
    async setSessionRelay(sessionId, relay) { relayFlags.push({ sessionId, relay }); },
    async sendPrompt(sessionId, _promptId, text) { prompts.push({ sessionId, text }); },
    async endSession(sessionId) { ended.push(sessionId); },
    async exec() { return { exitStatus: 0, stdout: "", stderr: "" }; },
    async writeFiles(_s, files) { return files.map((f) => ({ path: f.path, ok: true })); },
  };

  const deps: EngineDeps = {
    step: runner ? runner.step : async (fn, name) => { names.push(name); return fn(); },
    recv: runner ? runner.recv : async () => recvQueue.shift() ?? null,
    store: {
      async loadSnapshot() { return snapshot; },
      async markRunning() {},
      async recordStep(_r, path, attempt, record) { records.push({ path, attempt, record }); },
      async finalizeRun(_r, status, error) { finalized.push({ status, ...(error !== undefined ? { error } : {}) }); },
      async listRunSessions() { return runSessions; },
      async releaseConcurrency() { return null; },
    },
    sessions: sessionOps,
    clock: { nowMs: () => (clock += 1000) },
    code: makeCodeBlockRuntime(),
  };

  return { deps, runner, names, records, sessions, prompts, relayFlags, policyCalls, ended, finalized };
}

afterEach(() => {
  setSlackRelayDeps(null);
});

describe("Slack thread brain through the interpreter", () => {
  test("(a) mention → session + relay → first turn idle → joined follow-up → second prompt → idle-timeout end → ✅ recap", async () => {
    const h = harness({
      recv: [
        // The first turn (initial prompt) ends.
        { kind: "session_idle", sessionId: "s-1" },
        // A follow-up reply joins the active run's mailbox.
        joined("<@UBOT> and what about the root cause?", "100.2"),
        // The second turn ends.
        { kind: "session_idle", sessionId: "s-1" },
        // Nothing more: the idle wait times out → the thread went quiet.
        null,
        null,
      ],
    });

    const result = await interpretAutomation(RUN, h.deps);

    expect(result.status).toBe("completed");
    // One session, the flagged channel's profile, mention text stripped of the bot handle, KEPT.
    expect(h.sessions).toEqual([
      expect.objectContaining({ profileId: "prof-a", prompt: "summarize the incident", keep: true }),
    ]);
    expect(h.sessions[0]!.title).toBe("summarize the incident");
    // The relay was installed on that session (consumer will forward curated events).
    expect(h.relayFlags).toEqual([{ sessionId: "s-1", relay: true }]);
    // The follow-up became the second prompt, mention stripped.
    expect(h.prompts).toEqual([{ sessionId: "s-1", text: "and what about the root cause?" }]);
    // Sessions are kept: nothing ended.
    expect(h.ended).toEqual([]);
    // The loop ran one real turn, then its idle wait timed out and the
    // `until` exited it (two iterations: the turn, then the quiet one).
    expect(h.names.filter((n) => /^step:thread\[\d+\]\.__until__:0$/.test(n)).length).toBe(2);
    expect(h.names).toContain("step:thread[1].next:0:wait");
    expect(h.names).not.toContain("step:thread[2].next:0");
    // The recap hook posted the ✅ completion through the same policy as legacy.
    expect(h.policyCalls).toContain("complete:");
    expect(h.finalized).toEqual([{ status: "completed" }]);
  });

  test("(a') the relay survives a pod restart: its state rides the checkpointed step outputs, not process memory", async () => {
    // Pass 1: the relay renders three messages into one bubble, then the pod
    // dies while waiting. Pass 2 (recovery): DBOS replays the recorded step
    // outputs WITHOUT re-running their closures and re-delivers the recv'd
    // messages, then the run goes live: message 4 must append to the SAME
    // bubble (the state the dead pod built), and the ✅ recap must carry the
    // last message. With state in a module map both would be lost — the
    // relay would answer "pass" forever and the recap would post nothing.
    const h = harness({
      replay: [
        curated(1, "run_started", {}),
        curated(2, "agent_message", { role: "assistant", text: "one" }),
        curated(3, "agent_message", { role: "assistant", text: "two" }),
        "crash",
        curated(4, "agent_message", { role: "assistant", text: "three" }),
        curated(5, "run_completed", { ok: true }),
        { kind: "session_idle", sessionId: "s-1" },
        null,
        null,
      ],
    });
    const runner = h.runner!;
    // The dying pod: its interpretAutomation never settles (recv hangs).
    void interpretAutomation(RUN, h.deps).catch(() => {});
    // Let pass 1 run up to the hang.
    for (let i = 0; i < 50 && !h.policyCalls.includes("msg:one\n\ntwo"); i += 1) {
      await new Promise((r) => setTimeout(r, 1));
    }
    expect(h.policyCalls).toEqual(["working", "msg:one", "msg:one\n\ntwo"]);
    const liveBeforeCrash = runner.executed.length;
    expect(liveBeforeCrash).toBeGreaterThan(0);

    runner.restart();
    const result = await interpretAutomation(RUN, h.deps);

    expect(result.status).toBe("completed");
    // Recovery replayed the recorded relay steps (their closures did not
    // run again: no duplicate "msg:one"/"working" through the policy) …
    expect(h.policyCalls.filter((c) => c === "msg:one")).toHaveLength(1);
    expect(h.policyCalls.filter((c) => c === "working")).toHaveLength(1);
    expect(new Set(runner.executed).size).toBe(runner.executed.length);
    expect(runner.executed.length).toBeGreaterThan(liveBeforeCrash);
    // … then message 4 appended to the bubble the dead pod opened (the
    // relay's state came back through the checkpointed outputs) …
    expect(h.policyCalls).toContain("msg:one\n\ntwo\n\nthree");
    expect(h.policyCalls).toContain("idle");
    // … and the recap read the last message from the recorded step outputs.
    expect(h.policyCalls.at(-1)).toBe("complete:three");
    // The relay's step names are unchanged across passes (contract stays 4).
    const relaySteps = h.names.filter((n) => n.includes(".__relay__:"));
    expect(relaySteps.slice(0, 3)).toEqual(relaySteps.slice(0, 3).map((n, i) => `step:relay.__relay__:${i + 1}`));
    // Only one install: recovery re-seeded the state, it did not re-bind.
    expect(h.relayFlags).toEqual([{ sessionId: "s-1", relay: true }]);
    expect(h.sessions).toHaveLength(1);
  });

  test("(b) a top-level channel message (no thread_ts) is filtered, never a session", async () => {
    const h = harness({
      eventKey: "message",
      payload: { team_id: "T1", event_id: "Ev9", event: { type: "message", channel: "C1", user: "U2", ts: "5.0", text: "hi" } },
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("filtered");
    expect(h.sessions).toEqual([]);
    // No relay → the recap hook has nothing to post to and says so.
    const recap = h.records.filter((r) => r.path.includes("recap")).at(-1);
    expect(recap?.record.outputs).toMatchObject({ posted: false });
  });

  test("(c) an unflagged channel with no default profile is filtered", async () => {
    const h = harness({ inputs: { channels: {}, default_profile: "", idle_timeout: 600, max_turns: 5 } });
    expect((await interpretAutomation(RUN, h.deps)).status).toBe("filtered");
    expect(h.sessions).toEqual([]);
  });

  test("(d) max_turns bounds the conversation; exhaustion ends the run cleanly", async () => {
    const h = harness({
      inputs: { channels: { C1: "prof-a" }, default_profile: "", idle_timeout: 600, max_turns: 2 },
      recv: [
        { kind: "session_idle", sessionId: "s-1" },
        joined("turn one", "100.2"),
        { kind: "session_idle", sessionId: "s-1" },
        joined("turn two", "100.3"),
        { kind: "session_idle", sessionId: "s-1" },
        // Would be turn three, but max_turns = 2: never consumed.
        joined("turn three", "100.4"),
      ],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.prompts.map((p) => p.text)).toEqual(["turn one", "turn two"]);
    // Exactly two iterations ran; the third joined message was never consumed.
    expect(h.names.filter((n) => /^step:thread\[\d+\]\.next:0$/.test(n)).length).toBe(2);
    expect(h.policyCalls.some((c) => c.startsWith("complete:"))).toBe(true);
  });

  test("(d') a bare `@bot` follow-up is not a turn: no prompt, no failure, the thread continues", async () => {
    const h = harness({
      recv: [
        { kind: "session_idle", sessionId: "s-1" },
        // Nothing left once the mention is stripped.
        joined("<@UBOT>", "100.2"),
        joined("<@UBOT> now the real question", "100.3"),
        { kind: "session_idle", sessionId: "s-1" },
        null,
        null,
      ],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    // The empty turn never reached send_prompt (which refuses an empty prompt);
    // the next real message did.
    expect(h.prompts.map((p) => p.text)).toEqual(["now the real question"]);
    expect(h.names).not.toContain("step:thread[0].has_turn.turn:0");
    expect(h.names).toContain("step:thread[1].has_turn.turn:0");
    expect(h.policyCalls.at(-1)).toBe("complete:");
  });

  test("(e) a failed turn ends the run as failed and posts ❌ through the policy", async () => {
    const h = harness({
      recv: [{ kind: "session_idle", sessionId: "s-1", runFailed: true }],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("failed");
    expect(h.policyCalls.some((c) => c.startsWith("fail:"))).toBe(true);
    expect(h.ended).toEqual([]);
  });
});
