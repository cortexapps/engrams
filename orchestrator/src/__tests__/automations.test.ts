import type { ProfileApp } from "../db/schema.ts";
import { beforeEach, describe, expect, test } from "bun:test";
import { Code, ConnectError, createClient, createRouterTransport } from "@connectrpc/connect";

import type {
  AutomationInput,
  AutomationRow,
  AutomationRunRow,
  AutomationStore,
  WebhookRegistrationRow,
  WebhookSampleRow,
} from "../db/automations.ts";
import type { ProfileRow } from "../db/profiles.ts";
import type { HarnessCatalogClient } from "../rpc/task-create.ts";
import {
  AutomationService,
  WebhookRegistrationService,
} from "../gen/engram/app/v1/automation_pb.ts";
import { invalidateRegistry } from "../connectors/registry.ts";
import { definitionFromLegacyAction } from "../automations/legacy-compat.ts";
import {
  registerAutomations,
  EVAL_CODE_LIMIT_PER_MINUTE,
  type AutomationDeps,
  type GetSession,
} from "../rpc/automations.ts";

const NOW = new Date("2026-07-22T12:00:00Z");

function session(userId: string | null, role: "user" | "admin" = "user"): GetSession {
  return async () => (userId ? { user: { id: userId, role } } : null);
}

const profile = (apps: ProfileApp[] = []): ProfileRow => ({
  id: "profile-1",
  name: "Automation profile",
  description: "",
  icon: "Bot",
  imageId: "image-1",
  harness: "claude",
  model: null,
  effort: null,
  includeUserTokens: false,
  envVars: {},
  skills: [],
  integrationGrants: [],
  network: { default: "deny", allowHosts: [], allowHostPatterns: [] },
  secrets: [],
  repos: [],
  apps,
  designation: null,
  createdAt: NOW,
  updatedAt: NOW,
  deletedAt: null,
});

/** Two-harness catalog: enough to prove the model/effort enums are validated
 *  against the EFFECTIVE harness, not whichever one is listed first. */
const catalog = (): HarnessCatalogClient => ({
  listHarnesses: async () => ({
    harnesses: [
      {
        name: "claude",
        descriptor: {
          models: [{ id: "opus", default: true, env: {} }, { id: "sonnet", default: false, env: {} }],
          effort: [{ id: "high", default: true, env: {} }],
        },
      },
      {
        name: "codex",
        descriptor: {
          models: [{ id: "gpt", default: true, env: {} }],
          effort: [],
        },
      },
    ],
  }),
});

