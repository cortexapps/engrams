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
  actions: Array<{ actionId: string; stepPath: string; params: Record<string, unknown> }>;
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
    /** Fake integration-action runtime: records calls; throws when asked. */
    failActions?: boolean;
    /** Make recordStep throw for these frame paths (a ledger blip). */
    failRecordStepFor?: string[];
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
  const actions: Array<{ actionId: string; stepPath: string; params: Record<string, unknown> }> = [];
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
      if (options.failRecordStepFor?.includes(framePath)) throw new Error("ledger down");
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
    async setSessionRelay() {},
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
    integrationActions: {
      async execute(input) {
        if (options.failActions) throw new Error("provider down");
        actions.push({ actionId: input.actionId, stepPath: input.stepPath, params: input.params });
        return { comment_id: 7 };
      },
    },
  };

  return { deps, names, stepRecords, finalized, ended, created, createdInputs, prompts, execs, released, promoted, actions };
}

const RUN = { runId: "autorun:auto-1:manual:x", automationId: "auto-1" };

// ---------------------------------------------------------------------------

describe("interpretAutomation — golden step sequences (ENGINE_STEP_CONTRACT 4)", () => {
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
      // Two idles: the create prompt's turn, then the notify prompt's turn.
      // The wait must skip the first (stale) and consume the second.
      recv: [
        { kind: "session_idle", sessionId: "s-launch" },
        { kind: "session_idle", sessionId: "s-launch" },
      ],
    });

    const result = await interpretAutomation(RUN, h.deps);

    expect(result.status).toBe("completed");
    expect(h.names).toEqual([
      "step:__snapshot__:0",
      "step:check:0",
      "step:check:0:ledger",
      "step:launch:0",
      "step:notify:0",
      "step:notify:clock:1",
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
      // Contract 4: the loop's bound is a checkpointed decision step.
      "step:poll.__bound__:0",
      "step:poll[0].tick:0",
      "step:poll[0].__until__:0",
      "step:__finalize__:0",
    ]);
    expect(h.execs[0]!.execId).toBe(`exec:auto:${RUN.runId}:poll[0].tick:a0`);
  });

  test("every loop iteration mints its own idempotency identity (exec, prompt, action)", async () => {
    // Regression: keys derived from the static block id collapsed a loop's
    // iterations onto one external resource — the coordinator outbox dedupes
    // prompt_id (ON CONFLICT DO NOTHING), durable exec attaches to an
    // existing execId, and action client ids / markers dedupe on the provider.
    const definition = makeDefinition([
      { id: "launch", type: "create_session", config: { profileId: "p", promptTemplate: "go" } },
      {
        id: "turns",
        type: "loop",
        config: { maxIterations: 2 },
        body: [
          {
            id: "ask",
            type: "send_prompt",
            config: { session: { blockId: "launch" }, promptTemplate: "again", waitFor: { kind: "none" } },
          },
          { id: "tick", type: "run_command", config: { session: { blockId: "launch" }, commandTemplate: "true" } },
          {
            id: "post",
            type: "integration_action",
            config: { provider: "github", actionId: "create_issue_comment", params: { body: "hi" } },
          },
        ],
      },
    ]);
    const h = makeHarness(definition);
    const result = await interpretAutomation(RUN, h.deps);

    expect(result.status).toBe("completed");
    expect(h.prompts.map((p) => p.promptId)).toEqual([
      `autorun:${RUN.runId}:turns[0].ask:s-launch`,
      `autorun:${RUN.runId}:turns[1].ask:s-launch`,
    ]);
    expect(h.execs.map((e) => e.execId)).toEqual([
      `exec:auto:${RUN.runId}:turns[0].tick:a0`,
      `exec:auto:${RUN.runId}:turns[1].tick:a0`,
    ]);
    expect(h.actions.map((a) => a.stepPath)).toEqual(["turns[0].post", "turns[1].post"]);
  });

  test("a snapshot failure still finalizes: failed status, claim released, successor promoted", async () => {
    const definition = makeDefinition([]);
    const h = makeHarness(definition, { promote: "autorun:auto-1:manual:next" });
    const failing: EngineDeps = {
      ...h.deps,
      store: {
        ...h.deps.store,
        loadSnapshot: () => Promise.reject(new Error("inputs.limit: must be a number")),
      },
    };
    const result = await interpretAutomation(RUN, failing);

    expect(result).toEqual({ status: "failed", error: "inputs.limit: must be a number" });
    expect(h.names).toEqual(["step:__snapshot__:0", "step:__finalize__:0"]);
    expect(h.finalized).toEqual([{ status: "failed", error: "inputs.limit: must be a number" }]);
    expect(h.released).toEqual([RUN.runId]);
    expect(h.promoted).toEqual(["autorun:auto-1:manual:next"]);
  });

  test("a loop bound may be a $ref into inputs, resolved in the bound step and clamped", async () => {
    const definition = makeDefinition([
      { id: "launch", type: "create_session", config: { profileId: "p", promptTemplate: "go" } },
      {
        id: "poll",
        type: "loop",
        config: { maxIterations: { $ref: "inputs.max_turns" } },
        body: [
          {
            id: "tick",
            type: "run_command",
            config: { session: { blockId: "launch" }, commandTemplate: "true" },
          },
        ],
      },
    ]);
    const two = makeHarness(definition, { inputs: { max_turns: 2 } });
    expect((await interpretAutomation(RUN, two.deps)).status).toBe("completed");
    expect(two.names.filter((n) => n.startsWith("step:poll[")).length).toBe(2);
    expect(two.names).toContain("step:poll.__bound__:0");

    // Unusable bound (missing input) → zero iterations, never NaN silence.
    const none = makeHarness(definition, { inputs: {} });
    expect((await interpretAutomation(RUN, none.deps)).status).toBe("completed");
    expect(none.names.filter((n) => n.startsWith("step:poll[")).length).toBe(0);
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
    // without another recv. a's create-turn idle is stale for wait_a and is
    // dropped; a's second idle (the awaited prompt's turn) matches.
    const h = makeHarness(definition, {
      recv: [
        { kind: "session_idle", sessionId: "s-b" },
        { kind: "session_idle", sessionId: "s-a" },
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
      recv: [
        { kind: "session_idle", sessionId: "s-launch" },
        { kind: "session_idle", sessionId: "s-launch", runFailed: true },
      ],
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

  test("a $ref-bound wait deadline is clamped into the schema's range, never refused", async () => {
    // inputs.idle_timeout = 200000 (> 24 h) must shorten the wait to the
    // ceiling instead of failing the block with config_render_failed —
    // the loop-bound rule (contract 4) applied to deadlines.
    const definition: AutomationDefinition = {
      ...makeDefinition([
        {
          id: "next",
          type: "wait_event",
          config: { eventKeys: ["app_mention"], deadlineSeconds: { $ref: "inputs.idle_timeout" } },
        },
      ]),
      inputsSchema: [{ key: "idle_timeout", label: "Idle", type: "number" }],
    };
    const h = makeHarness(definition, {
      inputs: { idle_timeout: 200_000 },
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
    const running = h.stepRecords.find((r) => r.framePath === "next" && r.record.status === "running")!;
    expect(running.record.inputs).toMatchObject({ deadlineSeconds: 86_400 });

    // Below the floor → 1; unusable (a string) → the block's default applies.
    const low = makeHarness(definition, { inputs: { idle_timeout: 0 }, recv: [null] });
    await interpretAutomation(RUN, low.deps);
    expect(
      low.stepRecords.find((r) => r.framePath === "next" && r.record.status === "running")!.record.inputs,
    ).toMatchObject({ deadlineSeconds: 1 });
    const bad = makeHarness(definition, { inputs: { idle_timeout: "soon" }, recv: [null] });
    const badResult = await interpretAutomation(RUN, bad.deps);
    expect(badResult.status).not.toBe("failed");
    const badRunning = bad.stepRecords.find((r) => r.framePath === "next" && r.record.status === "running")!;
    expect(badRunning.record.inputs).not.toHaveProperty("deadlineSeconds");
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

  test("a banked idle from an un-awaited turn never satisfies a later wait (turn correlation)", async () => {
    // The review-flagged shape: fire-and-forget prompt X, then wait on Y.
    const definition = makeDefinition([
      { id: "launch", type: "create_session", config: { profileId: "p", promptTemplate: "go" } },
      {
        id: "fire_x",
        type: "send_prompt",
        config: { session: { blockId: "launch" }, promptTemplate: "do X", waitFor: { kind: "none" } },
      },
      {
        id: "wait_y",
        type: "send_prompt",
        config: {
          session: { blockId: "launch" },
          promptTemplate: "do Y",
          waitFor: { kind: "run_end" },
          deadlineSeconds: 600,
        },
      },
    ]);

    // Only the create and X turns' idles arrive: Y must NOT complete.
    const starved = makeHarness(definition, {
      recv: [
        { kind: "session_idle", sessionId: "s-launch" },
        { kind: "session_idle", sessionId: "s-launch" },
        null,
        null,
      ],
    });
    expect((await interpretAutomation(RUN, starved.deps)).status).toBe("deadline");

    // With Y's own idle (the third), the run completes.
    const fed = makeHarness(definition, {
      recv: [
        { kind: "session_idle", sessionId: "s-launch" },
        { kind: "session_idle", sessionId: "s-launch" },
        { kind: "session_idle", sessionId: "s-launch" },
      ],
    });
    expect((await interpretAutomation(RUN, fed.deps)).status).toBe("completed");
  });

  test("a banked signal goes stale once a newer prompt targets its session", async () => {
    const definition = makeDefinition([
      { id: "launch", type: "create_session", config: { profileId: "p", promptTemplate: "go" } },
      {
        id: "phase_a",
        type: "send_prompt",
        config: {
          session: { blockId: "launch" },
          promptTemplate: "A",
          waitFor: { kind: "signal", name: "a_done" },
        },
      },
      {
        id: "phase_b",
        type: "send_prompt",
        config: {
          session: { blockId: "launch" },
          promptTemplate: "B",
          waitFor: { kind: "signal", name: "b_done" },
          deadlineSeconds: 600,
        },
      },
    ]);
    // A stray early "b_done" arrives during phase A's wait and banks in the
    // buffer. Phase B's own prompt then makes it stale, so phase B must wait
    // for a fresh "b_done" — the banked one can never satisfy it. (An
    // in-flight duplicate first received AFTER the next prompt cannot be
    // distinguished without producer-side turn info; the ledger closes the
    // banked path, which is the routine one.)
    const h = makeHarness(definition, {
      recv: [
        { kind: "signal", name: "b_done", sessionId: "s-launch", payload: { phase: "early" } },
        { kind: "signal", name: "a_done", sessionId: "s-launch", payload: { phase: "a" } },
        { kind: "signal", name: "b_done", sessionId: "s-launch", payload: { phase: "b" } },
      ],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    const phaseB = h.stepRecords.filter((r) => r.framePath === "phase_b").at(-1)!;
    expect(phaseB.record.outputs).toMatchObject({ signal: { phase: "b" } });
  });

  test("finalize hooks run as their own steps before finalize, only for matching statuses (contract 2)", async () => {
    const hooks: Pick<AutomationDefinition["settings"], "onFinalize"> = {
      onFinalize: [
        {
          when: ["failed", "deadline"],
          block: {
            id: "report_failure",
            type: "integration_action",
            config: {
              provider: "github",
              actionId: "update_issue_comment",
              params: { body: "❌ ${{ run.status }}: ${{ run.error }}" },
            },
          },
        },
        {
          when: ["completed"],
          block: {
            id: "celebrate",
            type: "integration_action",
            config: { provider: "github", actionId: "create_issue_comment", params: { body: "✅" } },
          },
        },
      ],
    };
    // A failing run: the failure hook fires, the completion hook does not.
    const failing = makeDefinition(
      [
        { id: "launch", type: "create_session", config: { profileId: "p", promptTemplate: "go" } },
        {
          id: "boom",
          type: "run_command",
          config: { session: { blockId: "launch" }, commandTemplate: "false" },
        },
      ],
      { onFinalize: hooks.onFinalize },
    );
    const h = makeHarness(failing, { failExec: true });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("failed");
    expect(h.names).toEqual([
      "step:__snapshot__:0",
      "step:launch:0",
      "step:boom:0",
      "step:__finalize__.report_failure:0",
      "step:__finalize__:0",
    ]);
    expect(h.actions).toHaveLength(1);
    expect(h.actions[0]!.actionId).toBe("update_issue_comment");
    // The hook's template read the terminal status and reason.
    expect(String(h.actions[0]!.params["body"])).toMatch(/^❌ failed: block "boom"/);
    // The hook's outputs land in the ledger under the finalize path.
    const hookStep = h.stepRecords.filter((r) => r.framePath === "__finalize__.report_failure").at(-1)!;
    expect(hookStep.record.status).toBe("succeeded");

    // A completing run: only the completion hook fires.
    const completing = makeDefinition(
      [{ id: "launch", type: "create_session", config: { profileId: "p", promptTemplate: "go" } }],
      { onFinalize: hooks.onFinalize },
    );
    const c = makeHarness(completing);
    expect((await interpretAutomation(RUN, c.deps)).status).toBe("completed");
    expect(c.names).toEqual([
      "step:__snapshot__:0",
      "step:launch:0",
      "step:__finalize__.celebrate:0",
      "step:__finalize__:0",
    ]);
    expect(c.actions.map((a) => a.actionId)).toEqual(["create_issue_comment"]);
  });

  test("a throwing finalize hook is recorded on its step and never changes the terminal status", async () => {
    const definition = makeDefinition(
      [{ id: "launch", type: "create_session", config: { profileId: "p", promptTemplate: "go" } }],
      {
        onFinalize: [
          {
            when: ["completed"],
            block: {
              id: "flaky_hook",
              type: "integration_action",
              config: { provider: "github", actionId: "create_issue_comment", params: {} },
            },
          },
        ],
      },
    );
    const h = makeHarness(definition, { failActions: true });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.finalized).toEqual([{ status: "completed" }]);
    const hookStep = h.stepRecords.filter((r) => r.framePath === "__finalize__.flaky_hook").at(-1)!;
    expect(hookStep.record.status).toBe("failed");
    expect(hookStep.record.error).toContain("provider down");
    // Finalize still ran after the hook.
    expect(h.names.at(-1)).toBe("step:__finalize__:0");
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


// ---------------------------------------------------------------------------
// Contract 3: installed message handlers
// ---------------------------------------------------------------------------

import { z } from "zod";
import { registerBlock, unregisterBlockForTest } from "../blocks/registry.ts";

describe("interpretAutomation — installed message handlers (contract 3)", () => {
  const TYPE = "system.test_relay";

  const relayDefinition = (config: Record<string, unknown>): AutomationDefinition => ({
    engine: 1,
    trigger: { kind: "manual" },
    blocks: [
      { id: "launch", type: "create_session", config: { profileId: "p", promptTemplate: "go" } },
      { id: "relay", type: TYPE, config },
      {
        id: "turn",
        type: "send_prompt",
        config: {
          session: { blockId: "launch" },
          promptTemplate: "ping",
          waitFor: { kind: "run_end" },
          deadlineSeconds: 600,
        },
      },
    ],
    inputsSchema: [],
    settings: { endSessionsOnFinish: false },
  });

  test("every message after install is offered to the handler first, in its own step; consumed ones never reach the wait", async () => {
    const seen: AutomationInbox[] = [];
    registerBlock<{ consume: string[] }>({
      type: TYPE,
      system: true,
      configSchema: z.object({ consume: z.array(z.string()) }),
      async execute() {
        return { kind: "ok", outputs: { installed: true } };
      },
      async onMessage(msg, config) {
        seen.push(msg);
        return { verdict: config.consume.includes(msg.kind) ? "consumed" : "pass" };
      },
    });
    try {
      // Two curated events (consumed by the relay) interleave with the two
      // idles the wait needs (create turn = stale, prompt turn = match).
      const curated = (n: number): AutomationInbox => ({
        kind: "session_event",
        sessionId: "s-launch",
        event: { idx: BigInt(n), kind: "agent_message", payloadJson: "{}" },
      });
      const h = makeHarness(relayDefinition({ consume: ["session_event"] }), {
        recv: [
          curated(1),
          { kind: "session_idle", sessionId: "s-launch" },
          curated(2),
          { kind: "session_idle", sessionId: "s-launch" },
        ],
      });
      const result = await interpretAutomation(RUN, h.deps);
      expect(result.status).toBe("completed");
      expect(h.names).toEqual([
        "step:__snapshot__:0",
        "step:launch:0",
        "step:relay:0",
        "step:turn:0",
        "step:relay.__relay__:1",
        "step:turn:clock:1",
        "step:relay.__relay__:2",
        "step:turn:clock:2",
        "step:relay.__relay__:3",
        "step:turn:clock:3",
        "step:relay.__relay__:4",
        "step:turn:0:wait",
        "step:__finalize__:0",
      ]);
      // The handler saw all four; it consumed the curated two and passed the
      // idles through to the wait (stale first, then the match).
      expect(seen.map((m) => m.kind)).toEqual([
        "session_event",
        "session_idle",
        "session_event",
        "session_idle",
      ]);
    } finally {
      unregisterBlockForTest(TYPE);
    }
  });

  test("a ledger blip AFTER a successful handler call never loses the result: state carries, the run continues", async () => {
    // `deps.step` is DBOS.runStep with no retries, and the run's catch turns
    // a throw into a failed run — so the ledger row is best-effort
    // observability; the step's return value is the durable truth.
    let calls = 0;
    const seenStates: unknown[] = [];
    registerBlock<Record<string, never>>({
      type: TYPE,
      system: true,
      configSchema: z.object({}),
      async execute() {
        return { kind: "ok", outputs: { handler_state: { n: 0 } } };
      },
      async onMessage(_msg, _config, ctx) {
        calls += 1;
        seenStates.push(ctx.handlerState);
        return { verdict: "pass", state: { n: calls } };
      },
    });
    try {
      const h = makeHarness(relayDefinition({}), {
        recv: [
          { kind: "session_idle", sessionId: "s-launch" },
          { kind: "session_idle", sessionId: "s-launch" },
        ],
        failRecordStepFor: ["relay.__relay__"],
      });
      const result = await interpretAutomation(RUN, h.deps);
      expect(result.status).toBe("completed");
      expect(calls).toBe(2);
      // The second call saw the state the first one returned: nothing was
      // discarded by the ledger failure.
      expect(seenStates).toEqual([{ n: 0 }, { n: 1 }]);
      expect(h.stepRecords.filter((r) => r.framePath === "relay.__relay__")).toEqual([]);
    } finally {
      unregisterBlockForTest(TYPE);
    }
  });

  test("a throwing handler is recorded on its relay step and never fails the run", async () => {
    registerBlock<Record<string, never>>({
      type: TYPE,
      system: true,
      configSchema: z.object({}),
      async execute() {
        return { kind: "ok", outputs: {} };
      },
      async onMessage() {
        throw new Error("slack exploded");
      },
    });
    try {
      const h = makeHarness(relayDefinition({}), {
        recv: [
          { kind: "session_idle", sessionId: "s-launch" },
          { kind: "session_idle", sessionId: "s-launch" },
        ],
      });
      const result = await interpretAutomation(RUN, h.deps);
      expect(result.status).toBe("completed");
      const relayRows = h.stepRecords.filter((r) => r.framePath === "relay.__relay__");
      expect(relayRows.length).toBe(2);
      expect(relayRows.every((r) => r.record.status === "failed")).toBe(true);
    } finally {
      unregisterBlockForTest(TYPE);
    }
  });
});
