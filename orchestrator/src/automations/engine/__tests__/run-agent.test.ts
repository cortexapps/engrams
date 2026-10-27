import { describe, expect, test } from "bun:test";

import { registerEngineBlocks } from "../blocks/index.ts";
import { RUN_AGENT_TYPE, runAgentConfigSchema } from "../blocks/agent.ts";
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
   * means the control plane still has it. */
  previous?: { sessionId: string; alive: boolean; owner?: "terminal" | "live" };
  dryRun?: boolean;
} = {}) {
  const store = makeFakeRelayStore();
  const created: Array<{ profileId: string; prompt: string; keep: boolean }> = [];
  const prompts: Array<{ sessionId: string; promptId: string; text: string }> = [];
  const adopted: string[] = [];
  const sessions: EngineSessionOps = {
    createSession: async (input) => {
      created.push({ profileId: input.profileId, prompt: input.prompt, keep: input.keep });
      return { sessionId: `s-new-${created.length}`, taskId: "t" };
    },
    sendPrompt: async (sessionId, promptId, text) => {
      prompts.push({ sessionId, promptId, text });
    },
    endSession: async () => {},
    exec: async () => ({ exitStatus: 0, stdout: "", stderr: "" }),
    writeFiles: async () => [],
    getSession: async (sessionId) =>
      options.previous?.alive && sessionId === options.previous.sessionId
        ? { found: true, status: "idle", lastActiveAt: "2026-08-22T09:00:00Z", lastEventAt: null }
        : { found: false },
  };
  const runStore: EngineRunStore = {
    loadSnapshot: () => Promise.reject(new Error("unused")),
    markRunning: async () => {},
    recordStep: async () => {},
    finalizeRun: async () => {},
    latestKeptInstanceSession: async () =>
      options.previous ? { sessionId: options.previous.sessionId, runId: "autorun:old" } : null,
    listRunSessions: async () => [],
    releaseConcurrency: async () => null,
    adoptSession: async ({ sessionId }) => {
      adopted.push(sessionId);
      if (sessionId !== options.previous?.sessionId) return "foreign";
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
  return { store, created, prompts, adopted, ctx, run };
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
    expect(h.created).toEqual([{ profileId: "prof-a", prompt: "whole thread: hi", keep: true }]);
    expect(h.prompts).toEqual([]);
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
    expect(h.prompts).toEqual([{ sessionId: "s-old", promptId: `autorun:${RUN_ID}:agent:s-old`, text: "new message: hi" }]);
  });

  test("a kept session that is gone (swept) starts a fresh one", async () => {
    const h = harness({ instanceId: "ai_thread1", previous: { sessionId: "s-old", alive: false } });
    const out = await h.run(BASE);
    expect(out).toMatchObject({ kind: "ok", outputs: { session_id: "s-new-1", started: true } });
    expect(h.adopted).toEqual([]);
  });

  test("reuse: never always starts a session", async () => {
    const h = harness({ instanceId: "ai_thread1", previous: { sessionId: "s-old", alive: true } });
    const out = await h.run({ ...BASE, reuse: "never" });
    expect(out).toMatchObject({ kind: "ok", outputs: { session_id: "s-new-1", started: true } });
    expect(h.prompts).toEqual([]);
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
    // The conversation continues (last text kept); the bubble closes so the
    // next answer opens under the new mention.
    expect(row.state).toMatchObject({ bubble: null, lastAssistantText: "earlier" });
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