function fakeStore(seed?: {
  registrations?: WebhookRegistrationRow[];
  samples?: WebhookSampleRow[];
}): AutomationStore {
  const automations = new Map<string, AutomationRow>();
  const registrations = new Map(
    (seed?.registrations ?? []).map((row) => [row.id, row]),
  );
  const samples = new Map((seed?.samples ?? []).map((row) => [row.id, row]));
  let automationSequence = 0;

  const rowFor = (
    id: string,
    input: AutomationInput,
    createdByUserId: string,
  ): AutomationRow => ({
    id,
    ...input,
    kind: "user",
    builtinKey: null,
    currentVersion: 1,
    inputs: {},
    endSessionsOnFinish: false,
    createdByUserId,
    lastFiredAt: null,
    createdAt: NOW,
    updatedAt: NOW,
    archivedAt: null,
  });

  return {
    async list({ includeArchived }) {
      return [...automations.values()].filter((row) => includeArchived || !row.archivedAt);
    },
    async listEnabledForIntegrationTrigger() {
      return [];
    },
    async get(id) {
      return automations.get(id) ?? null;
    },
    async getActive(id) {
      const row = automations.get(id);
      return row && !row.archivedAt ? row : null;
    },
    async listBoundToWebhookRegistration(registrationId) {
      return [...automations.values()]
        .filter(
          (row) =>
            !row.archivedAt &&
            row.trigger.kind === "webhook" &&
            row.trigger.registrationId === registrationId,
        )
        .sort((a, b) => a.id.localeCompare(b.id));
    },
    async listEnabledForWebhookRegistration(registrationId) {
      return [...automations.values()]
        .filter(
          (row) =>
            !row.archivedAt &&
            row.enabled &&
            row.trigger.kind === "webhook" &&
            row.trigger.registrationId === registrationId,
        )
        .sort((a, b) => a.id.localeCompare(b.id))
        .map((row) => ({
          automation: row,
          definition: definitionFromLegacyAction(row.trigger, row.action),
        }));
    },
    async getVersion(automationId, version) {
      const row = automations.get(automationId);
      if (!row || version !== row.currentVersion) return null;
      const definition = definitionFromLegacyAction(row.trigger, row.action);
      return {
        automationId,
        version,
        trigger: definition.trigger,
        blocks: definition.blocks,
        inputsSchema: definition.inputsSchema,
        settings: definition.settings,
        createdByUserId: row.createdByUserId,
        createdAt: row.createdAt,
      };
    },
    async create(input, createdByUserId) {
      const row = rowFor(`automation-${++automationSequence}`, input, createdByUserId);
      automations.set(row.id, row);
      return row;
    },
    async update(id, input) {
      const existing = automations.get(id);
      if (!existing || existing.archivedAt) return null;
      const row = { ...existing, ...input, updatedAt: NOW };
      automations.set(id, row);
      return row;
    },
    async archive(id) {
      const existing = automations.get(id);
      if (!existing) return null;
      const row = existing.archivedAt
        ? existing
        : { ...existing, enabled: false, nextFireAt: null, archivedAt: NOW, updatedAt: NOW };
      automations.set(id, row);
      return row;
    },
    async setEnabled(id, enabled, nextFireAt) {
      const existing = automations.get(id);
      if (!existing || existing.archivedAt) return null;
      const row = {
        ...existing,
        enabled,
        ...(nextFireAt !== undefined ? { nextFireAt } : {}),
        updatedAt: NOW,
      };
      automations.set(id, row);
      return row;
    },
    async listRuns(): Promise<AutomationRunRow[]> {
      return [];
    },
    async createRegistration(input) {
      const row: WebhookRegistrationRow = {
        ...input,
        createdAt: NOW,
        updatedAt: NOW,
      };
      registrations.set(row.id, row);
      return row;
    },
    async getRegistration(id) {
      return registrations.get(id) ?? null;
    },
    async listRegistrations() {
      return [...registrations.values()];
    },
    async deleteRegistration(id) {
      return registrations.delete(id);
    },
    async getSample(id) {
      return samples.get(id) ?? null;
    },
    async recordWebhookSample(input) {
      const id = `sample-${samples.size + 1}`;
      samples.set(id, { id, ...input });
    },
    async getLatestSample(registrationId, eventKey) {
      return (
        [...samples.values()]
          .filter(
            (row) =>
              row.registrationId === registrationId &&
              (eventKey === undefined || row.eventKey === eventKey),
          )
          .sort((a, b) => b.receivedAt.getTime() - a.receivedAt.getTime())[0] ?? null
      );
    },
    async listSamples(registrationId, eventKey, limit) {
      return [...samples.values()]
        .filter(
          (row) =>
            row.registrationId === registrationId &&
            (eventKey === undefined || row.eventKey === eventKey),
        )
        .sort((a, b) => b.receivedAt.getTime() - a.receivedAt.getTime())
        .slice(0, limit);
    },
    async listObservedEventKeys(registrationId) {
      return [
        ...new Set(
          [...samples.values()]
            .filter((row) => row.registrationId === registrationId)
            .map((row) => row.eventKey),
        ),
      ].sort();
    },
  };
}

function clients(deps: AutomationDeps) {
  const transport = createRouterTransport((router) => registerAutomations(router, deps));
  return {
    automations: createClient(AutomationService, transport),
    registrations: createClient(WebhookRegistrationService, transport),
  };
}

async function expectCode(promise: Promise<unknown>, code: Code): Promise<void> {
  try {
    await promise;
    throw new Error(`expected ${Code[code]}`);
  } catch (error) {
    if (!(error instanceof ConnectError)) throw error;
    expect(error.code).toBe(code);
  }
}

const cronRequest = {
  name: "Daily triage",
  description: "",
  enabled: true,
  trigger: {
    trigger: {
      case: "cron" as const,
      value: { schedule: "0 9 * * *", timezone: "America/Los_Angeles" },
    },
  },
  action: {
    action: {
      case: "createTask" as const,
      value: {
        profileId: "profile-1",
        promptTemplate: "Triage the queue",
        includeEventContext: false,
      },
    },
  },
};

