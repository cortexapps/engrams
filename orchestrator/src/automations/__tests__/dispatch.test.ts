import { describe, expect, test } from "bun:test";

import type {
  AutomationMetaRow,
  AutomationRunRow,
  DispatchTarget,
} from "../../db/automations.ts";
import type { AutomationDefinition } from "../engine/definition.ts";
import type { AutomationInbox } from "../engine/inbox.ts";
import {
  automationRunId,
  dispatchWebhookOccurrence,
  WEBHOOK_SAMPLE_RETENTION,
  type AutomationWebhookStore,
} from "../dispatch.ts";

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
    createdAt: RECEIVED_AT,
    updatedAt: RECEIVED_AT,
    archivedAt: null,
    ...overrides,
  };
}

function definition(overrides: Partial<AutomationDefinition> = {}): AutomationDefinition {
  return {
    engine: 1,
    trigger: { kind: "webhook", registrationId: "hooks-1", events: ["issues.opened"] },
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

interface Harness {
  store: AutomationWebhookStore;
  starts: Array<{ runId: string; automationId: string; workflowId: string }>;
  sends: Array<{ runId: string; message: AutomationInbox; key: string }>;
  runs: Map<string, AutomationRunRow>;
  samples: Array<{ registrationId: string; retain: number }>;
  claims: Map<string, string>;
}

function makeHarness(targets: DispatchTarget[]): Harness {
  const runs = new Map<string, AutomationRunRow>();
  const claims = new Map<string, string>();
  const starts: Harness["starts"] = [];
  const sends: Harness["sends"] = [];
  const samples: Harness["samples"] = [];

  const store: AutomationWebhookStore = {
    async recordWebhookSample(input) {
      samples.push({ registrationId: input.registrationId, retain: input.retain });
    },
    async listEnabledForWebhookRegistration() {
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
    async casConcurrency(automationId, key, fromRunId, toRunId) {
      const mapKey = `${automationId}:${key}`;
      if (claims.get(mapKey) !== fromRunId) return false;
      claims.set(mapKey, toRunId);
      return true;
    },
  };

  return {
    store,
    starts,
    sends,
    runs,
    samples,
    claims,
  };
}

function deps(h: Harness) {
  return {
    store: h.store,
    workflowStarter: {
      async start(input: { runId: string; automationId: string }, workflowId: string) {
        h.starts.push({ ...input, workflowId });
      },
    },
    sender: {
      async send(runId: string, message: AutomationInbox, key: string) {
        h.sends.push({ runId, message, key });
      },
    },
    now: () => RECEIVED_AT,
  };
}

function delivery(overrides: Partial<Parameters<typeof dispatchWebhookOccurrence>[0]> = {}) {
  return {
    registrationId: "hooks-1",
    registration: {
      id: "hooks-1",
      name: "Hooks",
      verification: { scheme: "generic_hmac_sha256" as const, secretRef: "webhook.hooks-1.secret" },
      providerHint: null,
      disabledReason: null,
      createdByUserId: "admin-1",
      createdAt: RECEIVED_AT,
      updatedAt: RECEIVED_AT,
    },
    eventKey: "issues.opened",
    deliveryId: "delivery-1",
    payload: { issue: { title: "boom" } },
    receivedAt: RECEIVED_AT,
    ...overrides,
  };
}

describe("dispatchWebhookOccurrence", () => {
  test("a match creates the deterministic run id and starts its workflow", async () => {
    const h = makeHarness([{ automation: meta(), definition: definition() }]);
    const result = await dispatchWebhookOccurrence(delivery(), deps(h));

    expect(result).toMatchObject({ matched: 1, started: 1, joined: 0, queued: 0, skipped: 0 });
    const runId = automationRunId("automation-1", "webhook:delivery-1");
    expect(runId).toBe("autorun:automation-1:webhook:delivery-1");
    expect(h.starts).toEqual([
      { runId, automationId: "automation-1", workflowId: runId },
    ]);
    const run = h.runs.get(runId)!;
    expect(run.version).toBe(2);
    expect(run.trigger).toMatchObject({
      source: "webhook",
      eventKey: "issues.opened",
      deliveryId: "delivery-1",
      receivedAt: RECEIVED_AT.toISOString(),
    });
    expect(h.samples).toEqual([{ registrationId: "hooks-1", retain: WEBHOOK_SAMPLE_RETENTION }]);
  });

  test("a mismatched event key stores the sample but never starts a run", async () => {
    const h = makeHarness([{ automation: meta(), definition: definition() }]);
    const result = await dispatchWebhookOccurrence(
      delivery({ eventKey: "issues.closed" }),
      deps(h),
    );
    expect(result.matched).toBe(0);
    expect(h.samples).toEqual([{ registrationId: "hooks-1", retain: WEBHOOK_SAMPLE_RETENTION }]);
    expect(h.starts).toEqual([]);
  });

  test("trigger filters gate dispatch", async () => {
    const h = makeHarness([
      {
        automation: meta(),
        definition: definition({
          trigger: {
            kind: "webhook",
            registrationId: "hooks-1",
            events: ["issues.opened"],
            filter: { "issue.title": "other" },
          },
        }),
      },
    ]);
    const result = await dispatchWebhookOccurrence(delivery(), deps(h));
    expect(result.matched).toBe(0);
    expect(h.starts).toEqual([]);
  });

  const concurrent = (policy: "queue" | "supersede" | "skip" | "join") =>
    definition({
      settings: {
        endSessionsOnFinish: false,
        concurrency: { keyTemplate: "issue-${{ event.raw.issue.number }}", policy },
      },
    });

  test("join routes the delivery into the holder's mailbox, no run row", async () => {
    const h = makeHarness([{ automation: meta(), definition: concurrent("join") }]);
    h.claims.set("automation-1:issue-7", "autorun:automation-1:webhook:earlier");

    const result = await dispatchWebhookOccurrence(
      delivery({ payload: { issue: { number: 7 } } }),
      deps(h),
    );

    expect(result).toMatchObject({ matched: 1, joined: 1, started: 0 });
    expect(h.runs.size).toBe(0);
    expect(h.sends).toHaveLength(1);
    const send = h.sends[0]!;
    expect(send.runId).toBe("autorun:automation-1:webhook:earlier");
    expect(send.message).toMatchObject({ kind: "event", eventKey: "issues.opened" });
    expect(send.key).toBe(
      "autorun-evt:webhook:delivery-1:autorun:automation-1:webhook:earlier",
    );
  });

  test("queue leaves a pending run and starts nothing", async () => {
    const h = makeHarness([{ automation: meta(), definition: concurrent("queue") }]);
    h.claims.set("automation-1:issue-7", "autorun:automation-1:webhook:earlier");

    const result = await dispatchWebhookOccurrence(
      delivery({ payload: { issue: { number: 7 } } }),
      deps(h),
    );

    expect(result).toMatchObject({ queued: 1, started: 0 });
    const run = [...h.runs.values()][0]!;
    expect(run.status).toBe("pending");
    expect(run.concurrencyKey).toBe("issue-7");
    expect(h.starts).toEqual([]);
  });

  test("skip records an auditable filtered run", async () => {
    const h = makeHarness([{ automation: meta(), definition: concurrent("skip") }]);
    h.claims.set("automation-1:issue-7", "autorun:automation-1:webhook:earlier");

    const result = await dispatchWebhookOccurrence(
      delivery({ payload: { issue: { number: 7 } } }),
      deps(h),
    );

    expect(result).toMatchObject({ skipped: 1, started: 0 });
    const run = [...h.runs.values()][0]!;
    expect(run.status).toBe("filtered");
    expect(run.error).toContain("autorun:automation-1:webhook:earlier");
  });

  test("supersede CASes the claim, signals the holder, then starts", async () => {
    const h = makeHarness([{ automation: meta(), definition: concurrent("supersede") }]);
    const holder = "autorun:automation-1:webhook:earlier";
    h.claims.set("automation-1:issue-7", holder);

    const result = await dispatchWebhookOccurrence(
      delivery({ payload: { issue: { number: 7 } } }),
      deps(h),
    );

    expect(result).toMatchObject({ started: 1 });
    const newRunId = automationRunId("automation-1", "webhook:delivery-1");
    expect(h.claims.get("automation-1:issue-7")).toBe(newRunId);
    expect(h.sends[0]).toMatchObject({
      runId: holder,
      message: { kind: "supersede", byRunId: newRunId },
    });
    expect(h.starts.map((s) => s.runId)).toEqual([newRunId]);
  });

  test("an uncontended concurrency claim starts with the key recorded", async () => {
    const h = makeHarness([{ automation: meta(), definition: concurrent("supersede") }]);
    const result = await dispatchWebhookOccurrence(
      delivery({ payload: { issue: { number: 7 } } }),
      deps(h),
    );
    expect(result).toMatchObject({ started: 1 });
    const run = [...h.runs.values()][0]!;
    expect(run.concurrencyKey).toBe("issue-7");
  });
});
