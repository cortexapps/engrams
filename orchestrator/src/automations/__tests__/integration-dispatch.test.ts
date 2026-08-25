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
    blockOverrides: {},
    endSessionsOnFinish: false,
    createdByUserId: "admin-1",
    nextFireAt: null,
    lastFiredAt: null,
    draftSessionId: null,
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
  claims: Map<string, string>;
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
          entrypointId: input.entrypointId ?? "main",
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
          dryRun: input.dryRun ?? false,
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
    async getConcurrencyHolder(automationId, key) {
      return claims.get(`${automationId}:${key}`) ?? null;
    },
    async casConcurrency(automationId, key, fromRunId, toRunId) {
      const mapKey = `${automationId}:${key}`;
      if (claims.get(mapKey) !== fromRunId) return false;
      claims.set(mapKey, toRunId);
      return true;
    },
  };

  return { store, starts, runs , claims };
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

    expect(result).toEqual({
      matched: 1,
      started: 1,
      joined: 0,
      queued: 0,
      skipped: 0,
      failed: 0,
      builtins: {},
    });
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

  test("a matching EXTRA entrypoint opens its own run with an entrypoint-scoped id (D9)", async () => {
    // Main listens for PR-opened; the "feedback" entrypoint listens for
    // review events on the SAME connection. A review delivery must open a
    // run through feedback only; a PR delivery through main only; and one
    // delivery matching BOTH entrypoints opens two runs with distinct ids.
    const both: AutomationDefinition = {
      ...definition(trigger()),
      entrypoints: [
        {
          id: "feedback",
          trigger: trigger({ eventKeys: ["pull_request_review.submitted", "pull_request.opened"] }),
          blocks: [
            {
              id: "nudge",
              type: "send_prompt",
              config: {
                session: { template: "s-kept" },
                promptTemplate: "review arrived",
                waitFor: { kind: "none" },
              },
            },
          ],
        },
      ],
    };
    const h = makeHarness([{ automation: meta(), definition: both }]);

    const review = await dispatchIntegrationEvent(
      input({ eventKey: "pull_request_review.submitted", deliveryId: "gh-rev-1" }),
      deps(h),
    );
    expect(review).toMatchObject({ matched: 1, started: 1 });
    const run = h.runs.get("autorun:automation-1:feedback:github:gh-rev-1")!;
    expect(run.entrypointId).toBe("feedback");
    expect(run.deliveryKey).toBe("github:gh-rev-1");

    // One delivery, two matching entrypoints: two runs, two ids, one
    // delivery key — the (automation, entrypoint, delivery) unique holds.
    const openedBoth = await dispatchIntegrationEvent(
      input({ eventKey: "pull_request.opened", deliveryId: "gh-pr-9" }),
      deps(h),
    );
    expect(openedBoth).toMatchObject({ matched: 2, started: 2 });
    expect(h.runs.get("autorun:automation-1:github:gh-pr-9")?.entrypointId).toBe("main");
    expect(h.runs.get("autorun:automation-1:feedback:github:gh-pr-9")?.entrypointId).toBe(
      "feedback",
    );
  });

  test("a supersede-lost row keeps its entrypoint (never the 'main' default)", async () => {
    // The CAS-race loser records a filtered run row; that row's
    // entrypoint_id is part of the delivery dedupe identity and must match
    // the 3-part run id, not fall back to the column default.
    const withEp: AutomationDefinition = {
      ...definition(trigger({ eventKeys: ["x"] })),
      settings: {
        endSessionsOnFinish: false,
        concurrency: { keyTemplate: "k", policy: "supersede" },
      },
      entrypoints: [
        { id: "feedback", trigger: trigger({ eventKeys: ["pull_request.opened"] }), blocks: [] },
      ],
    };
    const h = makeHarness([{ automation: meta(), definition: withEp }]);
    h.claims.set("automation-1:k", "autorun:automation-1:github:earlier");
    const d = deps(h);
    // Lose every CAS: a concurrent superseder always got there first.
    d.store = { ...d.store, casConcurrency: async () => false };

    const result = await dispatchIntegrationEvent(input({ deliveryId: "gh-lost-1" }), d);
    expect(result).toMatchObject({ matched: 1, skipped: 1 });
    const row = h.runs.get("autorun:automation-1:feedback:github:gh-lost-1")!;
    expect(row.status).toBe("filtered");
    expect(row.entrypointId).toBe("feedback");
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

  test("a continue-only event joins an active run or is dropped — it never opens one", async () => {
    // The Slack thread brain's shape: app_mention opens a thread run (join
    // policy, one run per thread); a `message` reply only continues it. A
    // reply in a thread the bot was never mentioned in must not become a
    // fresh run (the legacy brain only engaged app_mention-opened threads).
    const threadKey = "${{ event.raw.event.thread_ts | default: event.raw.event.ts }}";
    const brain = {
      automation: meta({ id: "slack-brain", kind: "builtin", builtinKey: "slack_brain" }),
      definition: {
        ...definition(
          trigger({
            provider: "slack",
            eventKeys: ["app_mention", "message"],
            continueOnly: ["message"],
          }),
        ),
        settings: {
          endSessionsOnFinish: false,
          concurrency: { keyTemplate: threadKey, policy: "join" as const },
        },
      },
    };
    const h = makeHarness([brain]);
    const sent: Array<{ runId: string; deliveryKey: string }> = [];
    const d = {
      ...deps(h),
      sender: {
        async send(runId: string, msg: { kind: string; deliveryKey?: string }) {
          sent.push({ runId, deliveryKey: msg.deliveryKey ?? "" });
        },
      },
    };
    const slack = (eventKey: string, deliveryId: string, event: Record<string, unknown>) =>
      input({ provider: "slack", eventKey, deliveryId, payload: { event } });

    // A reply in a thread nobody opened: dropped — no run row, no claim, no send.
    const stray = await dispatchIntegrationEvent(
      slack("message", "d-stray", { ts: "5.2", thread_ts: "5.0", text: "hi" }),
      d,
    );
    expect(stray).toMatchObject({ matched: 1, skipped: 1, started: 0, joined: 0 });
    expect(h.runs.size).toBe(0);
    expect(h.starts).toEqual([]);
    expect(sent).toEqual([]);
    expect(await h.store.getConcurrencyHolder("slack-brain", "5.0")).toBeNull();

    // The mention opens the thread run …
    const opened = await dispatchIntegrationEvent(
      slack("app_mention", "d-open", { ts: "7.0", text: "<@bot> hello" }),
      d,
    );
    expect(opened).toMatchObject({ started: 1, builtins: { slack_brain: "started" } });
    const runId = h.starts[0]!.runId;

    // … and a reply in THAT thread joins it (delivered into its mailbox).
    const reply = await dispatchIntegrationEvent(
      slack("message", "d-reply", { ts: "7.1", thread_ts: "7.0", text: "and?" }),
      d,
    );
    expect(reply).toMatchObject({ joined: 1, builtins: { slack_brain: "joined" } });
    expect(sent).toEqual([{ runId, deliveryKey: "slack:d-reply" }]);
    expect(h.runs.size).toBe(1);
  });

  test("a kill-switched built-in never admits a run, so a flagged repo is not served by both brains", async () => {
    // With ORCHESTRATOR_REVIEW_AUTOMATION_DISABLED on, the GitHub route falls
    // back to the legacy review graph. If the dispatcher still delivered to
    // the enabled built-in, a flagged repo would get TWO reviews. The switch
    // gates the trigger path too; a user automation on the same event is
    // unaffected.
    const builtin = {
      automation: meta({ id: "builtin-review", kind: "builtin", builtinKey: "pr_review" }),
      definition: definition(trigger()),
    };
    const user = { automation: meta({ id: "user-auto" }), definition: definition(trigger()) };
    const h = makeHarness([builtin, user]);

    const off = await dispatchIntegrationEvent(input(), {
      ...deps(h),
      disabledBuiltins: new Set<string>(),
    });
    expect(off.started).toBe(2);
    // The per-built-in tally is what a legacy route consults (the Slack
    // window): the built-in's own admission outcome, user automations absent.
    expect(off.builtins).toEqual({ pr_review: "started" });

    const h2 = makeHarness([builtin, user]);
    const on = await dispatchIntegrationEvent(input(), {
      ...deps(h2),
      disabledBuiltins: new Set(["pr_review"]),
    });
    expect(on).toMatchObject({ matched: 1, started: 1, builtins: {} });
    expect(h2.starts.map((s) => s.automationId)).toEqual(["user-auto"]);

    // The Slack switch registers its key the same way (4.6).
    const slack = {
      automation: meta({ id: "builtin-slack", kind: "builtin", builtinKey: "slack_brain" }),
      definition: definition(trigger()),
    };
    const h3 = makeHarness([slack, user]);
    const slackOff = await dispatchIntegrationEvent(input(), {
      ...deps(h3),
      disabledBuiltins: new Set(["slack_brain"]),
    });
    expect(slackOff).toMatchObject({ matched: 1, started: 1, builtins: {} });
    expect(h3.starts.map((s) => s.automationId)).toEqual(["user-auto"]);
  });

  test("builtinTookDelivery: started/joined/queued = the engine owns it; absent or skipped = legacy", async () => {
    const { builtinTookDelivery } = await import("../dispatch.ts");
    const base = { matched: 1, started: 0, joined: 0, queued: 0, skipped: 0, failed: 0 };
    expect(builtinTookDelivery(undefined, "slack_brain")).toBe(false);
    expect(builtinTookDelivery({ ...base, builtins: {} }, "slack_brain")).toBe(false);
    expect(builtinTookDelivery({ ...base, builtins: { slack_brain: "skipped" } }, "slack_brain")).toBe(false);
    expect(builtinTookDelivery({ ...base, builtins: { slack_brain: "started" } }, "slack_brain")).toBe(true);
    expect(builtinTookDelivery({ ...base, builtins: { slack_brain: "joined" } }, "slack_brain")).toBe(true);
    expect(builtinTookDelivery({ ...base, builtins: { pr_review: "started" } }, "slack_brain")).toBe(false);
  });

  test("disabledBuiltinsFromConfig maps each switch to its built-in key", async () => {
    const { disabledBuiltinsFromConfig } = await import("../dispatch.ts");
    const { config } = await import("../../config.ts");
    const saved = { r: config.reviewAutomationDisabled, s: config.slackAutomationDisabled };
    try {
      config.reviewAutomationDisabled = false;
      config.slackAutomationDisabled = false;
      expect([...disabledBuiltinsFromConfig()]).toEqual([]);
      config.reviewAutomationDisabled = true;
      expect([...disabledBuiltinsFromConfig()]).toEqual(["pr_review"]);
      config.slackAutomationDisabled = true;
      expect([...disabledBuiltinsFromConfig()].sort()).toEqual(["pr_review", "slack_brain"]);
    } finally {
      config.reviewAutomationDisabled = saved.r;
      config.slackAutomationDisabled = saved.s;
    }
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