beforeEach(() => invalidateRegistry());

describe("AutomationService", () => {
  test("both services are admin-gated", async () => {
    const store = fakeStore();
    const anonymous = clients({ getSession: session(null), store, profiles: { getActive: async () => profile() } });
    await expectCode(anonymous.automations.listAutomations({}), Code.Unauthenticated);

    const member = clients({ getSession: session("member"), store, profiles: { getActive: async () => profile() } });
    await expectCode(member.automations.createAutomation(cronRequest), Code.PermissionDenied);
    await expectCode(member.registrations.listWebhookRegistrations({}), Code.PermissionDenied);
  });

  test("validates cron expressions and IANA timezones before saving", async () => {
    const store = fakeStore();
    const { automations } = clients({
      getSession: session("admin", "admin"),
      store,
      profiles: { getActive: async () => profile() },
      now: () => NOW,
    });
    await expectCode(
      automations.createAutomation({
        ...cronRequest,
        trigger: { trigger: { case: "cron", value: { schedule: "not cron", timezone: "UTC" } } },
      }),
      Code.InvalidArgument,
    );
    await expectCode(
      automations.createAutomation({
        ...cronRequest,
        trigger: { trigger: { case: "cron", value: { schedule: "0 9 * * *", timezone: "Mars/Olympus" } } },
      }),
      Code.InvalidArgument,
    );

    const response = await automations.createAutomation(cronRequest);
    expect(response.automation?.nextFireAt).toBeTruthy();
  });

  test("accepts a profile that declares apps — the run ignores them", async () => {
    const { automations } = clients({
      getSession: session("admin", "admin"),
      store: fakeStore(),
      profiles: { getActive: async () => profile([{ name: "web", port: 3000 }]) },
      now: () => NOW,
    });
    const response = await automations.createAutomation(cronRequest);
    expect(response.automation?.id).toBeTruthy();
  });

  test("persists a harness/model/effort override and rejects ids the catalog lacks", async () => {
    const { automations } = clients({
      getSession: session("admin", "admin"),
      store: fakeStore(),
      profiles: { getActive: async () => profile() },
      harnessCatalog: catalog(),
      now: () => NOW,
    });
    const withOverride = (override: {
      harness?: string;
      model?: string;
      effort?: string;
    }) => automations.createAutomation({
      ...cronRequest,
      action: {
        action: {
          case: "createTask" as const,
          value: { ...cronRequest.action.action.value, ...override },
        },
      },
    });

    // The effective harness is the override, else the profile's ("claude").
    await expectCode(withOverride({ harness: "ghost" }), Code.InvalidArgument);
    await expectCode(withOverride({ model: "haiku" }), Code.InvalidArgument);
    await expectCode(withOverride({ effort: "max" }), Code.InvalidArgument);
    await expectCode(withOverride({ harness: "codex", model: "sonnet" }), Code.InvalidArgument);

    const created = await withOverride({ harness: "codex", model: "gpt" });
    expect(created.automation?.action?.action.value).toMatchObject({
      harness: "codex",
      model: "gpt",
    });
    expect(created.automation?.action?.action.value?.effort).toBeUndefined();
  });

  // Regression: a model/effort id is only meaningful next to one harness. Saving
  // one without a harness used to leave the action pointing at whatever harness
  // the PROFILE happened to name; an admin who later switched that profile to
  // another harness orphaned the id, and the launch path resolves an unknown id
  // to no model env at all — a silently wrong model, months later. The save now
  // pins the harness so the stored action cannot be orphaned.
  test("a model-only or effort-only override pins the profile's harness onto the action", async () => {
    const { automations } = clients({
      getSession: session("admin", "admin"),
      store: fakeStore(),
      profiles: { getActive: async () => profile() },
      harnessCatalog: catalog(),
      now: () => NOW,
    });
    const withOverride = (override: { model?: string; effort?: string }) =>
      automations.createAutomation({
        ...cronRequest,
        action: {
          action: {
            case: "createTask" as const,
            value: { ...cronRequest.action.action.value, ...override },
          },
        },
      });

    const modelOnly = await withOverride({ model: "sonnet" });
    expect(modelOnly.automation?.action?.action.value).toMatchObject({
      harness: "claude",
      model: "sonnet",
    });

    const effortOnly = await withOverride({ effort: "high" });
    expect(effortOnly.automation?.action?.action.value).toMatchObject({
      harness: "claude",
      effort: "high",
    });
    expect(effortOnly.automation?.action?.action.value?.model).toBeUndefined();
  });

  test("an automation with no override never reads the harness catalog", async () => {
    let reads = 0;
    const { automations } = clients({
      getSession: session("admin", "admin"),
      store: fakeStore(),
      profiles: { getActive: async () => profile() },
      harnessCatalog: {
        listHarnesses: async () => {
          reads++;
          return { harnesses: [] };
        },
      },
      now: () => NOW,
    });

    const created = await automations.createAutomation(cronRequest);
    expect(created.automation?.action?.action.value?.harness).toBeUndefined();
    expect(reads).toBe(0);
  });

  test("webhook filters accept only bounded dotted payload-path equality keys", async () => {
    const hook: WebhookRegistrationRow = {
      id: "generic",
      name: "Generic",
      verification: {
        scheme: "generic_hmac_sha256",
        secretRef: "webhook.generic.secret",
      },
      providerHint: null,
      createdByUserId: "admin",
      createdAt: NOW,
      updatedAt: NOW,
    };
    const { automations } = clients({
      getSession: session("admin", "admin"),
      store: fakeStore({ registrations: [hook] }),
      profiles: { getActive: async () => profile() },
      now: () => NOW,
    });
    const request = (filterJson: string) => automations.createAutomation({
      name: "Filtered hook",
      description: "",
      enabled: true,
      trigger: {
        trigger: {
          case: "webhook",
          value: {
            registrationId: "generic",
            events: ["incident.opened"],
            filterJson,
          },
        },
      },
      action: cronRequest.action,
    });

    await expectCode(request(JSON.stringify({ "incident.__proto__.admin": true })), Code.InvalidArgument);
    const created = await request(JSON.stringify({ "incident.severity": "critical" }));
    expect(created.automation?.trigger?.trigger.value).toMatchObject({
      filterJson: JSON.stringify({ "incident.severity": "critical" }),
    });
  });

  test("accepts the github-app system registration without a PG registration row", async () => {
    const { automations } = clients({
      getSession: session("admin", "admin"),
      store: fakeStore(),
      profiles: { getActive: async () => profile() },
      now: () => NOW,
    });
    const created = await automations.createAutomation({
      name: "GitHub issues",
      description: "",
      enabled: true,
      trigger: {
        trigger: {
          case: "webhook",
          value: {
            registrationId: "github-app",
            events: ["issues.opened"],
          },
        },
      },
      action: cronRequest.action,
    });
    expect(created.automation?.trigger?.trigger.value).toMatchObject({
      registrationId: "github-app",
      events: ["issues.opened"],
    });
  });
});

