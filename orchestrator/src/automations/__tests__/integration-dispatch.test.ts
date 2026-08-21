import { describe, expect, test } from "bun:test";

import type {
  AutomationMetaRow,
  AutomationRunRow,
  DispatchTarget,
} from "../../db/automations.ts";
import type { AutomationDefinition } from "../engine/definition.ts";
import {
  dispatchIntegrationEvent,
  IntegrationDispatchError,
  matchesIntegrationTrigger,
  scopeValuesFromInput,
  type IntegrationDispatchStore,
  type IntegrationTriggerSpec,
} from "../dispatch.ts";
import type { IntegrationEventDispatchInput } from "../integration-ingress.ts";

const RECEIVED_AT = new Date("2026-08-21T10:00:00Z");

function meta(overrides: Partial<AutomationMetaRow> = {}): AutomationMetaRow {
  return {
    id: "automation-1",
    name: "PR triage",
    description: "",
    enabled: true,
    kind: "user",
    builtinKey: null,
    currentVersion: 2,
    inputs: {},
    endSessionsOnFinish: false,
    createdByUserId: "admin-1",
    nextFireAt: null,
    lastFiredAt: null,
    createdAt: RECEIVED_AT,
    updatedAt: RECEIVED_AT,
    archivedAt: null,
    ...overrides,
  };
}

function trigger(overrides: Partial<IntegrationTriggerSpec> = {}): IntegrationTriggerSpec {
  return {
    kind: "integration",
    provider: "github",
    connectionId: "conn-1",
    eventKeys: ["pull_request.opened"],
    ...overrides,
  };
}

function definition(t: IntegrationTriggerSpec): AutomationDefinition {
  return {
    engine: 1,
    trigger: t,
    blocks: [
      {
        id: "create_session",
        type: "create_session",
        config: { profileId: "p1", promptTemplate: "triage", includeEventContext: false },
      },
    ],
    inputsSchema: [],
    settings: { endSessionsOnFinish: false },
  };
}

function event(
  overrides: Partial<Parameters<typeof matchesIntegrationTrigger>[1]> = {},
): Parameters<typeof matchesIntegrationTrigger>[1] {
  return {
    provider: "github",
    connectionId: "conn-1",
    eventKey: "pull_request.opened",
    ...overrides,
  };
}

const noInputs = () => undefined;

