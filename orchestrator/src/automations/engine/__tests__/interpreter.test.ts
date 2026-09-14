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
import { registerBlock, unregisterBlockForTest } from "../blocks/registry.ts";
import { z } from "zod";

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
  /** Per promoted id: was startQueuedRun invoked from INSIDE a step? Must
   * always be false — production startQueuedRun is DBOS.startWorkflow, and
   * DBOS rejects a workflow start from step context. */
  promotedInStep: boolean[];
  adopted: Array<{ runId: string; sessionId: string }>;
  actions: Array<{ actionId: string; stepPath: string; params: Record<string, unknown> }>;
  state: Map<string, { value: unknown; version: number; writer: string }>;
  closes: Array<{ instanceId: string; reason?: string }>;
  claims: Array<{
    automationId: string;
    handle: string;
    instanceId: string;
    writtenBy: string;
    allowTakeoverFromClosed?: boolean;
  }>;
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
    dryRun?: boolean;
    /** Seed the fake state store: key -> {value, version, writer}. */
    stateEntries?: Record<string, { value: unknown; version: number; writer: string }>;
    /** Leave deps.state undefined (the unavailable path). */
    noStateStore?: boolean;
    /** D9: the entrypoint recorded in the snapshot. */
    entrypointId?: string;
    /** Session bindings visible to adoptSession/getSessionBinding:
     * sessionId -> {automationId, runId, ownerTerminal}. */
    bindings?: Record<
      string,
      { automationId: string; runId: string; ownerTerminal: boolean; instanceId?: string }
    >;
    /** getSession probe results by session id (absent = found: false). */
    probes?: Record<
      string,
      { status: string; lastActiveAt: string; lastEventAt: string | null }
    >;
    /** ADR 0120: bind the run to a workstream (state auto-scopes). */
    instanceId?: string;
    /** Pre-owned handles for claim_handle conflict cases. */
    handleOwners?: Record<string, string>;
    /** Instances the fake treats as CLOSED (claim takeover cases). */
    closedInstanceIds?: string[];
    /** pr_ref lookups: "repo#number" -> the authoring session. */
    prRefs?: Record<
      string,
      { sessionId: string; taskId: string | null; headBranch: string; url: string; title: string }
    >;
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
  const promotedInStep: boolean[] = [];
  let stepDepth = 0;
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
    ...(options.dryRun ? { dryRun: true } : {}),
    ...(options.entrypointId !== undefined ? { entrypointId: options.entrypointId } : {}),
    ...(options.instanceId !== undefined
      ? { instanceId: options.instanceId, instanceKey: `key-${options.instanceId}` }
      : {}),
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
    async adoptSession({ runId, automationId, sessionId, instanceId }) {
      const binding = bindings[sessionId];
      if (!binding || binding.automationId !== automationId) return "foreign";
      if ((binding.instanceId ?? "") !== instanceId) return "foreign";
      if (binding.runId === runId) return "already_ours";
      if (!binding.ownerTerminal) return "owner_live";
      binding.runId = runId;
      binding.ownerTerminal = false;
      adopted.push({ runId, sessionId });
      return "adopted";
    },
    async getSessionBinding(sessionId) {
      const binding = bindings[sessionId];
      return binding ? { instanceId: "", ...binding } : null;
    },
  };

  const sessions: EngineSessionOps = {
    async getSession(sessionId) {
      const probe = options.probes?.[sessionId];
      return probe ? { found: true, ...probe } : { found: false };
    },
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

  const bindings: Record<
    string,
    { automationId: string; runId: string; ownerTerminal: boolean; instanceId?: string }
  > =
    structuredClone(options.bindings ?? {});
  const adopted: Array<{ runId: string; sessionId: string }> = [];
  const closes: Array<{ instanceId: string; reason?: string }> = [];
  const closedInstances = new Set<string>(options.closedInstanceIds ?? []);
  const claims: Array<{
    automationId: string;
    handle: string;
    instanceId: string;
    writtenBy: string;
    allowTakeoverFromClosed?: boolean;
  }> = [];
  const handleOwners = new Map<string, string>(Object.entries(options.handleOwners ?? {}));

  const state = new Map<string, { value: unknown; version: number; writer: string }>(
    Object.entries(options.stateEntries ?? {}),
  );
  const fakeState: NonNullable<EngineDeps["state"]> = {
    async get(_automationId, key) {
      const entry = state.get(key);
      return entry ? { key, ...entry } : null;
    },
    async set(_automationId, key, value, opts) {
      const entry = state.get(key);
      if (opts.expectVersion !== undefined && (entry?.version ?? 0) !== opts.expectVersion) {
        return { ok: false, current: entry ? { key, ...entry } : null };
      }
      const version = (entry?.version ?? 0) + 1;
      state.set(key, { value, version, writer: opts.writer });
      return { ok: true, version };
    },
    async delete(_automationId, key, opts) {
      const entry = state.get(key);
      if (!entry) return { ok: true, deleted: false };
      if (opts.expectVersion !== undefined && entry.version !== opts.expectVersion) {
        return { ok: false, current: { key, ...entry } };
      }
      state.delete(key);
      return { ok: true, deleted: true };
    },
    async list(_automationId, opts) {
      const entries = [...state.entries()]
        .filter(([key]) => (opts?.prefix ? key.startsWith(opts.prefix) : true))
        .sort(([a], [b]) => (a < b ? -1 : 1))
        .map(([key, entry]) => ({ key, ...entry }));
      const limit = opts?.limit ?? 500;
      return { entries: entries.slice(0, limit), truncated: entries.length > limit };
    },
  };

  const deps: EngineDeps = {
    step: async (fn, name) => {
      names.push(name);
      stepDepth += 1;
      try {
        return await fn();
      } finally {
        stepDepth -= 1;
      }
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
      promotedInStep.push(stepDepth > 0);
    },
    integrationActions: {
      async execute(input) {
        if (options.failActions) throw new Error("provider down");
        actions.push({ actionId: input.actionId, stepPath: input.stepPath, params: input.params });
        return { comment_id: 7 };
      },
    },
  };

  if (!options.noStateStore) deps.state = fakeState;
  deps.instances = {
    async closeInstance(input) {
      closes.push(input);
      const first = !closedInstances.has(input.instanceId);
      closedInstances.add(input.instanceId);
      return first;
    },
    async recordInstanceHandle(input) {
      claims.push(input);
      const holder = handleOwners.get(input.handle);
      if (holder === undefined) {
        handleOwners.set(input.handle, input.instanceId);
        return { kind: "recorded" };
      }
      if (holder === input.instanceId) return { kind: "already_ours" };
      if (input.allowTakeoverFromClosed === true && closedInstances.has(holder)) {
        handleOwners.set(input.handle, input.instanceId);
        return { kind: "reclaimed", from: holder };
      }
      return { kind: "conflict", instanceId: holder };
    },
  };
  if (options.prRefs) {
    const refs = options.prRefs;
    deps.prRefs = {
      async getByPr(repo, prNumber) {
        return refs[`${repo}#${prNumber}`] ?? null;
      },
    };
  }
  return { deps, names, stepRecords, finalized, ended, created, createdInputs, prompts, execs, released, promoted, promotedInStep, actions, state, adopted, closes, claims };
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
    expect(h.promotedInStep).toEqual([false]);
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

  test("a dry run walks the whole graph without a single side effect", async () => {
    // Every side-effecting block stubs itself; waits resolve at once (no
    // session exists to signal, no event is routed); the run completes.
    const definition = makeDefinition([
      {
        id: "launch",
        type: "create_session",
        config: { profileId: "p", promptTemplate: "go ${{ inputs.x }}", titleTemplate: "T" },
      },
      { id: "tick", type: "run_command", config: { session: { blockId: "launch" }, commandTemplate: "make" } },
      {
        id: "files",
        type: "write_files",
        config: { session: { blockId: "launch" }, files: [{ path: "/w/a.txt", contentTemplate: "hi" }] },
      },
      {
        id: "ask",
        type: "send_prompt",
        config: { session: { blockId: "launch" }, promptTemplate: "more", waitFor: { kind: "run_end" } },
      },
      { id: "settle", type: "wait_session", config: { session: { blockId: "launch" }, until: "idle" } },
      {
        id: "turns",
        type: "loop",
        config: {
          maxIterations: 5,
          until: { mode: "all", conditions: [{ path: "steps.next.outcome", op: "equals", value: "deadline" }] },
        },
        body: [
          {
            id: "next",
            type: "wait_event",
            config: { eventKeys: ["x.y"], deadlineSeconds: 60, onDeadline: "continue" },
          },
        ],
      },
      {
        id: "post",
        type: "integration_action",
        config: { provider: "github", actionId: "create_issue_comment", params: { body: "b" } },
      },
      { id: "bye", type: "end_session", config: { session: { blockId: "launch" } } },
    ]);
    const h = makeHarness(definition, { dryRun: true, inputs: { x: "1" } });
    const result = await interpretAutomation(RUN, h.deps);

    expect(result.status).toBe("completed");
    expect(h.created).toEqual([]);
    expect(h.execs).toEqual([]);
    expect(h.prompts).toEqual([]);
    expect(h.ended).toEqual([]);
    expect(h.actions).toEqual([]);
    const outputs = Object.fromEntries(h.stepRecords.map((r) => [r.framePath, r.record.outputs ?? {}]));
    expect(outputs["launch"]).toMatchObject({
      session_id: `dry-run:${RUN.runId}:launch`,
      prompt: "go 1",
      dry_run: true,
    });
    expect(outputs["tick"]).toMatchObject({ dry_run: true, would_execute: { command: "make" }, exit_status: 0 });
    expect(outputs["files"]).toMatchObject({ would_execute: { files: [{ path: "/w/a.txt", chars: 2 }] } });
    expect(outputs["ask"]).toMatchObject({ sent: false, would_execute: { prompt: "more" }, outcome: "completed" });
    expect(outputs["settle"]).toMatchObject({ outcome: "completed", dry_run: true });
    // The event wait "expired" at once, so the loop ran exactly once.
    expect(outputs["turns[0].next"]).toMatchObject({ outcome: "deadline" });
    expect(h.names.filter((n) => n.startsWith("step:turns["))).toEqual([
      "step:turns[0].next:0",
      "step:turns[0].next:0:wait",
      "step:turns[0].__until__:0",
    ]);
    expect(outputs["bye"]).toMatchObject({ ended: false, dry_run: true });
  });

  test("a dry run refuses a block with product side effects instead of half-running the product", async () => {
    registerBlock({
      type: "test_effect",
      refusesDryRun: true,
      configSchema: z.object({}),
      async execute() {
        throw new Error("must not run");
      },
    });
    try {
      const definition = makeDefinition([{ id: "fx", type: "test_effect", config: {} }]);
      const h = makeHarness(definition, { dryRun: true });
      const result = await interpretAutomation(RUN, h.deps);
      expect(result.status).toBe("failed");
      expect(result.error).toContain("dry_run_unsupported");
      expect(h.stepRecords.find((r) => r.framePath === "fx")?.record.status).toBe("failed");
    } finally {
      unregisterBlockForTest("test_effect");
    }
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
    // From WORKFLOW context, never from inside the finalize step: production
    // startQueuedRun is DBOS.startWorkflow, which DBOS rejects from a step
    // ("Invalid call to a `workflow` function from within a `step`"). When
    // it ran inside the step, the claim CAS committed but the successor
    // never got a workflow — it held the key forever and the queue jammed
    // behind it (prod 2026-08-26).
    expect(h.promotedInStep).toEqual([false]);
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


describe("interpretAutomation — installed message handlers (contract 3)", () => {
  const TYPE = "test_relay";

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

describe("state blocks (ADR 0119 D10)", () => {
  test("set - get - list - delete round trip; keys template and values take $refs", async () => {
    const definition = makeDefinition([
      {
        id: "save",
        type: "state_set",
        config: {
          key: "ticket:${{ event.raw.id }}",
          value: { $ref: "event.raw.doc" },
        },
      },
      { id: "load", type: "state_get", config: { key: "ticket:${{ event.raw.id }}" } },
      { id: "scan", type: "state_list", config: { prefix: "ticket:" } },
      { id: "drop", type: "state_delete", config: { key: "ticket:${{ event.raw.id }}" } },
      { id: "gone", type: "state_get", config: { key: "ticket:${{ event.raw.id }}" } },
    ]);
    const h = makeHarness(definition, {
      payload: { id: "ENG-1", doc: { session_id: "s-1", pr: null } },
    });
    const result = await interpretAutomation(RUN, h.deps);

    expect(result.error).toBeUndefined();
    expect(result.status).toBe("completed");
    const outputs = Object.fromEntries(h.stepRecords.map((r) => [r.framePath, r.record.outputs ?? {}]));
    expect(outputs["save"]).toMatchObject({ ok: true, version: 1 });
    expect(outputs["load"]).toMatchObject({
      found: true,
      value: { session_id: "s-1", pr: null },
      version: 1,
    });
    expect(outputs["scan"]).toMatchObject({ count: 1, truncated: false });
    expect(outputs["drop"]).toMatchObject({ ok: true, deleted: true });
    expect(outputs["gone"]).toMatchObject({ found: false, value: null, version: 0 });
  });

  test("a CAS miss is an output the graph branches on, never an error", async () => {
    const definition = makeDefinition([
      {
        id: "claim",
        type: "state_set",
        config: { key: "sweep:cursor", value: { at: 2 }, expectVersion: 3 },
      },
      {
        id: "lost",
        type: "filter",
        config: {
          conditions: { mode: "all", conditions: [{ path: "steps.claim.ok", op: "is_false" }] },
        },
      },
    ]);
    const h = makeHarness(definition, {
      stateEntries: { "sweep:cursor": { value: { at: 1 }, version: 5, writer: "other" } },
    });
    const result = await interpretAutomation(RUN, h.deps);

    expect(result.status).toBe("completed");
    const outputs = Object.fromEntries(h.stepRecords.map((r) => [r.framePath, r.record.outputs ?? {}]));
    expect(outputs["claim"]).toMatchObject({
      ok: false,
      current_version: 5,
      current_value: { at: 1 },
    });
    expect(h.state.get("sweep:cursor")?.version).toBe(5);
  });

  test("writes stamp the frame-path writer tag", async () => {
    const definition = makeDefinition([
      { id: "save", type: "state_set", config: { key: "k", value: 1 } },
    ]);
    const h = makeHarness(definition);
    await interpretAutomation(RUN, h.deps);
    expect(h.state.get("k")?.writer).toBe(`${RUN.runId}:save`);
  });

  test("a dry run reads live state but stubs the writes", async () => {
    const definition = makeDefinition([
      { id: "load", type: "state_get", config: { key: "ticket:ENG-9" } },
      { id: "save", type: "state_set", config: { key: "ticket:ENG-9", value: { x: 2 } } },
      { id: "drop", type: "state_delete", config: { key: "ticket:ENG-9" } },
    ]);
    const h = makeHarness(definition, {
      dryRun: true,
      stateEntries: { "ticket:ENG-9": { value: { x: 1 }, version: 4, writer: "w" } },
    });
    const result = await interpretAutomation(RUN, h.deps);

    expect(result.status).toBe("completed");
    const outputs = Object.fromEntries(h.stepRecords.map((r) => [r.framePath, r.record.outputs ?? {}]));
    expect(outputs["load"]).toMatchObject({ found: true, value: { x: 1 }, version: 4 });
    expect(outputs["save"]).toMatchObject({ ok: true, dry_run: true, would_execute: { key: "ticket:ENG-9" } });
    expect(outputs["drop"]).toMatchObject({ ok: true, deleted: false, dry_run: true });
    expect(h.state.get("ticket:ENG-9")?.value).toEqual({ x: 1 });
  });

  test("a missing state store is a typed non-retryable failure", async () => {
    const definition = makeDefinition([
      { id: "load", type: "state_get", config: { key: "k" } },
    ]);
    const h = makeHarness(definition, { noStateStore: true });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("failed");
    expect(result.error).toContain("state_store_unavailable");
  });
});

describe("session adoption + session_status (ADR 0119 D11)", () => {
  const BINDING_TERMINAL = {
    "s-kept": { automationId: "auto-1", runId: "autorun:auto-1:old", ownerTerminal: true },
  };

  test("a template session ref adopts a terminal-run session, then prompts and waits route here", async () => {
    const definition = makeDefinition([
      {
        id: "nudge",
        type: "send_prompt",
        config: {
          session: { template: "${{ event.raw.session_id }}" },
          promptTemplate: "review feedback arrived",
          waitFor: { kind: "none" },
        },
      },
    ]);
    const h = makeHarness(definition, {
      payload: { session_id: "s-kept" },
      bindings: structuredClone(BINDING_TERMINAL),
    });
    const result = await interpretAutomation(RUN, h.deps);

    expect(result.status).toBe("completed");
    expect(h.adopted).toEqual([{ runId: RUN.runId, sessionId: "s-kept" }]);
    expect(h.prompts.map((prompt) => prompt.sessionId)).toEqual(["s-kept"]);
  });

  test("adoption never steals from a live run: the block fails, typed and loud", async () => {
    const definition = makeDefinition([
      {
        id: "nudge",
        type: "send_prompt",
        config: {
          session: { template: "s-kept" },
          promptTemplate: "hi",
          waitFor: { kind: "none" },
        },
      },
    ]);
    const h = makeHarness(definition, {
      bindings: {
        "s-kept": { automationId: "auto-1", runId: "autorun:auto-1:live", ownerTerminal: false },
      },
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("failed");
    expect(result.error).toContain("never steals");
    expect(h.prompts).toEqual([]);
  });

  test("the binding row is the ownership boundary: an unbound id is refused", async () => {
    const definition = makeDefinition([
      {
        id: "bye",
        type: "end_session",
        config: { session: { template: "s-someone-elses" } },
      },
    ]);
    const h = makeHarness(definition, {});
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("failed");
    expect(result.error).toContain("not bound to this automation");
    expect(h.ended).toEqual([]);
  });

  test("session_status probes without adopting; unbound and gone are values", async () => {
    const definition = makeDefinition([
      { id: "probe", type: "session_status", config: { session: { template: "s-kept" } } },
      { id: "stale", type: "session_status", config: { session: { template: "s-unknown" } } },
      { id: "swept", type: "session_status", config: { session: { template: "s-gone" } } },
    ]);
    const h = makeHarness(definition, {
      bindings: {
        "s-kept": { automationId: "auto-1", runId: "autorun:auto-1:live", ownerTerminal: false },
        "s-gone": { automationId: "auto-1", runId: "autorun:auto-1:old", ownerTerminal: true },
      },
      probes: {
        "s-kept": {
          status: "idle",
          lastActiveAt: "2001-09-09T01:40:00Z",
          lastEventAt: "2001-09-09T01:45:40Z",
        },
      },
    });
    const result = await interpretAutomation(RUN, h.deps);

    expect(result.status).toBe("completed");
    const outputs = Object.fromEntries(h.stepRecords.map((r) => [r.framePath, r.record.outputs ?? {}]));
    // Harness clock starts at 1_000_000_000 ms and ticks 60s per step; the
    // probe only asserts shape + owner_run_live, and that idle_seconds is a
    // number derived from last_event_at.
    expect(outputs["probe"]).toMatchObject({
      found: true,
      session_id: "s-kept",
      status: "idle",
      owner_run_live: true,
    });
    expect(typeof (outputs["probe"] as { idle_seconds: unknown }).idle_seconds).toBe("number");
    expect(outputs["stale"]).toMatchObject({ found: false, reason: "unbound" });
    expect(outputs["swept"]).toMatchObject({ found: false, session_id: "s-gone", reason: "gone" });
    // No probe ever re-binds.
    expect(h.adopted).toEqual([]);
  });

  test("a dry run resolves template refs without touching bindings", async () => {
    const definition = makeDefinition([
      {
        id: "nudge",
        type: "send_prompt",
        config: {
          session: { template: "s-kept" },
          promptTemplate: "hi",
          waitFor: { kind: "none" },
        },
      },
    ]);
    const h = makeHarness(definition, { dryRun: true, bindings: structuredClone(BINDING_TERMINAL) });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.adopted).toEqual([]);
    expect(h.prompts).toEqual([]);
  });
});

describe("entrypoint walks (ADR 0119 D9)", () => {
  const twoWays = (): AutomationDefinition => ({
    ...makeDefinition([
      { id: "launch", type: "create_session", config: { profileId: "p", promptTemplate: "go" } },
    ]),
    entrypoints: [
      {
        id: "sweep",
        trigger: { kind: "manual" },
        blocks: [{ id: "note", type: "state_set", config: { key: "swept", value: true } }],
      },
    ],
  });

  test("a run with an entrypointId walks ONLY that entrypoint's blocks", async () => {
    const h = makeHarness(twoWays(), { entrypointId: "sweep" });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.created).toEqual([]); // main's create_session never ran
    expect(h.state.get("swept")?.value).toBe(true);
    expect(
      h.stepRecords.filter((r) => r.record.status === "succeeded").map((r) => r.framePath),
    ).toEqual(["note"]);
  });

  test("no entrypointId (an old checkpointed snapshot) walks main", async () => {
    const h = makeHarness(twoWays());
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.created).toEqual(["s-launch"]);
    expect(h.state.size).toBe(0);
  });

  test("an entrypoint missing from the pinned version fails loudly", async () => {
    const h = makeHarness(twoWays(), { entrypointId: "renamed_away" });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("failed");
    expect(result.error).toContain("renamed_away");
  });
});

describe("lookup_pr_session (pr_ref ledger)", () => {
  test("maps a PR to its authoring session; not-found is a value; feedback routes via adoption", async () => {
    const definition = makeDefinition([
      {
        id: "who",
        type: "lookup_pr_session",
        config: {
          repo: "${{ event.raw.repo }}",
          prNumber: { $ref: "event.raw.pr" },
        },
      },
      {
        id: "known",
        type: "filter",
        config: { conditions: { mode: "all", conditions: [{ path: "steps.who.found", op: "is_true" }] } },
      },
      {
        id: "nudge",
        type: "send_prompt",
        config: {
          session: { template: "${{ steps.who.session_id }}" },
          promptTemplate: "review feedback arrived",
          waitFor: { kind: "none" },
        },
      },
    ]);
    const h = makeHarness(definition, {
      payload: { repo: "acme/repo", pr: 42 },
      prRefs: {
        "acme/repo#42": {
          sessionId: "s-impl",
          taskId: "t-1",
          headBranch: "ticket-eng-1",
          url: "https://github.com/acme/repo/pull/42",
          title: "Implement ENG-1",
        },
      },
      bindings: {
        "s-impl": { automationId: "auto-1", runId: "autorun:auto-1:old", ownerTerminal: true },
      },
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    const outputs = Object.fromEntries(h.stepRecords.map((r) => [r.framePath, r.record.outputs ?? {}]));
    expect(outputs["who"]).toMatchObject({ found: true, session_id: "s-impl", head_branch: "ticket-eng-1" });
    expect(h.adopted).toEqual([{ runId: RUN.runId, sessionId: "s-impl" }]);
    expect(h.prompts.map((prompt) => prompt.sessionId)).toEqual(["s-impl"]);
  });

  test("an unmapped PR filters the run instead of failing", async () => {
    const definition = makeDefinition([
      { id: "who", type: "lookup_pr_session", config: { repo: "acme/repo", prNumber: 7 } },
      {
        id: "known",
        type: "filter",
        config: { conditions: { mode: "all", conditions: [{ path: "steps.who.found", op: "is_true" }] } },
      },
    ]);
    const h = makeHarness(definition, { prRefs: {} });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("filtered");
  });

  test("a missing lookup seam is a typed non-retryable failure", async () => {
    const definition = makeDefinition([
      { id: "who", type: "lookup_pr_session", config: { repo: "acme/repo", prNumber: 7 } },
    ]);
    const h = makeHarness(definition);
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("failed");
    expect(result.error).toContain("pr_ref_lookup_unavailable");
  });
});

describe("instance-scoped state (ADR 0120)", () => {
  test("an instance-bound run reads and writes under its own prefix, transparently", async () => {
    const definition = makeDefinition([
      { id: "save", type: "state_set", config: { key: "plan", value: { step: 1 } } },
      { id: "load", type: "state_get", config: { key: "plan" } },
      { id: "all", type: "state_list", config: {} },
    ]);
    const h = makeHarness(definition, {
      instanceId: "ai_one",
      // A sibling workstream's document AND an automation-scoped document:
      // neither may leak into this run's view.
      stateEntries: {
        "i/ai_two/plan": { value: { step: 9 }, version: 3, writer: "other" },
        plan: { value: { step: 0 }, version: 5, writer: "global" },
      },
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");

    // The write landed under the instance prefix; the raw map shows it.
    expect(h.state.get("i/ai_one/plan")).toMatchObject({ value: { step: 1 }, version: 1 });
    expect(h.state.get("plan")).toMatchObject({ value: { step: 0 }, version: 5 });

    const outputs = Object.fromEntries(h.stepRecords.map((r) => [r.framePath, r.record.outputs ?? {}]));
    expect(outputs["load"]).toMatchObject({ found: true, value: { step: 1 } });
    // list sees ONLY this workstream's documents, with the prefix stripped.
    const listed = outputs["all"] as { entries: Array<{ key: string }> };
    expect(listed.entries.map((e) => e.key)).toEqual(["plan"]);
  });

  test("an unbound run of the same automation stays automation-scoped", async () => {
    const definition = makeDefinition([
      { id: "load", type: "state_get", config: { key: "plan" } },
    ]);
    const h = makeHarness(definition, {
      stateEntries: { plan: { value: { step: 0 }, version: 5, writer: "global" } },
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    const outputs = Object.fromEntries(h.stepRecords.map((r) => [r.framePath, r.record.outputs ?? {}]));
    expect(outputs["load"]).toMatchObject({ found: true, value: { step: 0 } });
  });

  test("cross-instance adoption classifies as foreign", async () => {
    const definition = makeDefinition([
      {
        id: "nudge",
        type: "send_prompt",
        config: {
          session: { template: "s-owned" },
          promptTemplate: "hello",
          waitFor: { kind: "none" },
        },
      },
    ]);
    const h = makeHarness(definition, {
      instanceId: "ai_one",
      bindings: {
        "s-owned": {
          automationId: "auto-1",
          runId: "autorun:auto-1:old",
          ownerTerminal: true,
          instanceId: "ai_two",
        },
      },
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("failed");
    expect(result.error).toContain("not bound to this automation");
  });
});

describe("instance_close block (ADR 0120)", () => {
  test("closes the run's own workstream with the rendered reason; a second close is still ok", async () => {
    const definition = makeDefinition([
      { id: "done", type: "instance_close", config: { reason: "shipped ${{ inputs.name }}" } },
      { id: "again", type: "instance_close", config: {} },
    ]);
    const h = makeHarness(definition, { instanceId: "ai_one", inputs: { name: "v1" } });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.closes).toEqual([
      { instanceId: "ai_one", reason: "shipped v1" },
      { instanceId: "ai_one" },
    ]);
    const outputs = Object.fromEntries(h.stepRecords.map((r) => [r.framePath, r.record.outputs ?? {}]));
    expect(outputs["done"]).toMatchObject({ closed: true });
    expect(outputs["again"]).toMatchObject({ closed: false });
  });

  test("a non-instanced run no-ops instead of failing; a dry run never closes", async () => {
    const definition = makeDefinition([
      { id: "done", type: "instance_close", config: {} },
    ]);
    const h = makeHarness(definition, {});
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.closes).toEqual([]);
    const outputs = Object.fromEntries(h.stepRecords.map((r) => [r.framePath, r.record.outputs ?? {}]));
    expect(outputs["done"]).toMatchObject({ closed: false, not_instanced: true });

    const dry = makeHarness(definition, { instanceId: "ai_one", dryRun: true });
    const dryResult = await interpretAutomation(RUN, dry.deps);
    expect(dryResult.status).toBe("completed");
    expect(dry.closes).toEqual([]);
  });
});

describe("claim_handle block (ADR 0120)", () => {
  test("claims a rendered handle with provider case-folding; a foreign owner is a typed failure", async () => {
    const definition = makeDefinition([
      { id: "claim", type: "claim_handle", config: { handle: "github:${{ inputs.repo }}#7" } },
    ]);
    const h = makeHarness(definition, { instanceId: "ai_one", inputs: { repo: "Acme/Repo" } });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.claims).toEqual([
      {
        automationId: "auto-1",
        handle: "github:acme/repo#7",
        instanceId: "ai_one",
        writtenBy: `${RUN.runId}:claim`,
        // The explicit claim is the one takeover-capable writer.
        allowTakeoverFromClosed: true,
      },
    ]);

    const conflicted = makeHarness(definition, {
      instanceId: "ai_one",
      inputs: { repo: "Acme/Repo" },
      handleOwners: { "github:acme/repo#7": "ai_other" },
    });
    const failed = await interpretAutomation(RUN, conflicted.deps);
    expect(failed.status).toBe("failed");
    expect(failed.error).toContain("handle_conflict");
  });

  test("a claim takes over a CLOSED holder's handle and reports reclaimed", async () => {
    const definition = makeDefinition([
      { id: "claim", type: "claim_handle", config: { handle: "slack:C0AB" } },
    ]);
    const h = makeHarness(definition, {
      instanceId: "ai_next",
      handleOwners: { "slack:C0AB": "ai_dead" },
      closedInstanceIds: ["ai_dead"],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    const record = h.stepRecords.find(
      (r) => r.framePath === "claim" && r.record.status === "succeeded",
    );
    expect(record?.record.outputs).toMatchObject({
      claimed: true,
      handle: "slack:C0AB",
      reclaimed: true,
    });
  });

  test("an unbound run cannot claim; slack handles stay exact", async () => {
    const definition = makeDefinition([
      { id: "claim", type: "claim_handle", config: { handle: "slack:C0AB" } },
    ]);
    const unbound = makeHarness(definition, {});
    const result = await interpretAutomation(RUN, unbound.deps);
    expect(result.status).toBe("failed");
    expect(result.error).toContain("not_instanced");

    const bound = makeHarness(definition, { instanceId: "ai_one" });
    await interpretAutomation(RUN, bound.deps);
    expect(bound.claims[0]?.handle).toBe("slack:C0AB");
  });
});
