import { describe, expect, test } from "bun:test";

import { DecisionsApiError, type DecisionsRequest, type DecisionsResponse } from "../../../integrations/openrouter-decisions.ts";
import type { ProfileCard } from "../../profile-cards.ts";
import { profileOptions } from "../../profile-cards.ts";
import type { RunSnapshot } from "../context.ts";
import type { DecisionsRuntime, EngineDeps, EngineRunStore, EngineSessionOps, EngineStepRecord } from "../deps.ts";
import type { AutomationDefinition, BlockDef } from "../definition.ts";
import { validateDefinition } from "../definition.ts";
import { interpretAutomation } from "../interpreter.ts";
import { prepareQuestions } from "../blocks/decide.ts";
import { registerEngineBlocks } from "../blocks/index.ts";

registerEngineBlocks();

/** decide + list_profiles driven through the real interpreter with a fake
 * Decisions API: the wire mapping, the fallbacks, and the graph gating on
 * the answers. */

const RUN = { runId: "autorun:auto-1:manual:d1", automationId: "auto-1" };

const CARDS: ProfileCard[] = [
  {
    id: "p-web",
    name: "Web",
    description: "Frontend work in the web app",
    repos: ["acme/web"],
    skills: [],
    integrations: ["github"],
    allowHosts: [],
    envVarNames: [],
  },
  {
    id: "p-infra",
    name: "Infra",
    description: "Terraform and Kubernetes",
    repos: ["acme/deploy"],
    skills: ["kubectl"],
    integrations: [],
    allowHosts: ["*.googleapis.com"],
    envVarNames: ["GOOGLE_PROJECT"],
  },
];

type Reply = DecisionsResponse | Error | ((req: DecisionsRequest) => DecisionsResponse);

function harness(
  blocks: BlockDef[],
  opts: { replies?: Reply[]; connected?: boolean; inputs?: Record<string, unknown> } = {},
) {
  const definition: AutomationDefinition = {
    engine: 1,
    trigger: { kind: "manual" },
    blocks,
    inputsSchema: [],
    settings: { endSessionsOnFinish: false },
  };
  const snapshot: RunSnapshot = {
    definition,
    inputs: opts.inputs ?? {},
    automationId: RUN.automationId,
    automationName: "Decide test",
    version: 1,
    trigger: { kind: "manual", receivedAt: "2026-10-09T00:00:00Z" },
    aliases: [],
    startedAtMs: 1_000_000_000,
  };
  const steps: Array<{ framePath: string; record: EngineStepRecord }> = [];
  const store: EngineRunStore = {
    async loadSnapshot() {
      return snapshot;
    },
    async markRunning() {},
    async recordStep(_runId, framePath, _attempt, record) {
      steps.push({ framePath, record });
    },
    async finalizeRun() {},
    async latestKeptInstanceSession() {
      return null;
    },
    async listRunSessions() {
      return [];
    },
    async adoptSession() {
      return "foreign" as const;
    },
    async getSessionBinding() {
      return null;
    },
    async releaseConcurrency() {
      return null;
    },
  };
  const unused = () => Promise.reject(new Error("unused"));
  const sessions: EngineSessionOps = {
    createSession: unused,
    setSessionRelay: async () => {},
    sendPrompt: unused,
    endSession: unused,
    exec: unused,
    writeFiles: unused,
    getSession: () => Promise.resolve({ found: false as const }),
  };
  const requests: DecisionsRequest[] = [];
  const replies = [...(opts.replies ?? [])];
  const decisions: DecisionsRuntime = {
    connected: async () => opts.connected ?? true,
    async decide(request) {
      requests.push(request);
      const next = replies.shift();
      if (next === undefined) throw new Error("fake decisions exhausted");
      if (next instanceof Error) throw next;
      return typeof next === "function" ? next(request) : next;
    },
  };
  const deps: EngineDeps = {
    step: async (fn) => fn(),
    recv: async () => null,
    store,
    sessions,
    clock: { nowMs: () => 1_000_000_000 },
    decisions,
    profileCards: async () => CARDS,
  };
  const outputs = (path: string) =>
    steps.filter((s) => s.framePath === path).at(-1)?.record.outputs as Record<string, unknown> | undefined;
  return { deps, requests, steps, outputs };
}

function response(answers: DecisionsResponse["answers"]): DecisionsResponse {
  return { model: "typesafe/jev-1.13-20260917", answers, usage: { input_tokens: 90, output_tokens: 0, cost: 0.000004 } };
}

const ROUTE: BlockDef[] = [
  { id: "candidates", type: "list_profiles", config: {} },
  {
    id: "route",
    type: "decide",
    config: {
      state: { message: "${{ inputs.message }}" },
      questions: {
        profile: {
          type: "choice",
          instructions: "Which profile should handle this request?",
          options: { $ref: "steps.candidates.options" },
        },
        severity: { type: "score", instructions: "How urgent is it?", levels: ["low", "medium", "high"] },
        wants_choice: { type: "yes_no", instructions: "Does the person ask to pick the profile?" },
      },
    },
  },
];

