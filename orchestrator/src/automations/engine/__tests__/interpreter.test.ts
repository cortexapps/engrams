import { describe, expect, test } from "bun:test";

import type {
  EngineDeps,
  EngineRunStore,
  EngineSessionOps,
  EngineStepRecord,
} from "../deps.ts";
import type { AutomationDefinition, BlockDef } from "../definition.ts";
import type { RunSnapshot } from "../context.ts";
import type { AutomationInbox } from "../inbox.ts";
import { interpretAutomation } from "../interpreter.ts";

// ---------------------------------------------------------------------------
// Fakes (the automation-run.test.ts pattern: immediate steps + in-memory
// stores + a scripted receiver; zero DBOS)
// ---------------------------------------------------------------------------

interface Harness {
  deps: EngineDeps;
  names: string[];
  stepRecords: Array<{ framePath: string; attempt: number; record: EngineStepRecord }>;
  finalized: Array<{ status: string; error?: string }>;
  ended: string[];
  created: string[];
  createdInputs: Array<Parameters<EngineSessionOps["createSession"]>[0]>;
  prompts: Array<{ sessionId: string; promptId: string; text: string }>;
  execs: Array<{ sessionId: string; command: string; execId: string }>;
  released: string[];
  promoted: string[];
}

function makeDefinition(
  blocks: BlockDef[],
  settings?: Partial<AutomationDefinition["settings"]>,
): AutomationDefinition {
  return {
    engine: 1,
    trigger: { kind: "manual" },
    blocks,
    inputsSchema: [],
    settings: { endSessionsOnFinish: false, ...settings },
  };
}

function makeHarness(
  definition: AutomationDefinition,
  options: {
    recv?: Array<AutomationInbox | null>;
    inputs?: Record<string, unknown>;
    payload?: Record<string, unknown>;
    sessions?: Array<{ sessionId: string; keep: boolean }>;
    promote?: string | null;
    failExec?: boolean;
  } = {},
): Harness {
  const names: string[] = [];
  const stepRecords: Harness["stepRecords"] = [];
  const finalized: Harness["finalized"] = [];
  const ended: string[] = [];
  const created: string[] = [];
  const createdInputs: Array<Parameters<EngineSessionOps["createSession"]>[0]> = [];
  const prompts: Harness["prompts"] = [];
  const execs: Harness["execs"] = [];
  const released: string[] = [];
  const promoted: string[] = [];
  const recvQueue = [...(options.recv ?? [])];
  const runSessions = options.sessions ?? [];
  let clock = 1_000_000_000;

  const snapshot: RunSnapshot = {
    definition,
    inputs: options.inputs ?? {},
    automationId: "auto-1",
    automationName: "Test automation",
    version: 1,
    trigger: {
      kind: "manual",
      receivedAt: "2026-08-21T00:00:00Z",
      ...(options.payload ? { payload: options.payload, eventKey: "test.event" } : {}),
    },
    aliases: [],
    startedAtMs: clock,
  };

  const store: EngineRunStore = {
    async loadSnapshot() {
      return snapshot;
    },
    async markRunning() {},
    async recordStep(_runId, framePath, attempt, record) {
      stepRecords.push({ framePath, attempt, record });
    },
    async finalizeRun(_runId, status, error) {
      finalized.push({ status, ...(error !== undefined ? { error } : {}) });
    },
    async listRunSessions() {
      return runSessions;
    },
    async releaseConcurrency(runId) {
      released.push(runId);
      return options.promote ?? null;
    },
  };

  const sessions: EngineSessionOps = {
    async createSession(input) {
      const sessionId = `s-${input.blockId}`;
      createdInputs.push(input);
      created.push(sessionId);
      runSessions.push({ sessionId, keep: input.keep });
      return { sessionId, taskId: `t-${input.blockId}` };
    },
    async sendPrompt(sessionId, promptId, text) {
      prompts.push({ sessionId, promptId, text });
    },
    async endSession(sessionId) {
      ended.push(sessionId);
    },
    async exec(sessionId, command, opts) {
      execs.push({ sessionId, command, execId: opts.execId });
      if (options.failExec) return { exitStatus: 1, stdout: "", stderr: "boom" };
      return { exitStatus: 0, stdout: "ok\n", stderr: "" };
    },
    async writeFiles(_sessionId, files) {
      return files.map((f) => ({ path: f.path, ok: true }));
    },
  };

  const deps: EngineDeps = {
    step: async (fn, name) => {
      names.push(name);
      return fn();
    },
    recv: async () => (recvQueue.length > 0 ? recvQueue.shift()! : null),
    store,
    sessions,
    clock: {
      nowMs: () => {
        clock += 60_000;
        return clock;
      },
    },
    startQueuedRun: async (runId) => {
      promoted.push(runId);
    },
  };

  return { deps, names, stepRecords, finalized, ended, created, createdInputs, prompts, execs, released, promoted };
}

