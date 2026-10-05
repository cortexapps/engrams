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

function definition(
  t: IntegrationTriggerSpec,
  overrides: Partial<AutomationDefinition> = {},
): AutomationDefinition {
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
    ...overrides,
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
          instanceId: input.instanceId ?? "",
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
      filtered: 0,
      dropped: 0,
      suppressed: [],
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

  test("the admission prelude decides BEFORE the concurrency claim: a filtered delivery never supersedes the live run", async () => {
    // A review-shaped graph: facts + admit, supersede on the PR url. A
    // comment that is not a review command must not end the running pass.
    const reviewShaped: AutomationDefinition = {
      ...definition(trigger({ eventKeys: ["issue_comment.created"] })),
      blocks: [
        {
          id: "facts",
          type: "code",
          config: {
            mode: "value",
            source: "export default ({ event }) => /review/.test(event.raw?.comment?.body ?? \"\") ? { admit: true } : null;",
          },
        },
        {
          id: "admit",
          type: "filter",
          config: { conditions: { mode: "all", conditions: [{ path: "steps.facts.value.admit", op: "is_true" }] } },
        },
        { id: "launch", type: "create_session", config: { profileId: "p1", promptTemplate: "go" } },
      ],
      settings: {
        endSessionsOnFinish: false,
        concurrency: { keyTemplate: "${{ event.raw.issue.html_url }}", policy: "supersede" },
      },
    };
    const h = makeHarness([{ automation: meta(), definition: reviewShaped }]);
    const live = "autorun:automation-1:github:earlier";
    h.claims.set("automation-1:https://github.com/engrams/engrams/pull/7", live);
    const sent: unknown[] = [];
    const d = { ...deps(h), sender: { async send(_to: string, message: unknown) { sent.push(message); } } };
    const payload = (body: string) => ({
      repository: { full_name: "engrams/engrams" },
      issue: { html_url: "https://github.com/engrams/engrams/pull/7" },
      comment: { body },
    });

    // "@engrams stop" (or "LGTM"): filtered at admission — no claim, no supersede, no start.
    const stop = await dispatchIntegrationEvent(
      input({ eventKey: "issue_comment.created", deliveryId: "gh-stop", payload: payload("@engrams stop") }),
      d,
    );
    expect(stop).toMatchObject({ matched: 1, filtered: 1, started: 0, skipped: 0 });
    expect(sent).toEqual([]);
    expect(h.starts).toEqual([]);
    expect(h.claims.get("automation-1:https://github.com/engrams/engrams/pull/7")).toBe(live);
    const row = h.runs.get("autorun:automation-1:github:gh-stop")!;
    expect(row.status).toBe("filtered");
    expect(row.error).toContain('block "admit"');
    expect(row.concurrencyKey).toBeNull();

    // "@engrams review": admitted — supersedes the live run and starts.
    const review = await dispatchIntegrationEvent(
      input({ eventKey: "issue_comment.created", deliveryId: "gh-review", payload: payload("@engrams review") }),
      d,
    );
    expect(review).toMatchObject({ matched: 1, started: 1, filtered: 0 });
    expect(sent).toEqual([{ kind: "supersede", byRunId: "autorun:automation-1:github:gh-review" }]);
    expect(h.starts.map((s) => s.runId)).toEqual(["autorun:automation-1:github:gh-review"]);
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

// ---------------------------------------------------------------------------
// ADR 0120 instances: admission routing + rung-1 brain precedence
// ---------------------------------------------------------------------------

import type {
  AutomationInstanceRow,
  AutomationInstanceStore,
} from "../../db/automation-instances.ts";

interface InstanceHarness {
  store: AutomationInstanceStore;
  rows: Map<string, AutomationInstanceRow>;
  drops: Array<{ automationId: string; entrypointId: string; eventKey: string; reason: string; detail: string }>;
  seed(input: { automationId: string; key: string; inputs?: Record<string, unknown>; status?: "open" | "closed" }): AutomationInstanceRow;
  bindHandle(automationId: string, handle: string, instanceId: string): void;
}

function fakeInstances(): InstanceHarness {
  const rows = new Map<string, AutomationInstanceRow>();
  const handles = new Map<string, string>();
  const drops: InstanceHarness["drops"] = [];
  let seq = 0;
  const openByKey = (automationId: string, key: string) =>
    [...rows.values()].find(
      (r) => r.automationId === automationId && r.key === key && r.status === "open",
    ) ?? null;
  const store: AutomationInstanceStore = {
    async openInstance(input) {
      const existing = openByKey(input.automationId, input.key);
      if (existing) return existing;
      const row: AutomationInstanceRow = {
        id: `ai_test${++seq}`,
        automationId: input.automationId,
        key: input.key,
        label: input.label ?? null,
        status: "open",
        inputs: input.inputs,
        openedBy: input.openedBy,
        openedAt: RECEIVED_AT,
        closedAt: null,
        closeReason: null,
      };
      rows.set(row.id, row);
      return row;
    },
    async getInstance(id) {
      return rows.get(id) ?? null;
    },
    async getOpenInstanceByKey(automationId, key) {
      return openByKey(automationId, key);
    },
    async listOpenInstances(automationId) {
      return [...rows.values()].filter(
        (r) => r.automationId === automationId && r.status === "open",
      );
    },
    async closeInstance({ instanceId }) {
      const row = rows.get(instanceId);
      if (!row || row.status !== "open") return false;
      row.status = "closed";
      return true;
    },
    async recordInstanceHandle(input) {
      const mapKey = `${input.automationId}:${input.handle}`;
      const holder = handles.get(mapKey);
      if (holder === undefined) {
        handles.set(mapKey, input.instanceId);
        return { kind: "recorded" };
      }
      return holder === input.instanceId
        ? { kind: "already_ours" }
        : { kind: "conflict", instanceId: holder };
    },
    async resolveHandles(automationId, candidates) {
      return candidates.flatMap((handle) => {
        const instanceId = handles.get(`${automationId}:${handle}`);
        if (instanceId === undefined) return [];
        const row = rows.get(instanceId);
        return [{ handle, instanceId, instanceStatus: row?.status ?? "open" }];
      });
    },
    async openHandleOwners(candidates) {
      // Mirrors the PG cross-automation query: every ledger entry for one
      // of these handles whose owning instance is open, with its owner.
      const owners = new Map<string, { instanceId: string; automationId: string }>();
      for (const [key, instanceId] of handles) {
        const automationId = key.slice(0, key.indexOf(":"));
        const handle = key.slice(key.indexOf(":") + 1);
        if (candidates.includes(handle) && rows.get(instanceId)?.status !== "closed") {
          owners.set(instanceId, { instanceId, automationId });
        }
      }
      return [...owners.values()];
    },
    async recordDrop(input) {
      drops.push(input);
    },
    async listRecentDrops() {
      return [];
    },
    async listInstances() {
      return [];
    },
    async listInstanceHandles() {
      return [];
    },
  };
  return {
    store,
    rows,
    drops,
    seed(input) {
      const row: AutomationInstanceRow = {
        id: `ai_test${++seq}`,
        automationId: input.automationId,
        key: input.key,
        label: null,
        status: input.status ?? "open",
        inputs: input.inputs ?? {},
        openedBy: "seed",
        openedAt: RECEIVED_AT,
        closedAt: null,
        closeReason: null,
      };
      rows.set(row.id, row);
      return row;
    },
    bindHandle(automationId, handle, instanceId) {
      handles.set(`${automationId}:${handle}`, instanceId);
    },
  };
}

const GITHUB_PR_FACET = {
  events: [
    {
      key: "pull_request.opened",
      label: "PR opened",
      handleCandidates: [
        {
          parts: [
            { lit: "github:" },
            { path: "repository.full_name" },
            { lit: "#" },
            { path: "pull_request.number" },
          ],
        },
      ],
    },
  ],
};

function instancedDefinition(
  overrides: Partial<AutomationDefinition["settings"]["instance"] & object> = {},
): AutomationDefinition {
  return definition(trigger(), {
    settings: {
      endSessionsOnFinish: false,
      instance: { keyTemplate: "pr-${{ event.raw.pull_request.number }}", ...overrides },
    },
  });
}

describe("dispatchIntegrationEvent + instances (ADR 0120)", () => {
  const prPayload = {
    repository: { full_name: "engrams/engrams" },
    pull_request: { number: 41 },
  };
  const instanceDeps = (h: Harness, i: InstanceHarness) => ({
    ...deps(h),
    instances: i.store,
    facets: async () => GITHUB_PR_FACET,
  });

  test("admit open: renders the key, opens the instance with rendered inputs, stamps + prefixes", async () => {
    const target = {
      automation: meta(),
      definition: {
        ...instancedDefinition({ inputs: { pr: "${{ event.raw.pull_request.number }}" } }),
        inputsSchema: [{ key: "pr", label: "PR", type: "string" as const }],
      },
    };
    // Give it a concurrency template too, so the instance prefix is visible.
    target.definition.settings.concurrency = { keyTemplate: "fixed", policy: "queue" };
    const h = makeHarness([target]);
    const i = fakeInstances();

    const result = await dispatchIntegrationEvent(input({ payload: prPayload }), instanceDeps(h, i));
    expect(result).toMatchObject({ matched: 1, started: 1, dropped: 0, suppressed: [] });

    const instance = [...i.rows.values()][0]!;
    expect(instance).toMatchObject({ key: "pr-41", inputs: { pr: "41" }, status: "open" });
    const run = [...h.runs.values()][0]!;
    expect(run.instanceId).toBe(instance.id);
    expect(run.id).toBe(`autorun:automation-1:main:i-${instance.id}:github:gh-delivery-1`);
    expect([...h.claims.keys()]).toEqual([`automation-1:i:${instance.id}:fixed`]);

    // The same key on a later delivery JOINS the instance instead of opening
    // a second one.
    await dispatchIntegrationEvent(
      input({ payload: prPayload, deliveryId: "gh-delivery-2" }),
      instanceDeps(h, i),
    );
    expect(i.rows.size).toBe(1);
  });

  test("the admission prelude runs BEFORE the workstream opens: a filtered delivery creates no workstream", async () => {
    // Review-shaped: facts + admit, then a session; a workstream per PR.
    const target = {
      automation: meta(),
      definition: {
        ...instancedDefinition(),
        trigger: trigger({ eventKeys: ["issue_comment.created"] }),
        blocks: [
          {
            id: "facts",
            type: "code",
            config: {
              mode: "value",
              source: "export default ({ event }) => /review/.test(event.raw?.comment?.body ?? \"\") ? { admit: true } : null;",
            },
          },
          {
            id: "admit",
            type: "filter",
            config: { conditions: { mode: "all", conditions: [{ path: "steps.facts.value.admit", op: "is_true" }] } },
          },
          { id: "launch", type: "create_session", config: { profileId: "p1", promptTemplate: "go" } },
        ],
      } as AutomationDefinition,
    };
    const h = makeHarness([target]);
    const i = fakeInstances();
    const payload = (body: string) => ({ ...prPayload, comment: { body } });

    const lgtm = await dispatchIntegrationEvent(
      input({ eventKey: "issue_comment.created", payload: payload("LGTM"), deliveryId: "gh-lgtm" }),
      instanceDeps(h, i),
    );
    expect(lgtm).toMatchObject({ matched: 1, filtered: 1, started: 0, dropped: 0 });
    expect(i.rows.size).toBe(0); // nothing opened for a comment nobody asked to review
    const row = h.runs.get("autorun:automation-1:github:gh-lgtm")!;
    expect(row).toMatchObject({ status: "filtered", instanceId: "" });

    const review = await dispatchIntegrationEvent(
      input({ eventKey: "issue_comment.created", payload: payload("@engrams review"), deliveryId: "gh-review" }),
      instanceDeps(h, i),
    );
    expect(review).toMatchObject({ matched: 1, started: 1, filtered: 0 });
    expect(i.rows.size).toBe(1);
    const instance = [...i.rows.values()][0]!;
    expect(h.runs.get(`autorun:automation-1:main:i-${instance.id}:github:gh-review`)?.instanceId).toBe(instance.id);
  });

  test("a conversation workstream: the mention opens it, a reply joins its run, a stray reply drops, a closed thread drops", async () => {
    // The Slack threads shape: main = app_mention (opens), reply entrypoint =
    // message with continueOnly + admit require (joins the thread's live run
    // or drops), one instance-scoped join key for both.
    const key = "${{ event.raw.team_id }}:${{ event.raw.event.channel }}:${{ event.raw.event | coalesce: \"thread_ts\", \"ts\" }}";
    const slack: AutomationDefinition = {
      ...definition(trigger({ provider: "slack", eventKeys: ["app_mention"] })),
      entrypoints: [
        {
          id: "reply",
          trigger: trigger({ provider: "slack", eventKeys: ["message"], continueOnly: ["message"] }),
          blocks: [],
        },
      ],
      settings: {
        endSessionsOnFinish: false,
        instance: { keyTemplate: key, entrypoints: { reply: { admit: "require" } } },
        concurrency: { keyTemplate: key, policy: "join" },
      },
    };
    const h = makeHarness([{ automation: meta(), definition: slack }]);
    const i = fakeInstances();
    const sent: Array<{ to: string; eventKey?: string }> = [];
    const d = {
      ...deps(h),
      instances: i.store,
      facets: async () => undefined,
      sender: { async send(to: string, message: { kind: string; eventKey?: string }) { sent.push({ to, ...(message.eventKey ? { eventKey: message.eventKey } : {}) }); } },
    };
    const slackInput = (eventKey: string, ev: Record<string, unknown>, deliveryId: string) =>
      input({ provider: "slack", eventKey, deliveryId, payload: { team_id: "T1", event: ev } });

    // 1. A mention opens the thread's workstream and starts its run.
    const opened = await dispatchIntegrationEvent(
      slackInput("app_mention", { channel: "C1", ts: "100.1", text: "<@BOT> hi" }, "sl-1"),
      d,
    );
    expect(opened).toMatchObject({ matched: 1, started: 1 });
    expect(i.rows.size).toBe(1);
    const thread = [...i.rows.values()][0]!;
    expect(thread.key).toBe("T1:C1:100.1");
    const mainRun = `autorun:automation-1:main:i-${thread.id}:slack:sl-1`;
    expect(h.starts.map((s) => s.runId)).toEqual([mainRun]);

    // 2. A reply in that thread joins the run's mailbox: no new run, no new
    //    workstream, the event delivered to the holder.
    const joined = await dispatchIntegrationEvent(
      slackInput("message", { channel: "C1", ts: "100.2", thread_ts: "100.1", text: "more" }, "sl-2"),
      d,
    );
    expect(joined).toMatchObject({ matched: 1, joined: 1, started: 0, dropped: 0 });
    expect(sent).toEqual([{ to: mainRun, eventKey: "message" }]);
    expect(i.rows.size).toBe(1);
    expect(h.runs.size).toBe(1);

    // 3. A reply in a thread nobody was mentioned in drops (audited): no run,
    //    no workstream.
    const stray = await dispatchIntegrationEvent(
      slackInput("message", { channel: "C1", ts: "200.2", thread_ts: "200.1", text: "psst" }, "sl-3"),
      d,
    );
    expect(stray).toMatchObject({ matched: 1, dropped: 1, started: 0, joined: 0 });
    expect(i.drops.at(-1)).toMatchObject({ entrypointId: "reply", reason: "no_open_instance", detail: "T1:C1:200.1" });
    expect(i.rows.size).toBe(1);

    // 4. Once the thread's workstream is closed, a reply there drops too, and
    //    a NEW mention in the same thread opens a fresh workstream.
    await i.store.closeInstance({ instanceId: thread.id });
    const late = await dispatchIntegrationEvent(
      slackInput("message", { channel: "C1", ts: "100.9", thread_ts: "100.1", text: "still there?" }, "sl-4"),
      d,
    );
    expect(late).toMatchObject({ dropped: 1 });
    const again = await dispatchIntegrationEvent(
      slackInput("app_mention", { channel: "C1", ts: "100.10", thread_ts: "100.1", text: "<@BOT> again" }, "sl-5"),
      d,
    );
    expect(again).toMatchObject({ started: 1 });
    expect([...i.rows.values()].filter((r) => r.status === "open")).toHaveLength(1);
  });

  test("admit require: no open workstream drops the event with an audited reason and NO run row", async () => {
    const target = {
      automation: meta(),
      definition: instancedDefinition({ entrypoints: { main: { admit: "require" as const } } }),
    };
    const h = makeHarness([target]);
    const i = fakeInstances();

    const result = await dispatchIntegrationEvent(input({ payload: prPayload }), instanceDeps(h, i));
    expect(result).toMatchObject({ matched: 1, started: 0, dropped: 1 });
    expect(h.runs.size).toBe(0);
    expect(i.drops).toEqual([
      {
        automationId: "automation-1",
        entrypointId: "main",
        eventKey: "pull_request.opened",
        reason: "no_open_instance",
        detail: "pr-41",
      },
    ]);

    // With an open instance for the rendered key, the same event admits.
    i.seed({ automationId: "automation-1", key: "pr-41" });
    const admitted = await dispatchIntegrationEvent(
      input({ payload: prPayload, deliveryId: "gh-delivery-2" }),
      instanceDeps(h, i),
    );
    expect(admitted).toMatchObject({ started: 1, dropped: 0 });
  });

  test("handle_match: the ledger routes over the key template; unbound events drop", async () => {
    const target = {
      automation: meta(),
      definition: instancedDefinition({ entrypoints: { main: { admit: "handle_match" as const } } }),
    };
    const h = makeHarness([target]);
    const i = fakeInstances();
    // The instance's key is UNRELATED to what the key template would render:
    // only the handle can route this event.
    const owner = i.seed({ automationId: "automation-1", key: "project-ENG-7" });
    i.bindHandle("automation-1", "github:engrams/engrams#41", owner.id);

    const bound = await dispatchIntegrationEvent(input({ payload: prPayload }), instanceDeps(h, i));
    expect(bound).toMatchObject({ started: 1, dropped: 0 });
    expect([...h.runs.values()][0]!.instanceId).toBe(owner.id);

    const unbound = await dispatchIntegrationEvent(
      input({
        payload: { repository: { full_name: "engrams/engrams" }, pull_request: { number: 99 } },
        deliveryId: "gh-delivery-2",
      }),
      instanceDeps(h, i),
    );
    expect(unbound).toMatchObject({ started: 0, dropped: 1 });
    expect(i.drops.at(-1)).toMatchObject({ reason: "no_handle_match" });
  });

  test("a closed workstream's handle drops the event (v1 policy, audited)", async () => {
    const target = { automation: meta(), definition: instancedDefinition() };
    const h = makeHarness([target]);
    const i = fakeInstances();
    const owner = i.seed({ automationId: "automation-1", key: "pr-41", status: "closed" });
    i.bindHandle("automation-1", "github:engrams/engrams#41", owner.id);

    const result = await dispatchIntegrationEvent(input({ payload: prPayload }), instanceDeps(h, i));
    expect(result).toMatchObject({ started: 0, dropped: 1 });
    expect(h.runs.size).toBe(0);
    expect(i.drops).toEqual([
      {
        automationId: "automation-1",
        entrypointId: "main",
        eventKey: "pull_request.opened",
        reason: "closed_instance",
        detail: "github:engrams/engrams#41",
      },
    ]);
  });

  test("rung-1 precedence: a handle-bound workstream stands the slack brain down; unbound events do not", async () => {
    const slackTrigger = trigger({ provider: "slack", eventKeys: ["message"] });
    const brain = {
      automation: meta({ id: "brain-1", builtinKey: "slack_brain", kind: "builtin" }),
      definition: definition(slackTrigger),
    };
    const custom = {
      automation: meta({ id: "custom-1" }),
      definition: definition(slackTrigger, {
        settings: {
          endSessionsOnFinish: false,
          instance: {
            keyTemplate: "thread-${{ event.raw.event.thread_ts }}",
            entrypoints: { main: { admit: "handle_match" as const } },
          },
        },
      }),
    };
    const slackFacet = {
      events: [
        {
          key: "message",
          label: "Message",
          handleCandidates: [
            {
              parts: [
                { lit: "slack:" },
                { path: "event.channel" },
                { lit: ":" },
                { path: "event.thread_ts" },
              ],
            },
          ],
        },
      ],
    };
    const h = makeHarness([brain, custom]);
    const i = fakeInstances();
    const owner = i.seed({ automationId: "custom-1", key: "project-ENG-7" });
    i.bindHandle("custom-1", "slack:C1:1724.100", owner.id);

    const boundEvent = input({
      provider: "slack",
      eventKey: "message",
      payload: { event: { channel: "C1", thread_ts: "1724.100" } },
    });
    const bound = await dispatchIntegrationEvent(boundEvent, {
      ...deps(h),
      instances: i.store,
      facets: async () => slackFacet,
    });
    expect(bound).toMatchObject({ started: 1, suppressed: ["slack_brain"] });
    expect(bound.builtins).toEqual({});
    expect(h.starts.map((s) => s.automationId)).toEqual(["custom-1"]);

    // An UNBOUND thread: the custom automation drops (handle_match) and the
    // brain answers as before.
    const h2 = makeHarness([brain, custom]);
    const unbound = await dispatchIntegrationEvent(
      input({
        provider: "slack",
        eventKey: "message",
        deliveryId: "slack-2",
        payload: { event: { channel: "C1", thread_ts: "9999.000" } },
      }),
      { ...deps(h2), instances: i.store, facets: async () => slackFacet },
    );
    expect(unbound).toMatchObject({ started: 1, dropped: 1, suppressed: [] });
    expect(unbound.builtins).toEqual({ slack_brain: "started" });
    expect(h2.starts.map((s) => s.automationId)).toEqual(["brain-1"]);
  });

  test("a workstream's labelTemplate renders its title at open; an empty render leaves the key", async () => {
    const slackTrigger = trigger({ provider: "slack", eventKeys: ["app_mention"] });
    const brain = {
      automation: meta({ id: "brain-1", builtinKey: "slack_brain", kind: "builtin" }),
      definition: definition(slackTrigger, {
        settings: {
          endSessionsOnFinish: false,
          instance: {
            keyTemplate: "thread-${{ event.raw.event.ts }}",
            labelTemplate: "${{ event.raw.event.text | strip_mentions | truncate: 80 }}",
          },
        },
      }),
    };
    const i = fakeInstances();
    await dispatchIntegrationEvent(
      input({
        provider: "slack",
        eventKey: "app_mention",
        payload: { event: { channel: "C1", ts: "1.1", text: "<@UBOT> can you draw a pelican?" } },
      }),
      { ...deps(makeHarness([brain])), instances: i.store, facets: async () => undefined },
    );
    expect((await i.store.getOpenInstanceByKey("brain-1", "thread-1.1"))?.label).toBe("can you draw a pelican?");

    await dispatchIntegrationEvent(
      input({
        provider: "slack",
        eventKey: "app_mention",
        deliveryId: "slack-bare",
        payload: { event: { channel: "C1", ts: "2.2", text: "<@UBOT>" } },
      }),
      { ...deps(makeHarness([brain])), instances: i.store, facets: async () => undefined },
    );
    expect((await i.store.getOpenInstanceByKey("brain-1", "thread-2.2"))?.label).toBeNull();
  });

  test("rung-1 precedence: the brain's OWN workstream owning the thread never stands the brain down (its reply joins by key)", async () => {
    // Prod 2026-10-01: the brain's "Started a session" post binds the thread
    // handle to the brain's workstream, and the pre-pass read that binding
    // as "a workstream owns this conversation" — every follow-up in every
    // brain thread was suppressed by the brain's own binding. Its own
    // ownership is no verdict at all: the built-in routes the event to its
    // own workstream.
    const slackTrigger = trigger({ provider: "slack", eventKeys: ["message"] });
    const brain = {
      automation: meta({ id: "brain-1", builtinKey: "slack_brain", kind: "builtin" }),
      definition: definition(slackTrigger, {
        settings: {
          endSessionsOnFinish: false,
          instance: {
            keyTemplate: "thread-${{ event.raw.event.thread_ts }}",
            entrypoints: { main: { admit: "require" as const } },
          },
        },
      }),
    };
    const slackFacet = {
      events: [
        {
          key: "message",
          label: "Message",
          handleCandidates: [
            {
              parts: [
                { lit: "slack:" },
                { path: "event.channel" },
                { lit: ":" },
                { path: "event.thread_ts" },
              ],
            },
          ],
        },
      ],
    };
    const h = makeHarness([brain]);
    const i = fakeInstances();
    const own = i.seed({ automationId: "brain-1", key: "thread-1724.100" });
    i.bindHandle("brain-1", "slack:C1:1724.100", own.id);

    const reply = await dispatchIntegrationEvent(
      input({
        provider: "slack",
        eventKey: "message",
        payload: { event: { channel: "C1", thread_ts: "1724.100" } },
      }),
      { ...deps(h), instances: i.store, facets: async () => slackFacet },
    );
    // No stand-down: the owner IS the brain …
    expect(reply.suppressed).toEqual([]);
    // … and the built-in takes the delivery into its own workstream.
    expect(reply.builtins["slack_brain"]).toBeDefined();
    expect(reply.builtins["slack_brain"]).not.toBe("skipped");
    expect(h.starts.map((s) => s.automationId)).toEqual(["brain-1"]);
  });

  test("rung 2 channel binding: top-level messages route to the channel's workstream; threads beat the channel; a closed thread never falls through", async () => {
    const slackTrigger = trigger({ provider: "slack", eventKeys: ["message"] });
    const brain = {
      automation: meta({ id: "brain-1", builtinKey: "slack_brain", kind: "builtin" }),
      definition: definition(slackTrigger),
    };
    const custom = {
      automation: meta({ id: "custom-1" }),
      definition: definition(slackTrigger, {
        settings: {
          endSessionsOnFinish: false,
          instance: {
            keyTemplate: "chan-${{ event.raw.event.channel }}",
            entrypoints: { main: { admit: "handle_match" as const } },
          },
        },
      }),
    };
    // The real manifest shape: thread template FIRST, channel second —
    // declaration order is the whole precedence story.
    const slackFacet = {
      events: [
        {
          key: "message",
          label: "Message",
          handleCandidates: [
            {
              parts: [
                { lit: "slack:" },
                { path: "event.channel" },
                { lit: ":" },
                { path: "event.thread_ts" },
              ],
            },
            { parts: [{ lit: "slack:" }, { path: "event.channel" }] },
          ],
        },
      ],
    };
    const i = fakeInstances();
    const channelOwner = i.seed({ automationId: "custom-1", key: "chan-C1" });
    i.bindHandle("custom-1", "slack:C1", channelOwner.id);

    // A TOP-LEVEL message (no thread_ts) routes by the channel handle and
    // stands the brain down channel-wide — previously it produced zero
    // candidates and fell to the brain.
    const h = makeHarness([brain, custom]);
    const topLevel = await dispatchIntegrationEvent(
      input({
        provider: "slack",
        eventKey: "message",
        payload: { event: { channel: "C1" } },
      }),
      { ...deps(h), instances: i.store, facets: async () => slackFacet },
    );
    expect(topLevel).toMatchObject({ started: 1, suppressed: ["slack_brain"] });
    expect(h.starts.map((s) => s.automationId)).toEqual(["custom-1"]);
    const started = [...h.runs.values()][0]!;
    expect(started.instanceId).toBe(channelOwner.id);

    // A thread owned by ANOTHER workstream wins over the channel owner:
    // the thread template is declared first.
    const threadOwner = i.seed({ automationId: "custom-1", key: "thread-1724.100" });
    i.bindHandle("custom-1", "slack:C1:1724.100", threadOwner.id);
    const h2 = makeHarness([brain, custom]);
    await dispatchIntegrationEvent(
      input({
        provider: "slack",
        eventKey: "message",
        deliveryId: "slack-thread",
        payload: { event: { channel: "C1", thread_ts: "1724.100" } },
      }),
      { ...deps(h2), instances: i.store, facets: async () => slackFacet },
    );
    expect([...h2.runs.values()][0]!.instanceId).toBe(threadOwner.id);

    // A CLOSED thread hit is skipped, not terminal: the reply falls through
    // to the open channel owner — it is just channel traffic now — and the
    // brain stays suppressed in owned territory. Only when EVERY matching
    // candidate is closed does the event drop.
    await i.store.closeInstance({ instanceId: threadOwner.id });
    const h3 = makeHarness([brain, custom]);
    const closedThread = await dispatchIntegrationEvent(
      input({
        provider: "slack",
        eventKey: "message",
        deliveryId: "slack-closed-thread",
        payload: { event: { channel: "C1", thread_ts: "1724.100" } },
      }),
      { ...deps(h3), instances: i.store, facets: async () => slackFacet },
    );
    expect(closedThread).toMatchObject({ started: 1, dropped: 0, suppressed: ["slack_brain"] });
    expect([...h3.runs.values()][0]!.instanceId).toBe(channelOwner.id);

    // Every matching candidate closed (channel owner closes too): drop,
    // audited against the most specific candidate.
    await i.store.closeInstance({ instanceId: channelOwner.id });
    const h4 = makeHarness([brain, custom]);
    const allClosed = await dispatchIntegrationEvent(
      input({
        provider: "slack",
        eventKey: "message",
        deliveryId: "slack-all-closed",
        payload: { event: { channel: "C1", thread_ts: "1724.100" } },
      }),
      { ...deps(h4), instances: i.store, facets: async () => slackFacet },
    );
    expect(allClosed).toMatchObject({ started: 1, dropped: 1, suppressed: [] });
    expect(allClosed.builtins).toEqual({ slack_brain: "started" });
    expect(i.drops.at(-1)).toMatchObject({
      reason: "closed_instance",
      detail: "slack:C1:1724.100",
    });
  });

  test("conversation ownership suppresses the brain for event keys the owner does not subscribe to", async () => {
    // The prod 2026-08-26 shape: a TAGGED message arrives as TWO deliveries
    // (message + app_mention). The channel-owning workstream subscribes only
    // to `message`, so the app_mention delivery matches NO instanced target —
    // suppression must come from the ledger, not from a matched resolution.
    const brain = {
      automation: meta({ id: "brain-1", builtinKey: "slack_brain", kind: "builtin" }),
      definition: definition(trigger({ provider: "slack", eventKeys: ["app_mention"] })),
    };
    const custom = {
      automation: meta({ id: "custom-1" }),
      definition: definition(trigger({ provider: "slack", eventKeys: ["message"] }), {
        settings: {
          endSessionsOnFinish: false,
          instance: {
            keyTemplate: "chan-${{ event.raw.event.channel }}",
            entrypoints: { main: { admit: "handle_match" as const } },
          },
        },
      }),
    };
    const slackFacet = {
      events: [
        {
          key: "app_mention",
          label: "App mention",
          handleCandidates: [
            {
              parts: [
                { lit: "slack:" },
                { path: "event.channel" },
                { lit: ":" },
                { path: "event.thread_ts" },
              ],
            },
            { parts: [{ lit: "slack:" }, { path: "event.channel" }] },
          ],
        },
      ],
    };
    const i = fakeInstances();
    const owner = i.seed({ automationId: "custom-1", key: "chan-C1" });
    i.bindHandle("custom-1", "slack:C1", owner.id);

    const h = makeHarness([brain, custom]);
    const mention = await dispatchIntegrationEvent(
      input({
        provider: "slack",
        eventKey: "app_mention",
        payload: { event: { channel: "C1" } },
      }),
      { ...deps(h), instances: i.store, facets: async () => slackFacet },
    );
    // The custom automation does not match app_mention (started 0 for it);
    // the brain is suppressed by conversation ownership, so NOTHING answers
    // this delivery — the message-event twin already reached the owner.
    expect(mention).toMatchObject({ started: 0, suppressed: ["slack_brain"] });
    expect(mention.builtins).toEqual({});

    // Control: once the owner closes, the mention reaches the brain again.
    await i.store.closeInstance({ instanceId: owner.id });
    const h2 = makeHarness([brain, custom]);
    const afterClose = await dispatchIntegrationEvent(
      input({
        provider: "slack",
        eventKey: "app_mention",
        deliveryId: "slack-after-close",
        payload: { event: { channel: "C1" } },
      }),
      { ...deps(h2), instances: i.store, facets: async () => slackFacet },
    );
    expect(afterClose).toMatchObject({ started: 1, suppressed: [] });
    expect(afterClose.builtins).toEqual({ slack_brain: "started" });
  });

  test("the verdict is recorded even when NO brain is a matched target", async () => {
    // The prod 2026-08-26 second finding: with the brain not among the
    // matched targets, the ownership check was gated out and `suppressed`
    // stayed empty, so a consumer of the verdict spawned a session inside an
    // owned channel. The verdict is a property of the DELIVERY: it is
    // recorded whenever another automation's ownership holds, brains or no
    // brains.
    const custom = {
      automation: meta({ id: "custom-1" }),
      definition: definition(trigger({ provider: "slack", eventKeys: ["message"] }), {
        settings: {
          endSessionsOnFinish: false,
          instance: {
            keyTemplate: "chan-${{ event.raw.event.channel }}",
            entrypoints: { main: { admit: "handle_match" as const } },
          },
        },
      }),
    };
    const slackFacet = {
      events: [
        {
          key: "app_mention",
          label: "App mention",
          handleCandidates: [{ parts: [{ lit: "slack:" }, { path: "event.channel" }] }],
        },
      ],
    };
    const i = fakeInstances();
    const owner = i.seed({ automationId: "custom-1", key: "chan-C1" });
    i.bindHandle("custom-1", "slack:C1", owner.id);

    const h = makeHarness([custom]);
    const mention = await dispatchIntegrationEvent(
      input({
        provider: "slack",
        eventKey: "app_mention",
        payload: { event: { channel: "C1" } },
      }),
      { ...deps(h), instances: i.store, facets: async () => slackFacet },
    );
    // Zero matched targets — the owner subscribes to `message` and no brain
    // built-in exists — yet the delivery carries the stand-down verdict.
    expect(mention).toMatchObject({ matched: 0, started: 0, suppressed: ["slack_brain"] });
  });

});
