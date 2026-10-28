import { describe, expect, test } from "bun:test";

import { setSlackPolicyFactoryForTest } from "../../../integrations/slack-policy.ts";
import type { CommunicationPolicy } from "../../../workflows/communication-policy.ts";
import { registerEngineBlocks } from "../blocks/index.ts";
import { CLOSE_AGENT_TYPE, RUN_AGENT_TYPE, closeAgentConfigSchema, runAgentConfigSchema } from "../blocks/agent.ts";
import { getBlock } from "../blocks/registry.ts";
import { buildRunContext, type RunSnapshot } from "../context.ts";
import type { EngineDeps, EngineRunStore, EngineSessionOps } from "../deps.ts";
import { makeFakeRelayStore } from "./fake-relay-store.ts";

registerEngineBlocks();

const RUN_ID = "autorun:auto-1:slack:E1";

const RELAY = { provider: "slack", team: "T1", channel: "C1", threadTs: "100.0", mentionTs: "100.0", userId: "U1", eventId: "E1" };

function harness(options: {
  /** The workstream this run is bound to; absent = an unscoped run. */
  instanceId?: string;
  /** The kept session a previous run of the workstream left: `alive`
   * means the control plane still has it, `status` what it says (idle by
   * default), `runId` which run holds its binding. */
  previous?: { sessionId: string; alive: boolean; owner?: "terminal" | "live"; status?: string; runId?: string };
  dryRun?: boolean;
} = {}) {
  const store = makeFakeRelayStore();
  const announced: Array<{ sessionId: string; ts: string }> = [];
  const created: Array<{ profileId: string; prompt: string; keep: boolean }> = [];
  /** `relayed`: whether the session's relay row existed when the prompt
   * went in (the relay must be on before the first turn). */
  const prompts: Array<{ sessionId: string; promptId: string; text: string; relayed: boolean }> = [];
  const adopted: string[] = [];
  const ended: string[] = [];
  const closed: Array<{ instanceId: string; reason?: string }> = [];
  const sessions: EngineSessionOps = {
    createSession: async (input) => {
      created.push({ profileId: input.profileId, prompt: input.prompt, keep: input.keep });
      return { sessionId: `s-new-${created.length}`, taskId: "t" };
    },
    sendPrompt: async (sessionId, promptId, text) => {
      prompts.push({ sessionId, promptId, text, relayed: store.rows.has(sessionId) });
    },
    endSession: async (sessionId) => {
      ended.push(sessionId);
    },
    exec: async () => ({ exitStatus: 0, stdout: "", stderr: "" }),
    writeFiles: async () => [],
    getSession: async (sessionId) =>
      options.previous?.alive && sessionId === options.previous.sessionId
        ? { found: true, status: options.previous.status ?? "idle", lastActiveAt: "2026-08-22T09:00:00Z", lastEventAt: null }
        : { found: false },
  };
  const runStore: EngineRunStore = {
    loadSnapshot: () => Promise.reject(new Error("unused")),
    markRunning: async () => {},
    recordStep: async () => {},
    finalizeRun: async () => {},
    latestKeptInstanceSession: async () =>
      options.previous ? { sessionId: options.previous.sessionId, runId: options.previous.runId ?? "autorun:old" } : null,
    listRunSessions: async () => [],
    releaseConcurrency: async () => null,
    adoptSession: async ({ sessionId }) => {
      adopted.push(sessionId);
      if (sessionId !== options.previous?.sessionId) return "foreign";
      if (options.previous.runId === RUN_ID) return "already_ours";
      return options.previous.owner === "live" ? "owner_live" : "adopted";
    },
    getSessionBinding: async () => null,
  };
  const deps: EngineDeps = {
    step: async (fn) => fn(),
    recv: async () => null,
    store: runStore,
    sessions,
    clock: { nowMs: () => 0 },
    relays: store,
    announceStarted: async (sessionId, target) => {
      announced.push({ sessionId, ts: target.ts });
    },
    instances: {
      closeInstance: async (input) => {
        closed.push(input);
        return true;
      },
    },
  };
  const snapshot: RunSnapshot = {
    definition: { engine: 1, trigger: { kind: "manual" }, blocks: [], inputsSchema: [], settings: { endSessionsOnFinish: false } },
    inputs: {},
    automationId: "auto-1",
    automationName: "Agent test",
    version: 1,
    trigger: { kind: "manual", receivedAt: "2026-08-22T00:00:00Z" },
    aliases: [],
    startedAtMs: 0,
    ...(options.instanceId !== undefined ? { instanceId: options.instanceId } : {}),
    ...(options.dryRun ? { dryRun: true } : {}),
  };
  const ctx = buildRunContext(RUN_ID, snapshot, deps);
  ctx.currentBlockId = "agent";
  ctx.currentPath = "agent";
  // Parsed as the interpreter parses it: the schema's defaults apply.
  const run = (config: Record<string, unknown>) =>
    getBlock(RUN_AGENT_TYPE)!.execute!(runAgentConfigSchema.parse(config) as never, ctx);
  const close = (config: Record<string, unknown> = {}) =>
    getBlock(CLOSE_AGENT_TYPE)!.execute!(closeAgentConfigSchema.parse(config) as never, ctx);
  return { store, created, prompts, adopted, announced, ended, closed, ctx, run, close };
}