const RUN = { runId: "autorun:auto-1:manual:x", automationId: "auto-1" };

// ---------------------------------------------------------------------------

describe("interpretAutomation — golden step sequences (ENGINE_STEP_CONTRACT 1)", () => {
  test("linear graph: filter → create_session → send_prompt(wait) → end_session", async () => {
    const definition = makeDefinition([
      {
        id: "check",
        type: "filter",
        config: { conditions: { mode: "all", conditions: [{ path: "inputs.on", op: "is_true" }] } },
      },
      { id: "launch", type: "create_session", config: { profileId: "p", promptTemplate: "go" } },
      {
        id: "notify",
        type: "send_prompt",
        config: {
          session: { blockId: "launch" },
          promptTemplate: "ping ${{ steps.launch.session_id }}",
          waitFor: { kind: "run_end" },
        },
      },
      { id: "cleanup", type: "end_session", config: { session: { blockId: "launch" } } },
    ]);
    const h = makeHarness(definition, {
      inputs: { on: true },
      recv: [{ kind: "session_idle", sessionId: "s-launch" }],
    });

    const result = await interpretAutomation(RUN, h.deps);

    expect(result.status).toBe("completed");
    expect(h.names).toEqual([
      "step:__snapshot__:0",
      "step:check:0",
      "step:check:0:ledger",
      "step:launch:0",
      "step:notify:0",
      "step:notify:0:wait",
      "step:cleanup:0",
      "step:__finalize__:0",
    ]);
    expect(h.prompts[0]).toMatchObject({
      sessionId: "s-launch",
      promptId: `autorun:${RUN.runId}:notify:s-launch`,
      text: "ping s-launch",
    });
    expect(h.ended).toEqual(["s-launch"]);
    expect(h.finalized).toEqual([{ status: "completed" }]);
  });

  test("filter miss ends the run as filtered", async () => {
    const definition = makeDefinition([
      {
        id: "check",
        type: "filter",
        config: { conditions: { mode: "all", conditions: [{ path: "inputs.on", op: "is_true" }] } },
      },
      { id: "launch", type: "create_session", config: { profileId: "p", promptTemplate: "go" } },
    ]);
    const h = makeHarness(definition, { inputs: { on: false } });

    const result = await interpretAutomation(RUN, h.deps);

    expect(result.status).toBe("filtered");
    expect(h.names).toEqual([
      "step:__snapshot__:0",
      "step:check:0",
      "step:check:0:ledger",
      "step:__finalize__:0",
    ]);
    expect(h.created).toEqual([]);
    expect(h.finalized[0]!.status).toBe("filtered");
  });

  test("branch takes then/else with nested frame paths", async () => {
    const definition = makeDefinition([
      {
        id: "gate",
        type: "branch",
        config: { conditions: { mode: "all", conditions: [{ path: "inputs.hot", op: "is_true" }] } },
        then: [{ id: "hot_path", type: "create_session", config: { profileId: "p", promptTemplate: "hot" } }],
        else: [{ id: "cold_path", type: "create_session", config: { profileId: "p", promptTemplate: "cold" } }],
      },
    ]);
    const hot = makeHarness(definition, { inputs: { hot: true } });
    await interpretAutomation(RUN, hot.deps);
    expect(hot.names).toEqual([
      "step:__snapshot__:0",
      "step:gate.__cond__:0",
      "step:gate.hot_path:0",
      "step:__finalize__:0",
    ]);

    const cold = makeHarness(definition, { inputs: { hot: false } });
    await interpretAutomation(RUN, cold.deps);
    expect(cold.names).toContain("step:gate.cold_path:0");
  });

  test("loop iterates with iteration-suffixed frames and an until exit", async () => {
    const definition = makeDefinition([
      { id: "launch", type: "create_session", config: { profileId: "p", promptTemplate: "go" } },
      {
        id: "poll",
        type: "loop",
        config: {
          maxIterations: 3,
          until: { mode: "all", conditions: [{ path: "steps.tick.exit_status", op: "equals", value: 0 }] },
        },
        body: [
          {
            id: "tick",
            type: "run_command",
            config: { session: { blockId: "launch" }, commandTemplate: "true" },
          },
        ],
      },
    ]);
    const h = makeHarness(definition);
    const result = await interpretAutomation(RUN, h.deps);

    expect(result.status).toBe("completed");
    expect(h.names).toEqual([
      "step:__snapshot__:0",
      "step:launch:0",
      "step:poll[0].tick:0",
      "step:poll[0].__until__:0",
      "step:__finalize__:0",
    ]);
    expect(h.execs[0]!.execId).toBe(`exec:auto:${RUN.runId}:tick:a0`);
  });

  test("engine-level retries mint attempt-scoped steps then fail the run", async () => {
    const definition = makeDefinition([
      { id: "launch", type: "create_session", config: { profileId: "p", promptTemplate: "go" } },
      {
        id: "flaky",
        type: "run_command",
        retry: { attempts: 2, retryOn: "always" },
        config: { session: { blockId: "launch" }, commandTemplate: "false" },
      },
    ]);
    const h = makeHarness(definition, { failExec: true });
    const result = await interpretAutomation(RUN, h.deps);

    expect(result.status).toBe("failed");
    expect(h.names).toEqual([
      "step:__snapshot__:0",
      "step:launch:0",
      "step:flaky:0",
      "step:flaky:1",
      "step:__finalize__:0",
    ]);
    expect(result.error).toContain("exec_nonzero_exit");
  });
});

