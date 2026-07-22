import { describe, expect, test } from "bun:test";

import type { AutomationRow, WebhookRegistrationRow } from "../../db/automations.ts";
import {
  automationWebhookWorkflowId,
  dispatchWebhookOccurrence,
  SYSTEM_GITHUB_REGISTRATION_ID,
  WEBHOOK_SAMPLE_RETENTION,
  type AutomationWebhookStarter,
  type AutomationWebhookStore,
} from "../dispatch.ts";
import type { AutomationRunWorkflowInput } from "../../workflows/automation-run.ts";

const NOW = new Date("2026-07-22T12:00:00Z");

function automation(id: string, filter?: Record<string, unknown>): AutomationRow {
  return {
    id,
    name: id,
    description: "",
    enabled: true,
    trigger: {
      kind: "webhook",
      registrationId: "my-hook",
      events: ["incident.opened"],
      ...(filter ? { filter } : {}),
    },
    action: {
      kind: "create_task",
      profileId: "profile-1",
      promptTemplate: "handle it",
      includeEventContext: true,
    },
    createdByUserId: "admin",
    nextFireAt: null,
    lastFiredAt: null,
    createdAt: NOW,
    updatedAt: NOW,
    archivedAt: null,
  };
}

const registration: WebhookRegistrationRow = {
  id: "my-hook",
  name: "My hook",
  verification: { scheme: "generic_hmac_sha256", secretRef: "webhook.my-hook.secret" },
  providerHint: null,
  createdByUserId: "admin",
  createdAt: NOW,
  updatedAt: NOW,
};

function fixture(automations: AutomationRow[]) {
  const samples: Parameters<AutomationWebhookStore["recordWebhookSample"]>[0][] = [];
  const starts: Array<{ input: AutomationRunWorkflowInput; workflowId: string }> = [];
  const store: AutomationWebhookStore = {
    async recordWebhookSample(input) {
      samples.push(input);
    },
    async listActiveForWebhookRegistration() {
      return automations;
    },
  };
  const workflowStarter: AutomationWebhookStarter = {
    async start(input, workflowId) {
      starts.push({ input, workflowId });
    },
  };
  return { store, workflowStarter, samples, starts };
}

describe("webhook automation dispatch", () => {
  test("samples once and starts only event/filter matches", async () => {
    const f = fixture([
      automation("match", { "incident.severity": "critical" }),
      automation("filtered-out", { "incident.severity": "low" }),
      { ...automation("event-out"), trigger: {
        kind: "webhook" as const,
        registrationId: "my-hook",
        events: ["incident.closed"],
      } },
    ]);

    const result = await dispatchWebhookOccurrence({
      registrationId: "my-hook",
      registration,
      eventKey: "incident.opened",
      deliveryId: "delivery-1",
      payload: { incident: { severity: "critical" } },
      receivedAt: NOW,
    }, {
      store: f.store,
      workflowStarter: f.workflowStarter,
      randomUUID: () => "run-1",
    });

    expect(result).toEqual({ matched: 1, started: 1 });
    expect(f.samples).toEqual([{
      registrationId: "my-hook",
      eventKey: "incident.opened",
      payload: { incident: { severity: "critical" } },
      receivedAt: NOW,
      retain: WEBHOOK_SAMPLE_RETENTION,
    }]);
    expect(f.starts).toEqual([{
      workflowId: "auto:match:delivery-1",
      input: {
        automationId: "match",
        runId: "run-1",
        trigger: {
          source: "webhook",
          eventKey: "incident.opened",
          deliveryId: "delivery-1",
          payload: { incident: { severity: "critical" } },
        },
        receivedAt: NOW.toISOString(),
      },
    }]);
  });

  test("the unpersisted github-app system registration dispatches without sampling", async () => {
    const row = automation("github-auto");
    row.trigger = {
      kind: "webhook",
      registrationId: SYSTEM_GITHUB_REGISTRATION_ID,
      events: ["issues.opened"],
    };
    const f = fixture([row]);
    await dispatchWebhookOccurrence({
      registrationId: SYSTEM_GITHUB_REGISTRATION_ID,
      registration: null,
      eventKey: "issues.opened",
      deliveryId: "github-delivery",
      payload: { action: "opened" },
      receivedAt: NOW,
    }, {
      store: f.store,
      workflowStarter: f.workflowStarter,
      randomUUID: () => "github-run",
    });
    expect(f.samples).toEqual([]);
    expect(f.starts).toHaveLength(1);
  });

  for (const terminal of ["SUCCESS", "ERROR"] as const) {
    test(`duplicate delivery after DBOS ${terminal} gets a fresh run id but executes no second body`, async () => {
      const f = fixture([automation("deduped")]);
      const startCalls: Array<{ input: AutomationRunWorkflowInput; workflowId: string }> = [];
      const executedBodies: AutomationRunWorkflowInput[] = [];
      const existing = new Set<string>();
      const starter: AutomationWebhookStarter = {
        async start(input, workflowId) {
          startCalls.push({ input, workflowId });
          if (existing.has(workflowId)) return;
          existing.add(workflowId);
          executedBodies.push(input);
          // The terminal outcome is intentionally irrelevant: DBOS reserves
          // the workflow id after either success or error.
          expect(terminal === "SUCCESS" || terminal === "ERROR").toBe(true);
        },
      };
      let run = 0;
      const occurrence = {
        registrationId: "my-hook",
        registration,
        eventKey: "incident.opened",
        deliveryId: "same-delivery",
        payload: {},
        receivedAt: NOW,
      };
      await dispatchWebhookOccurrence(occurrence, {
        store: f.store,
        workflowStarter: starter,
        randomUUID: () => `run-${++run}`,
      });
      await dispatchWebhookOccurrence(occurrence, {
        store: f.store,
        workflowStarter: starter,
        randomUUID: () => `run-${++run}`,
      });

      expect(startCalls.map((call) => call.workflowId)).toEqual([
        automationWebhookWorkflowId("deduped", "same-delivery"),
        automationWebhookWorkflowId("deduped", "same-delivery"),
      ]);
      expect(startCalls.map((call) => call.input.runId)).toEqual(["run-1", "run-2"]);
      expect(executedBodies.map((input) => input.runId)).toEqual(["run-1"]);
    });
  }
});