/** A Slack policy that records its closing messages. */
function recordingPolicy() {
  const neutral: Array<{ ts: string; message: string }> = [];
  const pol = {
    onNeutralClose: async (m: { ts: string }, message: string) => {
      neutral.push({ ts: m.ts, message });
    },
  } as unknown as CommunicationPolicy;
  return { pol, neutral };
}

const BASE = { profileId: "prof-a", promptTemplate: "new message: hi" };

describe("run_agent", () => {
  test("an unscoped run starts a session with the opening prompt", async () => {
    const h = harness();
    const out = await h.run({ ...BASE, openingPromptTemplate: "whole thread: hi" });
    expect(out).toMatchObject({
      kind: "ok",
      outputs: { session_id: "s-new-1", started: true, delivered: true, prompt: "whole thread: hi" },
    });
    // The session starts with no prompt; the opening prompt follows once
    // the relay would be on (none here).
    expect(h.created).toEqual([{ profileId: "prof-a", prompt: "", keep: true }]);
    expect(h.prompts).toEqual([
      { sessionId: "s-new-1", promptId: `autorun:${RUN_ID}:agent:s-new-1`, text: "whole thread: hi", relayed: false },
    ]);
    expect(h.adopted).toEqual([]);
    expect(h.store.rows.size).toBe(0);
  });

  test("a workstream with a live kept session gets the prompt, not a new session", async () => {
    const h = harness({ instanceId: "ai_thread1", previous: { sessionId: "s-old", alive: true } });
    const out = await h.run(BASE);
    expect(out).toMatchObject({
      kind: "ok",
      outputs: { session_id: "s-old", started: false, delivered: true, prompt: "new message: hi" },
    });
    expect(h.adopted).toEqual(["s-old"]);
    expect(h.created).toEqual([]);
    expect(h.prompts).toEqual([
      { sessionId: "s-old", promptId: `autorun:${RUN_ID}:agent:s-old`, text: "new message: hi", relayed: false },
    ]);
  });

  test("a kept session that is gone (swept) starts a fresh one", async () => {
    const h = harness({ instanceId: "ai_thread1", previous: { sessionId: "s-old", alive: false } });
    const out = await h.run(BASE);
    expect(out).toMatchObject({ kind: "ok", outputs: { session_id: "s-new-1", started: true } });
    expect(h.adopted).toEqual([]);
  });

  test("a kept session that ended (completed, failed) is gone too: the workstream never wedges on it", async () => {
    for (const status of ["completed", "failed", "dead"]) {
      const h = harness({ instanceId: "ai_thread1", previous: { sessionId: "s-old", alive: true, status } });
      const out = await h.run(BASE);
      expect(out).toMatchObject({ kind: "ok", outputs: { session_id: "s-new-1", started: true } });
      expect(h.adopted).toEqual([]);
    }
  });

  test("a step re-executed after a crash converges on the session this run already holds", async () => {
    // The adopt (or the start) went through, the checkpoint did not: the
    // kept session is bound to THIS run. The step continues it instead of
    // starting a second session for the workstream, under the same prompt
    // id, so the outbox dedupes a prompt that already went in.
    const h = harness({ instanceId: "ai_thread1", previous: { sessionId: "s-old", alive: true, runId: RUN_ID } });
    const out = await h.run(BASE);
    expect(out).toMatchObject({ kind: "ok", outputs: { session_id: "s-old", started: false } });
    expect(h.created).toEqual([]);
    expect(h.prompts.map((p) => p.promptId)).toEqual([`autorun:${RUN_ID}:agent:s-old`]);
  });

  test("reuse: never always starts a session", async () => {
    const h = harness({ instanceId: "ai_thread1", previous: { sessionId: "s-old", alive: true } });
    const out = await h.run({ ...BASE, reuse: "never" });
    expect(out).toMatchObject({ kind: "ok", outputs: { session_id: "s-new-1", started: true } });
    expect(h.prompts.map((p) => p.sessionId)).toEqual(["s-new-1"]);
  });

  test("a kept session another run still owns is a retryable error, never a steal", async () => {
    const h = harness({ instanceId: "ai_thread1", previous: { sessionId: "s-old", alive: true, owner: "live" } });
    const out = await h.run(BASE);
    expect(out).toMatchObject({ kind: "error", code: "agent_session_busy", retryable: true });
    expect(h.prompts).toEqual([]);
    expect(h.created).toEqual([]);
  });

  test("an empty follow-up prompt delivers nothing and says so", async () => {
    const h = harness({ instanceId: "ai_thread1", previous: { sessionId: "s-old", alive: true } });
    const out = await h.run({ ...BASE, promptTemplate: "" });
    expect(out).toMatchObject({ kind: "ok", outputs: { session_id: "s-old", started: false, delivered: false } });
    expect(h.prompts).toEqual([]);
  });

  test("with a relay, a new session gets one installed and a continued one is re-pointed", async () => {
    const fresh = harness();
    const out = await fresh.run({ ...BASE, relay: RELAY });
    expect(out).toMatchObject({ kind: "ok", outputs: { session_id: "s-new-1", relayed: true, mention_ts: "100.0" } });
    expect(fresh.store.rows.get("s-new-1")!.target).toMatchObject({ channel: "C1", ts: "100.0" });
    // The new session's link goes into the thread at once, and the relay
    // is on before the opening prompt starts the first turn.
    expect(fresh.announced).toEqual([{ sessionId: "s-new-1", ts: "100.0" }]);
    expect(fresh.prompts).toEqual([
      { sessionId: "s-new-1", promptId: `autorun:${RUN_ID}:agent:s-new-1`, text: "new message: hi", relayed: true },
    ]);

    const continued = harness({ instanceId: "ai_thread1", previous: { sessionId: "s-old", alive: true } });
    await continued.store.install({
      sessionId: "s-old",
      automationId: "auto-1",
      provider: "slack",
      target: { ...RELAY, threadRoot: "100.0", ts: "100.0" },
      state: { bubble: { ts: "b1", text: "earlier" }, lastAssistantText: "earlier", assets: [], questionTs: {}, questionProtocols: {} },
    });
    const again = await continued.run({ ...BASE, relay: { ...RELAY, mentionTs: "200.0", eventId: "E2" } });
    expect(again).toMatchObject({ kind: "ok", outputs: { session_id: "s-old", relayed: true, mention_ts: "200.0" } });
    const row = continued.store.rows.get("s-old")!;
    expect(row.target).toMatchObject({ ts: "200.0", eventId: "E2" });
    // The re-point happened before the prompt: the turn lands under the
    // new mention.
    expect(continued.store.writes.indexOf("repoint")).toBeGreaterThanOrEqual(0);
    expect(continued.prompts[0]!.relayed).toBe(true);
    // The conversation continues (last text kept); the bubble closes so the
    // next answer opens under the new mention.
    expect(row.state).toMatchObject({ bubble: null, lastAssistantText: "earlier" });
    // A continued session's thread already carries its link.
    expect(continued.announced).toEqual([]);
  });

  test("a re-executed step finds its relay installed and announces nothing twice", async () => {
    const h = harness({ instanceId: "ai_thread1", previous: { sessionId: "s-new-1", alive: true, runId: RUN_ID } });
    await h.store.install({
      sessionId: "s-new-1",
      automationId: "auto-1",
      provider: "slack",
      target: { ...RELAY, threadRoot: "100.0", ts: "100.0" },
      state: { bubble: null, lastAssistantText: null, assets: [], questionTs: {}, questionProtocols: {} },
    });
    const out = await h.run({ ...BASE, relay: RELAY });
    expect(out).toMatchObject({ kind: "ok", outputs: { session_id: "s-new-1", relayed: true } });
    expect(h.announced).toEqual([]);
  });

  test("without a relay a new session is announced nowhere", async () => {
    const h = harness();
    await h.run(BASE);
    expect(h.announced).toEqual([]);
  });

  test("close_agent closes the relay with the reason, ends the kept session and closes the workstream", async () => {
    const h = harness({ instanceId: "ai_thread1", previous: { sessionId: "s-old", alive: true } });
    await h.store.install({
      sessionId: "s-old",
      automationId: "auto-1",
      provider: "slack",
      target: { ...RELAY, threadRoot: "100.0", ts: "100.0" },
      state: { bubble: null, lastAssistantText: "earlier", assets: [], questionTs: {}, questionProtocols: {} },
    });
    const policy = recordingPolicy();
    setSlackPolicyFactoryForTest(() => policy.pol);
    try {
      const out = await h.close({ reason: "closed by the \"done\" way in" });
      expect(out).toMatchObject({ kind: "ok", outputs: { closed: true, ended: true, session_id: "s-old" } });
    } finally {
      setSlackPolicyFactoryForTest(null);
    }
    expect(policy.neutral).toEqual([{ ts: "100.0", message: 'closed by the "done" way in' }]);
    expect(h.store.rows.size).toBe(0);
    expect(h.ended).toEqual(["s-old"]);
    expect(h.closed).toEqual([{ instanceId: "ai_thread1", reason: 'closed by the "done" way in' }]);
  });

  test("close_agent with no live session only closes the workstream; unbound it closes nothing", async () => {
    const gone = harness({ instanceId: "ai_thread1", previous: { sessionId: "s-old", alive: true, status: "completed" } });
    expect(await gone.close()).toMatchObject({ kind: "ok", outputs: { closed: true, ended: false } });
    expect(gone.ended).toEqual([]);
    expect(gone.closed).toHaveLength(1);

    const unbound = harness();
    expect(await unbound.close()).toMatchObject({ kind: "ok", outputs: { closed: false, not_instanced: true } });
    expect(unbound.closed).toEqual([]);
  });

  test("a dry run boots nothing, sends nothing and installs no relay", async () => {
    const h = harness({ instanceId: "ai_thread1", previous: { sessionId: "s-old", alive: true }, dryRun: true });
    const out = await h.run({ ...BASE, relay: RELAY });
    expect(out).toMatchObject({ kind: "ok", outputs: { started: true, dry_run: true, relayed: false } });
    expect(h.created).toEqual([]);
    expect(h.prompts).toEqual([]);
    expect(h.adopted).toEqual([]);
    expect(h.store.rows.size).toBe(0);
  });
});