describe("WebhookRegistrationService", () => {
  test("seals a generated secret under the deterministic ref and returns it once", async () => {
    const store = fakeStore();
    const writes: Array<{ name: string; value: string }> = [];
    const { registrations } = clients({
      getSession: session("admin", "admin"),
      store,
      profiles: { getActive: async () => profile() },
      randomSecret: () => "generated-secret",
      orgSecret: {
        async putSecret(request) {
          writes.push(request);
          return {};
        },
        async deleteSecret() {
          return { deleted: true };
        },
      },
    });

    const created = await registrations.createWebhookRegistration({
      id: "github-team",
      name: "GitHub team",
      verificationScheme: "github_hmac_sha256",
      providerHint: "github",
    });
    expect(created.secret).toBe("generated-secret");
    expect(writes).toEqual([
      { name: "webhook.github-team.secret", value: "generated-secret" },
    ]);

    const listed = await registrations.listWebhookRegistrations({});
    expect(listed.registrations.map((row) => row.id)).toEqual(["github-team"]);
    expect(Object.keys(listed.registrations[0]!)).not.toContain("secret");
  });

  test("rolls the registration back when secret provisioning fails", async () => {
    const store = fakeStore();
    const { registrations } = clients({
      getSession: session("admin", "admin"),
      store,
      profiles: { getActive: async () => profile() },
      randomSecret: () => "generated-secret",
      orgSecret: {
        async putSecret() {
          throw new Error("coordinator unavailable");
        },
        async deleteSecret() {
          return { deleted: false };
        },
      },
    });
    await expectCode(
      registrations.createWebhookRegistration({
        id: "generic",
        name: "Generic",
        verificationScheme: "generic_hmac_sha256",
      }),
      Code.Internal,
    );
    expect(await store.getRegistration("generic")).toBeNull();
  });

  test("refuses deletion while an active automation is bound, then deletes after archive", async () => {
    const store = fakeStore();
    const deletedSecrets: string[] = [];
    const { automations, registrations } = clients({
      getSession: session("admin", "admin"),
      store,
      profiles: { getActive: async () => profile() },
      randomSecret: () => "generated-secret",
      orgSecret: {
        async putSecret() {
          return {};
        },
        async deleteSecret(request) {
          deletedSecrets.push(request.name);
          return { deleted: true };
        },
      },
    });

    await registrations.createWebhookRegistration({
      id: "generic",
      name: "Generic",
      verificationScheme: "generic_hmac_sha256",
    });
    const created = await automations.createAutomation({
      name: "Webhook triage",
      description: "",
      enabled: true,
      trigger: {
        trigger: {
          case: "webhook",
          value: { registrationId: "generic", events: ["issue.created"] },
        },
      },
      action: cronRequest.action,
    });

    try {
      await registrations.deleteWebhookRegistration({ id: "generic" });
      throw new Error("expected FailedPrecondition");
    } catch (error) {
      expect(error).toBeInstanceOf(ConnectError);
      if (!(error instanceof ConnectError)) throw error;
      expect(error.code).toBe(Code.FailedPrecondition);
      expect(error.message).toContain(
        `cannot delete webhook registration "generic": 1 non-archived automation(s) reference it (${created.automation!.id})`,
      );
    }
    expect(await store.getRegistration("generic")).not.toBeNull();
    expect(deletedSecrets).toEqual([]);

    // The prod-validation regression (2026-07-22): a DISABLED automation still
    // references its registration — disabling pauses firing, it does not
    // release the binding. Deletion must still refuse.
    await automations.setAutomationEnabled({ id: created.automation!.id, enabled: false });
    try {
      await registrations.deleteWebhookRegistration({ id: "generic" });
      throw new Error("expected FailedPrecondition for the disabled automation");
    } catch (error) {
      expect(error).toBeInstanceOf(ConnectError);
      if (!(error instanceof ConnectError)) throw error;
      expect(error.code).toBe(Code.FailedPrecondition);
    }
    expect(await store.getRegistration("generic")).not.toBeNull();
    expect(deletedSecrets).toEqual([]);

    await automations.archiveAutomation({ id: created.automation!.id });
    const deleted = await registrations.deleteWebhookRegistration({ id: "generic" });
    expect(deleted.deleted).toBe(true);
    expect(await store.getRegistration("generic")).toBeNull();
    expect(deletedSecrets).toEqual(["webhook.generic.secret"]);
  });

  test("ListEvents merges provider taxonomy with observed sample keys", async () => {
    const registration: WebhookRegistrationRow = {
      id: "github-team",
      name: "GitHub team",
      verification: {
        scheme: "github_hmac_sha256",
        secretRef: "webhook.github-team.secret",
      },
      providerHint: "github",
      createdByUserId: "admin",
      createdAt: NOW,
      updatedAt: NOW,
    };
    const sample: WebhookSampleRow = {
      id: "sample-1",
      registrationId: registration.id,
      eventKey: "release.published",
      payload: {},
      receivedAt: NOW,
    };
    const { registrations } = clients({
      getSession: session("admin", "admin"),
      store: fakeStore({ registrations: [registration], samples: [sample] }),
      profiles: { getActive: async () => profile() },
      connectors: { async list() { return []; } },
    });

    const response = await registrations.listWebhookEvents({
      registrationId: registration.id,
    });
    expect(response.events.find((event) => event.key === "issues.opened")).toMatchObject({
      observed: false,
    });
    expect(response.events.find((event) => event.key === "release.published")).toMatchObject({
      observed: true,
    });
    expect(response.variables.some((variable) => variable.alias === "issue.title")).toBe(true);
    // Hidden lifecycle events (installation.*) reach the ledger but never the
    // trigger picker.
    expect(response.events.some((event) => event.key.startsWith("installation"))).toBe(false);
  });
});