describe("interpretAutomation — waits, control messages, finalize", () => {
  const waitingDefinition = makeDefinition([
    { id: "launch", type: "create_session", config: { profileId: "p", promptTemplate: "go" } },
    {
      id: "notify",
      type: "send_prompt",
      config: {
        session: { blockId: "launch" },
        promptTemplate: "ping",
        waitFor: { kind: "run_end" },
        deadlineSeconds: 120,
      },
    },
  ]);

  test("recv deadline becomes a typed deadline terminal, not an exception", async () => {
    const h = makeHarness(waitingDefinition, { recv: [null, null, null] });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("deadline");
    // Clock micro-steps checkpointed the elapsed time.
    expect(h.names.some((n) => n.startsWith("step:notify:clock:"))).toBe(true);
    expect(h.finalized[0]!.status).toBe("deadline");
  });

  test("stop halts; supersede skips claim release", async () => {
    const stopped = makeHarness(waitingDefinition, { recv: [{ kind: "stop", reason: "operator" }] });
    expect((await interpretAutomation(RUN, stopped.deps)).status).toBe("halted");
    expect(stopped.released).toEqual([RUN.runId]);

    const superseded = makeHarness(waitingDefinition, {
      recv: [{ kind: "supersede", byRunId: "autorun:auto-1:manual:y" }],
    });
    expect((await interpretAutomation(RUN, superseded.deps)).status).toBe("superseded");
    expect(superseded.released).toEqual([]);
  });

  test("messages for other sessions buffer and are consumed by a later wait", async () => {
    const definition = makeDefinition([
      { id: "a", type: "create_session", config: { profileId: "p", promptTemplate: "a" } },
      { id: "b", type: "create_session", config: { profileId: "p", promptTemplate: "b" } },
      {
        id: "wait_a",
        type: "send_prompt",
        config: { session: { blockId: "a" }, promptTemplate: "pa", waitFor: { kind: "run_end" } },
      },
      { id: "wait_b", type: "wait_session", config: { session: { blockId: "b" }, until: "idle" } },
    ]);
    // b's idle arrives while we wait on a; it buffers, then wait_b consumes it
    // without another recv.
    const h = makeHarness(definition, {
      recv: [
        { kind: "session_idle", sessionId: "s-b" },
        { kind: "session_idle", sessionId: "s-a" },
      ],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    const waitB = h.stepRecords.filter((r) => r.framePath === "wait_b").at(-1)!;
    expect(waitB.record.outputs).toMatchObject({ outcome: "completed" });
  });

  test("a failed session run fails the run", async () => {
    const h = makeHarness(waitingDefinition, {
      recv: [{ kind: "session_idle", sessionId: "s-launch", runFailed: true }],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("failed");
  });

  test("signal waits match name and session", async () => {
    const definition = makeDefinition([
      { id: "launch", type: "create_session", config: { profileId: "p", promptTemplate: "go" } },
      {
        id: "review",
        type: "send_prompt",
        config: {
          session: { blockId: "launch" },
          promptTemplate: "review",
          waitFor: { kind: "signal", name: "finder_done" },
        },
      },
    ]);
    const h = makeHarness(definition, {
      recv: [
        { kind: "signal", name: "other_signal", sessionId: "s-launch" },
        { kind: "signal", name: "finder_done", sessionId: "s-launch", payload: { candidates: 3 } },
      ],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    const review = h.stepRecords.filter((r) => r.framePath === "review").at(-1)!;
    expect(review.record.outputs).toMatchObject({ signal: { candidates: 3 } });
  });

  test("wait_event consumes joined deliveries", async () => {
    const definition = makeDefinition([
      { id: "next", type: "wait_event", config: { eventKeys: ["app_mention"] } },
    ]);
    const h = makeHarness(definition, {
      recv: [
        {
          kind: "event",
          eventKey: "app_mention",
          deliveryKey: "slack:E1",
          payload: { text: "hi" },
          receivedAt: "2026-08-21T00:01:00Z",
        },
      ],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    const next = h.stepRecords.filter((r) => r.framePath === "next").at(-1)!;
    expect(next.record.outputs).toMatchObject({ event_key: "app_mention" });
  });

  test("finalize keeps sessions by default and ends them under end_sessions_on_finish", async () => {
    const keepDef = makeDefinition([
      { id: "launch", type: "create_session", config: { profileId: "p", promptTemplate: "go" } },
    ]);
    const kept = makeHarness(keepDef);
    await interpretAutomation(RUN, kept.deps);
    expect(kept.ended).toEqual([]);

    const endDef = makeDefinition(
      [{ id: "launch", type: "create_session", config: { profileId: "p", promptTemplate: "go" } }],
      { endSessionsOnFinish: true },
    );
    const endedRun = makeHarness(endDef);
    await interpretAutomation(RUN, endedRun.deps);
    expect(endedRun.ended).toEqual(["s-launch"]);

    // keepOnFinish overrides the automation setting.
    const pinnedDef = makeDefinition(
      [
        {
          id: "launch",
          type: "create_session",
          config: { profileId: "p", promptTemplate: "go", keepOnFinish: true },
        },
      ],
      { endSessionsOnFinish: true },
    );
    const pinned = makeHarness(pinnedDef);
    await interpretAutomation(RUN, pinned.deps);
    expect(pinned.ended).toEqual([]);
  });

  test("queue promotion starts the fixed successor run id", async () => {
    const definition = makeDefinition([
      { id: "launch", type: "create_session", config: { profileId: "p", promptTemplate: "go" } },
    ]);
    const h = makeHarness(definition, { promote: "autorun:auto-1:manual:next" });
    await interpretAutomation(RUN, h.deps);
    expect(h.promoted).toEqual(["autorun:auto-1:manual:next"]);
  });

  test("includeEventContext appends the redacted payload with the disclaimer (ADR 0102 parity)", async () => {
    const definition = makeDefinition([
      {
        id: "launch",
        type: "create_session",
        config: { profileId: "p", promptTemplate: "Triage this.", includeEventContext: true },
      },
    ]);
    const withEvent = makeHarness(definition, {
      payload: { issue: { title: "boom" } },
    });
    await interpretAutomation(RUN, withEvent.deps);
    const prompt = withEvent.createdInputs[0]!.prompt;
    expect(prompt).toStartWith("Triage this.");
    expect(prompt).toContain("untrusted external input");
    expect(prompt).toContain('"boom"');
    expect(prompt).toContain("test.event");

    // No event key (cron/manual) → nothing appended, the legacy gate.
    const withoutEvent = makeHarness(definition);
    await interpretAutomation(RUN, withoutEvent.deps);
    expect(withoutEvent.createdInputs[0]!.prompt).toBe("Triage this.");
  });

  test("phase-2 stubs return typed unavailable failures", async () => {
    const definition = makeDefinition([
      { id: "transform", type: "code", config: { source: "export default () => 1", mode: "value" } },
    ]);
    const h = makeHarness(definition);
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("failed");
    expect(result.error).toContain("code_runtime_unavailable");
  });
});