describe("decide block", () => {
  test("the definition validates with run-time options and templated state", () => {
    const definition: AutomationDefinition = {
      engine: 1,
      trigger: { kind: "manual" },
      blocks: ROUTE,
      inputsSchema: [],
      settings: { endSessionsOnFinish: false },
    };
    expect(() => validateDefinition(definition)).not.toThrow();
  });

  test("maps labels on the wire back to values, ranked, with one call for every question", async () => {
    const h = harness(ROUTE, {
      inputs: { message: "the deploy pipeline is stuck on terraform apply" },
      replies: [
        response({
          profile: { type: "choice", choice: "Infra", confidence: 0.83, probabilities: { Infra: 0.9, Web: 0.1 } },
          severity: { type: "score", score: 1.7, confidence: 0.6, probabilities: { "0": 0.1, "1": 0.2, "2": 0.7 } },
          wants_choice: { type: "noul", noul: 0.04 },
        }),
      ],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.requests).toHaveLength(1);
    const wire = h.requests[0]!;
    expect(wire.model).toBe("typesafe/jev-1.13");
    expect(wire.state).toEqual({ message: "the deploy pipeline is stuck on terraform apply" });
    expect(wire.questions["profile"]).toEqual({
      type: "choice",
      instructions: "Which profile should handle this request?",
      criteria: {
        Web: { purpose: "Frontend work in the web app", repositories: ["acme/web"], integrations: ["github"] },
        // Purpose facts only: skills, hosts and env var names stay off the wire.
        Infra: { purpose: "Terraform and Kubernetes", repositories: ["acme/deploy"] },
      },
    });
    expect(wire.questions["severity"]).toEqual({
      type: "score",
      instructions: "How urgent is it?",
      criteria: ["low", "medium", "high"],
    });
    expect(wire.questions["wants_choice"]).toEqual({
      type: "noul",
      instructions: "Does the person ask to pick the profile?",
    });
    const out = h.outputs("route")!;
    expect(out["decided"]).toBe(true);
    expect(out["cost"]).toBe(0.000004);
    const answers = out["answers"] as Record<string, Record<string, unknown>>;
    expect(answers["profile"]).toEqual({
      value: "p-infra",
      label: "Infra",
      confidence: 0.83,
      probabilities: { "p-infra": 0.9, "p-web": 0.1 },
      ranked: ["p-infra", "p-web"],
    });
    expect(answers["severity"]).toMatchObject({ score: 1.7, level: 2, level_text: "high" });
    expect(answers["wants_choice"]).toEqual({ yes: 0.04, answer: false });
  });

  test("a branch gates on the answer's confidence", async () => {
    const gated: BlockDef[] = [
      ...ROUTE,
      {
        id: "confident",
        type: "branch",
        config: {
          conditions: {
            mode: "all",
            conditions: [{ path: "steps.route.answers.profile.confidence", op: "gt", value: 0.7 }],
          },
        },
        then: [{ id: "take", type: "code", config: { mode: "value", source: "return 'take';" } }],
        else: [{ id: "ask", type: "code", config: { mode: "value", source: "return 'ask';" } }],
      },
    ];
    const low = harness(gated, {
      inputs: { message: "hi" },
      replies: [
        response({
          profile: { type: "choice", choice: "Web", confidence: 0.41, probabilities: { Web: 0.55, Infra: 0.45 } },
          severity: { type: "score", score: 0, confidence: 0.9 },
          wants_choice: { type: "noul", noul: 0.1 },
        }),
      ],
    });
    // No code runtime in this harness: the gate's arm is visible from which
    // code block the interpreter tried to run.
    await interpretAutomation(RUN, low.deps);
    expect(low.steps.some((s) => s.framePath.endsWith("ask"))).toBe(true);
    expect(low.steps.some((s) => s.framePath.endsWith("take"))).toBe(false);
  });

  test("without OpenRouter the block is undecided and makes no call", async () => {
    const h = harness(ROUTE, { connected: false, inputs: { message: "x" } });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.requests).toHaveLength(0);
    expect(h.outputs("route")).toEqual({ decided: false, reason: "router_not_connected" });
  });

  test("a retryable failure is retried once inside the step", async () => {
    const answers = response({
      profile: { type: "choice", choice: "Web", confidence: 0.9 },
      severity: { type: "score", score: 0 },
      wants_choice: { type: "noul", noul: 0 },
    });
    const h = harness(ROUTE, {
      inputs: { message: "x" },
      replies: [new DecisionsApiError("decisions API 429: slow down", 429, true), answers],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.requests).toHaveLength(2);
    expect((h.outputs("route")!["answers"] as Record<string, { value: string }>)["profile"]!.value).toBe("p-web");
  });

  test("onError undecided turns a failed call into a fallback, not a failed run", async () => {
    const blocks = structuredClone(ROUTE);
    blocks[1]!.config["onError"] = "undecided";
    const h = harness(blocks, {
      inputs: { message: "x" },
      replies: [new DecisionsApiError("decisions API 402: no credits", 402, false)],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.requests).toHaveLength(1);
    expect(h.outputs("route")).toEqual({
      decided: false,
      reason: "decision_failed: decisions API 402: no credits",
    });
  });

  test("by default a failed call fails the block", async () => {
    const h = harness(ROUTE, {
      inputs: { message: "x" },
      replies: [new DecisionsApiError("decisions API 401: bad key", 401, false)],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("failed");
  });

  test("a choice outside the options is refused", async () => {
    const h = harness(ROUTE, {
      inputs: { message: "x" },
      replies: [
        response({
          profile: { type: "choice", choice: "Mobile", confidence: 0.9 },
          severity: { type: "score", score: 0 },
          wants_choice: { type: "noul", noul: 0 },
        }),
      ],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("failed");
    const failed = h.steps.filter((s) => s.framePath === "route").at(-1)!;
    expect(failed.record.error).toContain('not an option of "profile"');
  });

  test("list_profiles restricts to ids", async () => {
    const h = harness([{ id: "candidates", type: "list_profiles", config: { ids: ["p-web", "p-gone"] } }]);
    await interpretAutomation(RUN, h.deps);
    const out = h.outputs("candidates")!;
    expect(out["count"]).toBe(1);
    expect((out["options"] as Array<{ value: string }>).map((o) => o.value)).toEqual(["p-web"]);
  });
});

describe("decide save-time validation", () => {
  test("a templated option value and a templated level save", () => {
    const definition: AutomationDefinition = {
      engine: 1,
      trigger: { kind: "manual" },
      blocks: [
        {
          id: "route",
          type: "decide",
          config: {
            state: "x",
            questions: {
              team: {
                type: "choice",
                instructions: "Which team?",
                options: [{ value: "${{ inputs.team }}", label: "Web" }, { value: "infra" }],
              },
              risk: { type: "score", instructions: "How risky?", levels: ["low", "${{ inputs.high }}"] },
            },
          },
        },
      ],
      inputsSchema: [],
      settings: { endSessionsOnFinish: false },
    };
    expect(() => validateDefinition(definition)).not.toThrow();
  });
});

describe("prepareQuestions", () => {
  test("two options with one value are refused (their probabilities would merge)", () => {
    expect(
      prepareQuestions({
        q: { type: "choice", instructions: "pick", options: [{ value: "a", label: "A" }, { value: "a", label: "B" }] },
      }),
    ).toEqual({ ok: false, error: 'choice question "q" repeats the option value "a"' });
  });

  test("a label that is an object-prototype key is still sent as an option", () => {
    const prepared = prepareQuestions({
      q: { type: "choice", instructions: "pick", options: [{ value: "p", label: "__proto__" }, { value: "o" }] },
    });
    if (!prepared.ok) throw new Error(prepared.error);
    const criteria = (prepared.value.questions["q"] as { criteria: Record<string, unknown> }).criteria;
    expect(Object.keys(JSON.parse(JSON.stringify(criteria)))).toEqual(["__proto__", "o"]);
  });

  test("refuses what save-time validation cannot see", () => {
    expect(prepareQuestions({ q: { type: "choice", instructions: "pick" } })).toEqual({
      ok: false,
      error: 'choice question "q" has no options',
    });
    expect(prepareQuestions({ q: { type: "yes_no" } })).toEqual({
      ok: false,
      error: 'question "q" has no instructions',
    });
    expect(prepareQuestions({ q: { type: "yes_no", instructions: "ok?", yes: "fine" } })).toMatchObject({
      ok: false,
    });
    expect(
      prepareQuestions({
        q: { type: "choice", instructions: "pick", options: [{ value: "a", label: "A" }, { value: "b", label: "A" }] },
      }),
    ).toEqual({ ok: false, error: 'choice question "q" repeats the option label "A"' });
  });

  test("yes_no with both sides sends noul criteria", () => {
    const prepared = prepareQuestions({ q: { type: "yes_no", instructions: "spam?", yes: "an ad", no: "a real request" } });
    expect(prepared).toMatchObject({
      ok: true,
      value: { questions: { q: { type: "noul", criteria: { true: "an ad", false: "a real request" } } } },
    });
  });
});

describe("profileOptions", () => {
  test("makes duplicate names unique and drops empty facts", () => {
    const options = profileOptions([
      { ...CARDS[0]!, id: "a", name: "Dev", description: " ", repos: [], integrations: [] },
      { ...CARDS[0]!, id: "b", name: "Dev" },
    ]);
    expect(options[0]).toEqual({ value: "a", label: "Dev" });
    expect(options[1]!.label).toBe("Dev (2)");
  });

  test("a suffix never collides with a name that already has one", () => {
    const labels = profileOptions([
      { ...CARDS[0]!, id: "a", name: "Dev" },
      { ...CARDS[0]!, id: "b", name: "Dev" },
      { ...CARDS[0]!, id: "c", name: "Dev (2)" },
    ]).map((o) => o.label);
    expect(new Set(labels).size).toBe(3);
  });
});