describe("AutomationService.EvalCode", () => {
  const admin = () => ({
    getSession: session("admin-1", "admin"),
    store: fakeStore(),
    profiles: { getActive: async () => profile() },
  });

  test("runs the real sandbox and returns value, logs, duration", async () => {
    const { automations } = clients(admin());
    const response = await automations.evalCode({
      source: `export default ({ event }) => { console.log("seen", event.n); return event.n * 2; };`,
      mode: "value",
      inputJson: JSON.stringify({ event: { n: 21 } }),
    });
    expect(response.valueJson).toBe("42");
    expect(response.logs).toEqual(["log: seen 21"]);
    expect(response.errorName).toBeUndefined();
  });

  test("returns typed errors with line numbers instead of failing the RPC", async () => {
    const { automations } = clients(admin());
    const response = await automations.evalCode({
      source: 'export default () => {\n  throw new Error("boom");\n};',
      mode: "value",
      inputJson: "",
    });
    expect(response.valueJson).toBeUndefined();
    expect(response.errorName).toBe("Error");
    expect(response.errorMessage).toBe("boom");
    expect(response.errorLine).toBe(2);
  });

  test("admin-gated", async () => {
    const member = clients({ ...admin(), getSession: session("user-1", "user") });
    await expectCode(
      member.automations.evalCode({ source: "export default () => 1;", mode: "value", inputJson: "" }),
      Code.PermissionDenied,
    );
    const anonymous = clients({ ...admin(), getSession: session(null) });
    await expectCode(
      anonymous.automations.evalCode({ source: "export default () => 1;", mode: "value", inputJson: "" }),
      Code.Unauthenticated,
    );
  });

  test("validates mode and input_json", async () => {
    const { automations } = clients(admin());
    await expectCode(
      automations.evalCode({ source: "export default () => 1;", mode: "maybe", inputJson: "" }),
      Code.InvalidArgument,
    );
    await expectCode(
      automations.evalCode({ source: "export default () => 1;", mode: "value", inputJson: "not json" }),
      Code.InvalidArgument,
    );
    await expectCode(
      automations.evalCode({ source: "export default () => 1;", mode: "value", inputJson: "[1,2]" }),
      Code.InvalidArgument,
    );
  });

  test("rate limits per user with a fake sandbox", async () => {
    let calls = 0;
    const { automations } = clients({
      ...admin(),
      evalCode: async () => {
        calls += 1;
        return { ok: true, value: calls, durationMs: 1, logs: [] };
      },
    });
    for (let i = 0; i < EVAL_CODE_LIMIT_PER_MINUTE; i += 1) {
      await automations.evalCode({ source: "export default () => 1;", mode: "value", inputJson: "" });
    }
    await expectCode(
      automations.evalCode({ source: "export default () => 1;", mode: "value", inputJson: "" }),
      Code.ResourceExhausted,
    );
    expect(calls).toBe(EVAL_CODE_LIMIT_PER_MINUTE);
  });
});