describe("matchesIntegrationTrigger", () => {
  test("provider, connection, and event key must all match", () => {
    expect(matchesIntegrationTrigger(trigger(), event(), noInputs)).toBe(true);
    expect(matchesIntegrationTrigger(trigger(), event({ provider: "slack" }), noInputs)).toBe(false);
    expect(
      matchesIntegrationTrigger(trigger(), event({ connectionId: "conn-2" }), noInputs),
    ).toBe(false);
    expect(
      matchesIntegrationTrigger(trigger(), event({ eventKey: "issues.opened" }), noInputs),
    ).toBe(false);
  });

  test("scope values match the delivery's noun, case-insensitively for github", () => {
    const scoped = trigger({ scope: { values: ["Engrams/Engrams"] } });
    expect(
      matchesIntegrationTrigger(scoped, event({ scopeValue: "engrams/engrams" }), noInputs),
    ).toBe(true);
    expect(
      matchesIntegrationTrigger(scoped, event({ scopeValue: "other/repo" }), noInputs),
    ).toBe(false);
    // No scope value on the delivery → a scoped trigger never fires.
    expect(matchesIntegrationTrigger(scoped, event(), noInputs)).toBe(false);
  });

  test("slack scope compares exactly", () => {
    const scoped = trigger({
      provider: "slack",
      scope: { values: ["C123ABC"] },
    });
    expect(
      matchesIntegrationTrigger(
        scoped,
        event({ provider: "slack", scopeValue: "C123ABC" }),
        noInputs,
      ),
    ).toBe(true);
    expect(
      matchesIntegrationTrigger(
        scoped,
        event({ provider: "slack", scopeValue: "c123abc" }),
        noInputs,
      ),
    ).toBe(false);
  });

  test("fromInput resolves map keys and list elements; unbound input never matches", () => {
    const scoped = trigger({ scope: { fromInput: "repos" } });
    const mapInputs = { repos: { "engrams/engrams": { mode: "auto" } } };
    const listInputs = { repos: ["engrams/engrams"] };
    expect(
      matchesIntegrationTrigger(
        scoped,
        event({ scopeValue: "Engrams/Engrams" }),
        (key) => scopeValuesFromInput(mapInputs, key),
      ),
    ).toBe(true);
    expect(
      matchesIntegrationTrigger(
        scoped,
        event({ scopeValue: "engrams/engrams" }),
        (key) => scopeValuesFromInput(listInputs, key),
      ),
    ).toBe(true);
    expect(
      matchesIntegrationTrigger(
        scoped,
        event({ scopeValue: "engrams/engrams" }),
        (key) => scopeValuesFromInput({}, key),
      ),
    ).toBe(false);
    // A non-string list resolves to nothing.
    expect(scopeValuesFromInput({ repos: [1, 2] }, "repos")).toBeUndefined();
    expect(scopeValuesFromInput({ repos: "one" }, "repos")).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------

interface Harness {
  store: IntegrationDispatchStore;
  starts: Array<{ runId: string; automationId: string; workflowId: string }>;
  runs: Map<string, AutomationRunRow>;
}

function makeHarness(targets: DispatchTarget[]): Harness {
  const runs = new Map<string, AutomationRunRow>();
  const starts: Harness["starts"] = [];
  const claims = new Map<string, string>();

  const store: IntegrationDispatchStore = {
    async listEnabledForIntegrationTrigger() {
      return targets;
    },
    async insertRun(input) {
      if (!runs.has(input.id)) {
        runs.set(input.id, {
          id: input.id,
          automationId: input.automationId,
          version: input.version,
          trigger: input.trigger,
          deliveryKey: input.deliveryKey,
          concurrencyKey: input.concurrencyKey,
          renderedPrompt: null,
          renderedTitle: null,
          taskId: null,
          sessionId: null,
          status: input.status ?? "pending",
          error: input.error ?? null,
          scheduledFor: input.scheduledFor,
          leaseOwner: null,
          leaseExpiresAt: null,
          startedAt: null,
          endedAt: input.endedAt ?? null,
          createdAt: RECEIVED_AT,
        });
      }
      return runs.get(input.id)!;
    },
    async claimConcurrency(automationId, key, runId) {
      const mapKey = `${automationId}:${key}`;
      const holder = claims.get(mapKey);
      if (holder === undefined || holder === runId) {
        claims.set(mapKey, runId);
        return { claimed: true };
      }
      return { claimed: false, holderRunId: holder };
    },
    async casConcurrency(automationId, key, fromRunId, toRunId) {
      const mapKey = `${automationId}:${key}`;
      if (claims.get(mapKey) !== fromRunId) return false;
      claims.set(mapKey, toRunId);
      return true;
    },
  };

  return { store, starts, runs };
}

function input(
  overrides: Partial<IntegrationEventDispatchInput> = {},
): IntegrationEventDispatchInput {
  return {
    provider: "github",
    connectionId: "conn-1",
    eventKey: "pull_request.opened",
    deliveryId: "gh-delivery-1",
    payload: { repository: { full_name: "engrams/engrams" } },
    receivedAt: RECEIVED_AT,
    ...overrides,
  };
}

function deps(h: Harness) {
  return {
    store: h.store,
    workflowStarter: {
      async start(wf: { runId: string; automationId: string }, workflowId: string) {
        h.starts.push({ ...wf, workflowId });
      },
    },
    sender: { async send() {} },
    now: () => RECEIVED_AT,
  };
}

describe("dispatchIntegrationEvent", () => {
  test("starts one run per matching automation with provider-prefixed delivery keys", async () => {
    const matching = { automation: meta(), definition: definition(trigger()) };
    const wrongEvent = {
      automation: meta({ id: "automation-2" }),
      definition: definition(trigger({ eventKeys: ["issues.opened"] })),
    };
    const scopedOut = {
      automation: meta({ id: "automation-3" }),
      definition: definition(trigger({ scope: { values: ["other/repo"] } })),
    };
    const h = makeHarness([matching, wrongEvent, scopedOut]);

    const result = await dispatchIntegrationEvent(
      input({ scopeValue: "engrams/engrams" }),
      deps(h),
    );

    expect(result).toEqual({ matched: 1, started: 1, joined: 0, queued: 0, skipped: 0, failed: 0 });
    expect(h.starts).toEqual([
      {
        runId: "autorun:automation-1:github:gh-delivery-1",
        automationId: "automation-1",
        workflowId: "autorun:automation-1:github:gh-delivery-1",
      },
    ]);
    const run = h.runs.get("autorun:automation-1:github:gh-delivery-1")!;
    expect(run.trigger).toMatchObject({
      source: "integration",
      eventKey: "pull_request.opened",
      deliveryId: "gh-delivery-1",
      scopeValue: "engrams/engrams",
    });
    expect(run.deliveryKey).toBe("github:gh-delivery-1");
  });

  test("a redelivery mints the same run id, so DBOS start and the delivery unique dedupe", async () => {
    const h = makeHarness([{ automation: meta(), definition: definition(trigger()) }]);
    await dispatchIntegrationEvent(input(), deps(h));
    await dispatchIntegrationEvent(input(), deps(h));
    expect(h.runs.size).toBe(1);
    expect(new Set(h.starts.map((s) => s.workflowId)).size).toBe(1);
  });

  test("fromInput scope reads the automation's own inputs", async () => {
    const scoped = {
      automation: meta({ inputs: { repos: { "engrams/engrams": { mode: "auto" } } } }),
      definition: definition(trigger({ scope: { fromInput: "repos" } })),
    };
    const h = makeHarness([scoped]);
    const hit = await dispatchIntegrationEvent(input({ scopeValue: "engrams/engrams" }), deps(h));
    expect(hit.started).toBe(1);

    const miss = await dispatchIntegrationEvent(
      input({ deliveryId: "gh-delivery-2", scopeValue: "other/repo" }),
      deps(h),
    );
    expect(miss).toMatchObject({ matched: 0, started: 0 });
  });

  test("concurrency policies delegate to the shared admission path", async () => {
    const queued = {
      automation: meta(),
      definition: {
        ...definition(trigger()),
        settings: {
          endSessionsOnFinish: false,
          concurrency: { keyTemplate: "fixed", policy: "queue" as const },
        },
      },
    };
    const h = makeHarness([queued]);
    const first = await dispatchIntegrationEvent(input(), deps(h));
    expect(first.started).toBe(1);
    const second = await dispatchIntegrationEvent(input({ deliveryId: "gh-delivery-2" }), deps(h));
    expect(second).toMatchObject({ queued: 1, started: 0 });
    const pending = h.runs.get("autorun:automation-1:github:gh-delivery-2")!;
    expect(pending.status).toBe("pending");
  });

  test("one target's admission fault never drops its siblings, and the delivery fails afterwards", async () => {
    const a = { automation: meta({ id: "automation-a" }), definition: definition(trigger()) };
    const b = { automation: meta({ id: "automation-b" }), definition: definition(trigger()) };
    const c = { automation: meta({ id: "automation-c" }), definition: definition(trigger()) };
    const h = makeHarness([a, b, c]);
    const d = deps(h);
    // The middle target's workflow start throws (transient DBOS fault).
    d.workflowStarter = {
      async start(wf, workflowId) {
        if (wf.automationId === "automation-b") throw new Error("dbos unavailable");
        h.starts.push({ ...wf, workflowId });
      },
    };

    let thrown: unknown;
    try {
      await dispatchIntegrationEvent(input(), d);
    } catch (error) {
      thrown = error;
    }

    // a and c were still admitted; only b failed.
    expect(h.starts.map((s) => s.automationId).sort()).toEqual(["automation-a", "automation-c"]);
    expect(thrown).toBeInstanceOf(IntegrationDispatchError);
    const err = thrown as IntegrationDispatchError;
    expect(err.result).toMatchObject({ matched: 3, started: 2, failed: 1 });
    expect(err.failures.map((f) => f.automationId)).toEqual(["automation-b"]);

    // The provider's retry replays the same delivery: a and c dedupe on
    // their fixed run ids, b gets its second chance.
    d.workflowStarter = {
      async start(wf, workflowId) {
        h.starts.push({ ...wf, workflowId });
      },
    };
    const retry = await dispatchIntegrationEvent(input(), d);
    expect(retry.failed).toBe(0);
    expect(new Set(h.starts.map((s) => s.workflowId)).size).toBe(3);
  });
});