describe("integration triggers (2.C)", () => {
  const integrationRequest = (overrides = {}) => ({
    ...cronRequest,
    name: "PR watcher",
    trigger: {
      trigger: {
        case: "integration" as const,
        value: {
          provider: "github",
          connectionId: "conn-github",
          eventKeys: ["pull_request.opened", "issue_comment.created"],
          scopeValues: [],
          ...overrides,
        },
      },
    },
  });

  const baseDeps = (store = fakeStore()) => ({
    getSession: session("admin-1", "admin"),
    store,
    profiles: { getActive: async () => profile() },
    connectors: { async list() { return []; } },
    harnessCatalog: catalog(),
    now: () => NOW,
  });

  test("create round-trips an integration trigger with input-bound scope", async () => {
    const store = fakeStore();
    const { automations } = clients(baseDeps(store));
    const created = await automations.createAutomation(integrationRequest({ scopeFromInput: "repos" }));
    const trigger = created.automation?.trigger?.trigger;
    if (trigger?.case !== "integration") throw new Error("expected integration trigger");
    expect(trigger.value.provider).toBe("github");
    expect(trigger.value.connectionId).toBe("conn-github");
    expect(trigger.value.eventKeys).toEqual(["pull_request.opened", "issue_comment.created"]);
    expect(trigger.value.scopeFromInput).toBe("repos");

    const fetched = await automations.getAutomation({ id: created.automation!.id });
    expect(fetched.automation?.trigger?.trigger.case).toBe("integration");
  });

  test("create round-trips literal scope values", async () => {
    const { automations } = clients(baseDeps());
    const created = await automations.createAutomation(integrationRequest({ scopeValues: ["engrams/engrams"] }));
    const trigger = created.automation?.trigger?.trigger;
    if (trigger?.case !== "integration") throw new Error("expected integration trigger");
    expect(trigger.value.scopeValues).toEqual(["engrams/engrams"]);
  });

  test("rejects undeclared event keys, unknown providers, and double scope", async () => {
    const { automations } = clients(baseDeps());
    await expectCode(
      automations.createAutomation(integrationRequest({ eventKeys: ["pull_request.opened", "made.up"] })),
      Code.InvalidArgument,
    );
    await expectCode(
      automations.createAutomation(integrationRequest({ provider: "notaprovider" })),
      Code.InvalidArgument,
    );
    await expectCode(
      automations.createAutomation(
        integrationRequest({ scopeValues: ["engrams/engrams"], scopeFromInput: "repos" }),
      ),
      Code.InvalidArgument,
    );
  });

  test("listEventCatalog serves labels, schemas, samples, observed and hidden flags", async () => {
    const latest = { issue: { title: "from-ledger" } };
    const { automations } = clients({
      ...baseDeps(),
      connections: {
        getDefault: async () => ({
          id: "conn-github", alias: "default", provider: "github",
          displayName: "GitHub (default)", isDefault: true, config: {},
          enabled: true, testedAt: null, createdAt: NOW, updatedAt: NOW,
        }),
      },
      integrationEvents: {
        getLatest: async (_connectionId: string, eventKey: string) =>
          eventKey === "issues.opened"
            ? { id: "evt-1", provider: "github", connectionId: "conn-github",
                eventKey, deliveryId: "d1", payload: latest, scopeValue: null,
                receivedAt: NOW }
            : null,
        listObservedEventKeys: async () => ["issues.opened"],
      },
    });
    const response = await automations.listEventCatalog({ provider: "github" });
    const issues = response.events.find((event) => event.key === "issues.opened");
    expect(issues).toMatchObject({ observed: true });
    expect(JSON.parse(issues!.sampleJson)).toEqual(latest);
    const pr = response.events.find((event) => event.key === "pull_request.opened");
    expect(pr).toMatchObject({ observed: false, hidden: false });
    expect(pr!.label.length).toBeGreaterThan(0);
    expect(JSON.parse(pr!.sampleJson)).toBeTruthy(); // fixture fallback
    expect(JSON.parse(pr!.schemaJson)).toMatchObject({ type: "object" });
    const installation = response.events.find((event) => event.key === "installation.created");
    expect(installation?.hidden).toBe(true);
    expect(response.scope).toMatchObject({ key: "repositories" });
    expect(response.defaultConnectionId).toBe("conn-github");
  });

  test("listEventCatalog without a connection still serves fixtures", async () => {
    const { automations } = clients({
      ...baseDeps(),
      connections: { getDefault: async () => null },
      integrationEvents: {
        getLatest: async () => null,
        listObservedEventKeys: async () => [],
      },
    });
    const response = await automations.listEventCatalog({ provider: "slack" });
    expect(response.defaultConnectionId).toBe("");
    expect(response.events.every((event) => !event.observed)).toBe(true);
  });

  test("listActionCatalog is a member-safe projection", async () => {
    const { automations } = clients(baseDeps());
    const response = await automations.listActionCatalog({ provider: "github" });
    const post = response.actions.find((action) => action.id === "create_issue_comment");
    expect(post).toBeDefined();
    expect(post!.label.length).toBeGreaterThan(0);
    expect(JSON.parse(post!.inputSchemaJson)).toMatchObject({ type: "object" });
    expect(Object.keys(post!)).not.toContain("execute");
    await expectCode(
      automations.listActionCatalog({ provider: "notaprovider" }),
      Code.InvalidArgument,
    );
  });

  test("catalogs are admin-gated", async () => {
    const { automations } = clients({ ...baseDeps(), getSession: session("user-1", "user") });
    await expectCode(automations.listEventCatalog({ provider: "github" }), Code.PermissionDenied);
    await expectCode(automations.listActionCatalog({ provider: "github" }), Code.PermissionDenied);
  });
});
