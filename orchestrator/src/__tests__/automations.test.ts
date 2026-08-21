import { beforeEach, describe, expect, test } from "bun:test";
import { Code, ConnectError, createClient, createRouterTransport } from "@connectrpc/connect";

import type { ProfileApp } from "../db/schema.ts";
import type {
  AutomationMetaRow,
  AutomationRow,
  AutomationRunRow,
  AutomationStepRunRow,
  AutomationStore,
  AutomationVersionRow,
  AutomationDispatchStore,
  CreateAutomationInput,
  WebhookRegistrationRow,
  WebhookSampleRow,
} from "../db/automations.ts";
import type { ProfileRow } from "../db/profiles.ts";
import type { HarnessCatalogClient } from "../rpc/task-create.ts";
import type { AutomationDefinition, BlockOverrides } from "../automations/engine/definition.ts";
import type { AutomationInbox } from "../automations/engine/inbox.ts";
import { registerEngineBlocks } from "../automations/engine/blocks/index.ts";
import {
  AutomationRunService,
  AutomationService,
  WebhookRegistrationService,
} from "../gen/engram/app/v1/automation_pb.ts";
import { invalidateRegistry } from "../connectors/registry.ts";
import {
  registerAutomations,
  triggerSummary,
  EVAL_CODE_LIMIT_PER_MINUTE,
  type AutomationDeps,
  type GetSession,
} from "../rpc/automations.ts";

registerEngineBlocks();

const NOW = new Date("2026-08-21T12:00:00Z");

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
        descriptor: { models: [{ id: "gpt", default: true, env: {} }], effort: [] },
      },
    ],
  }),
});

// ---------------------------------------------------------------------------
// In-memory store (AutomationStore + AutomationDispatchStore)
// ---------------------------------------------------------------------------

interface Stored {
  meta: AutomationMetaRow;
  versions: Map<number, AutomationVersionRow>;
}

function fakeStore(seed?: {
  registrations?: WebhookRegistrationRow[];
  samples?: WebhookSampleRow[];
  automations?: Stored[];
  runs?: AutomationRunRow[];
  steps?: AutomationStepRunRow[];
}) {
  const automations = new Map<string, Stored>((seed?.automations ?? []).map((a) => [a.meta.id, a]));
  const registrations = new Map((seed?.registrations ?? []).map((row) => [row.id, row]));
  const samples = new Map((seed?.samples ?? []).map((row) => [row.id, row]));
  const runs = new Map<string, AutomationRunRow>((seed?.runs ?? []).map((r) => [r.id, r]));
  const steps = seed?.steps ?? [];
  const claims = new Map<string, string>();
  let sequence = 0;

  const row = (stored: Stored): AutomationRow => ({
    ...stored.meta,
    version: stored.versions.get(stored.meta.currentVersion)!,
  });
  const versionOf = (automationId: string, version: number, d: AutomationDefinition, by: string | null): AutomationVersionRow => ({
    automationId,
    version,
    trigger: d.trigger,
    blocks: d.blocks,
    inputsSchema: d.inputsSchema,
    settings: d.settings,
    createdByUserId: by,
    createdAt: NOW,
  });

  const store: AutomationStore & AutomationDispatchStore = {
    async list({ includeArchived }) {
      return [...automations.values()].filter((a) => includeArchived || !a.meta.archivedAt).map(row);
    },
    async get(id) {
      const a = automations.get(id);
      return a ? row(a) : null;
    },
    async getActive(id) {
      const a = automations.get(id);
      return a && !a.meta.archivedAt ? row(a) : null;
    },
    async getByBuiltinKey(key) {
      const a = [...automations.values()].find((x) => x.meta.builtinKey === key);
      return a ? row(a) : null;
    },
    async listBoundToWebhookRegistration(registrationId) {
      return [...automations.values()]
        .filter((a) => {
          const t = a.versions.get(a.meta.currentVersion)!.trigger;
          return !a.meta.archivedAt && t.kind === "webhook" && t.registrationId === registrationId;
        })
        .map((a) => a.meta);
    },
    async listEnabledForWebhookRegistration() {
      return [];
    },
    async listEnabledForIntegrationTrigger() {
      return [];
    },
    async create(input: CreateAutomationInput, createdByUserId) {
      const id = `automation-${++sequence}`;
      const meta: AutomationMetaRow = {
        id,
        name: input.name,
        description: input.description,
        enabled: input.enabled,
        kind: input.kind ?? "user",
        builtinKey: input.builtinKey ?? null,
        currentVersion: 1,
        inputs: input.inputs ?? {},
        blockOverrides: {},
        endSessionsOnFinish: input.definition.settings.endSessionsOnFinish,
        createdByUserId,
        nextFireAt: input.nextFireAt,
        lastFiredAt: null,
        createdAt: NOW,
        updatedAt: NOW,
        archivedAt: null,
      };
      automations.set(id, { meta, versions: new Map([[1, versionOf(id, 1, input.definition, createdByUserId)]]) });
      return row(automations.get(id)!);
    },
    async saveVersion(automationId, definition, by, patch) {
      const a = automations.get(automationId);
      if (!a || a.meta.archivedAt) return null;
      const next = a.meta.currentVersion + 1;
      a.versions.set(next, versionOf(automationId, next, definition, by));
      a.meta = {
        ...a.meta,
        currentVersion: next,
        endSessionsOnFinish: definition.settings.endSessionsOnFinish,
        ...(patch?.name !== undefined ? { name: patch.name } : {}),
        ...(patch?.description !== undefined ? { description: patch.description } : {}),
        ...(patch?.nextFireAt !== undefined ? { nextFireAt: patch.nextFireAt } : {}),
      };
      return row(a);
    },
    async updateMeta(id, patch) {
      const a = automations.get(id);
      if (!a || a.meta.archivedAt) return null;
      a.meta = { ...a.meta, ...(patch.name !== undefined ? { name: patch.name } : {}), ...(patch.description !== undefined ? { description: patch.description } : {}) };
      return row(a);
    },
    async setInputs(id, inputs) {
      const a = automations.get(id);
      if (!a) return null;
      a.meta = { ...a.meta, inputs };
      return row(a);
    },
    async setBlockOverrides(id, overrides: BlockOverrides) {
      const a = automations.get(id);
      if (!a) return null;
      a.meta = { ...a.meta, blockOverrides: overrides };
      return row(a);
    },
    async archive(id) {
      const a = automations.get(id);
      if (!a) return null;
      a.meta = { ...a.meta, archivedAt: NOW, enabled: false, nextFireAt: null };
      return row(a);
    },
    async setEnabled(id, enabled, nextFireAt) {
      const a = automations.get(id);
      if (!a || a.meta.archivedAt) return null;
      a.meta = { ...a.meta, enabled, ...(nextFireAt !== undefined ? { nextFireAt } : {}) };
      return row(a);
    },
    async getVersion(automationId, version) {
      return automations.get(automationId)?.versions.get(version) ?? null;
    },
    async listVersions(automationId) {
      return [...(automations.get(automationId)?.versions.values() ?? [])].sort((x, y) => y.version - x.version);
    },
    async listRuns(automationId, limit) {
      return [...runs.values()]
        .filter((r) => r.automationId === automationId)
        .sort((x, y) => y.createdAt.getTime() - x.createdAt.getTime())
        .slice(0, limit);
    },
    async getRun(id) {
      return runs.get(id) ?? null;
    },
    async listStepRuns(runId) {
      return steps.filter((s) => s.runId === runId);
    },
    async listRunSessionIds() {
      return [{ sessionId: "sess-1", blockId: "launch" }];
    },
    async latestRuns(ids) {
      const out = new Map<string, AutomationRunRow>();
      for (const id of ids) {
        const latest = (await this.listRuns(id, 1))[0];
        if (latest) out.set(id, latest);
      }
      return out;
    },
    async runCounts7d(ids) {
      const out = new Map();
      for (const id of ids) {
        const day = NOW.toISOString().slice(0, 10);
        const mine = [...runs.values()].filter((r) => r.automationId === id);
        if (mine.length === 0) continue;
        out.set(id, [
          {
            day,
            completed: mine.filter((r) => r.status === "completed").length,
            failed: mine.filter((r) => r.status === "failed").length,
            filtered: mine.filter((r) => r.status === "filtered").length,
            other: 0,
          },
        ]);
      }
      return out;
    },
    async createRegistration(input) {
      const r: WebhookRegistrationRow = { ...input, disabledReason: null, createdAt: NOW, updatedAt: NOW };
      registrations.set(r.id, r);
      return r;
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
    async setRegistrationDisabledReason(id, reason) {
      const r = registrations.get(id);
      if (!r) return false;
      registrations.set(id, { ...r, disabledReason: reason });
      return true;
    },
    async replaceCurrentDefinition(automationId, definition) {
      const saved = await this.saveVersion(automationId, definition, null);
      return saved;
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
          .filter((r) => r.registrationId === registrationId && (eventKey === undefined || r.eventKey === eventKey))
          .sort((x, y) => y.receivedAt.getTime() - x.receivedAt.getTime())[0] ?? null
      );
    },
    async listSamples(registrationId, eventKey, limit) {
      return [...samples.values()]
        .filter((r) => r.registrationId === registrationId && (eventKey === undefined || r.eventKey === eventKey))
        .slice(0, limit);
    },
    async listObservedEventKeys(registrationId) {
      return [...new Set([...samples.values()].filter((r) => r.registrationId === registrationId).map((r) => r.eventKey))].sort();
    },
    // dispatch admission
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
          createdAt: NOW,
        });
      }
      return runs.get(input.id)!;
    },
    async claimConcurrency(automationId, key, runId) {
      const k = `${automationId}:${key}`;
      const holder = claims.get(k);
      if (holder === undefined || holder === runId) {
        claims.set(k, runId);
        return { claimed: true };
      }
      return { claimed: false, holderRunId: holder };
    },
    async casConcurrency(automationId, key, from, to) {
      const k = `${automationId}:${key}`;
      if (claims.get(k) !== from) return false;
      claims.set(k, to);
      return true;
    },
  };
  return { store, automations, runs, registrations };
}

function clients(deps: AutomationDeps) {
  const transport = createRouterTransport((router) => registerAutomations(router, deps));
  return {
    automations: createClient(AutomationService, transport),
    runsApi: createClient(AutomationRunService, transport),
    registrations: createClient(WebhookRegistrationService, transport),
  };
}

async function expectCode(promise: Promise<unknown>, code: Code): Promise<ConnectError> {
  try {
    await promise;
    throw new Error(`expected ${Code[code]}`);
  } catch (error) {
    if (!(error instanceof ConnectError)) throw error;
    expect(error.code).toBe(code);
    return error;
  }
}

function cronDefinition(overrides: Partial<AutomationDefinition> = {}): AutomationDefinition {
  return {
    engine: 1,
    trigger: { kind: "cron", schedule: "0 9 * * *", timezone: "America/Los_Angeles" },
    blocks: [
      {
        id: "launch",
        type: "create_session",
        config: { profileId: "profile-1", promptTemplate: "Triage ${{ trigger.automation.name }}" },
        tunable: ["promptTemplate", "model"],
      },
    ],
    inputsSchema: [{ key: "mention", label: "Mention", type: "string", default: "@engrams" }],
    settings: { endSessionsOnFinish: false },
    ...overrides,
  };
}

function adminDeps(overrides: Partial<AutomationDeps> = {}): AutomationDeps & { fake: ReturnType<typeof fakeStore> } {
  const fake = fakeStore();
  return {
    getSession: session("admin-1", "admin"),
    store: fake.store,
    profiles: { getActive: async (id: string) => (id === "profile-1" ? profile() : null) },
    harnessCatalog: catalog(),
    connectors: { async list() { return []; } },
    workflowStarter: { async start() {} },
    sender: { async send() {} },
    now: () => NOW,
    randomId: () => "fixed-id",
    fake,
    ...overrides,
  };
}

function builtinStored(id = "builtin-1"): Stored {
  const d = cronDefinition();
  return {
    meta: {
      id,
      name: "PR review",
      description: "",
      enabled: true,
      kind: "builtin",
      builtinKey: "pr_review",
      currentVersion: 1,
      inputs: { mention: "@engrams" },
      blockOverrides: {},
      endSessionsOnFinish: false,
      createdByUserId: null,
      nextFireAt: null,
      lastFiredAt: null,
      createdAt: NOW,
      updatedAt: NOW,
      archivedAt: null,
    },
    versions: new Map([[1, { automationId: id, version: 1, trigger: d.trigger, blocks: d.blocks, inputsSchema: d.inputsSchema, settings: d.settings, createdByUserId: null, createdAt: NOW }]]),
  };
}

beforeEach(() => invalidateRegistry());

// ---------------------------------------------------------------------------

describe("AutomationService v2", () => {
  test("all services are admin-gated", async () => {
    const member = clients(adminDeps({ getSession: session("user-1", "user") }));
    await expectCode(member.automations.listAutomations({}), Code.PermissionDenied);
    await expectCode(member.runsApi.listRuns({ automationId: "x" }), Code.PermissionDenied);
    await expectCode(member.registrations.listWebhookRegistrations({}), Code.PermissionDenied);
    const anonymous = clients(adminDeps({ getSession: session(null) }));
    await expectCode(anonymous.automations.listAutomations({}), Code.Unauthenticated);
  });

  test("create → version 1; save → version 2; list carries summaries", async () => {
    const deps = adminDeps();
    const { automations } = clients(deps);
    const created = await automations.createAutomation({
      name: "Daily triage",
      description: "",
      enabled: true,
      definitionJson: JSON.stringify(cronDefinition()),
      inputsJson: "{}",
    });
    expect(created.automation?.currentVersion).toBe(1);
    expect(created.automation?.nextFireAt).toBeDefined();
    const parsed = JSON.parse(created.automation!.version!.definitionJson) as AutomationDefinition;
    expect(parsed.blocks[0]!.type).toBe("create_session");

    const saved = await automations.saveVersion({
      automationId: created.automation!.id,
      definitionJson: JSON.stringify(cronDefinition({ settings: { endSessionsOnFinish: true } })),
    });
    expect(saved.automation?.currentVersion).toBe(2);
    const versions = await automations.listVersions({ automationId: created.automation!.id });
    expect(versions.versions.map((v) => v.number)).toEqual([2, 1]);

    const list = await automations.listAutomations({});
    expect(list.automations[0]!.triggerSummary).toBe("Cron · 0 9 * * * America/Los_Angeles");
    expect(list.automations[0]!.runs7d).toEqual([]);
  });

  test("validation answers with block-addressed errors", async () => {
    const { automations } = clients(adminDeps());
    const err = await expectCode(
      automations.createAutomation({
        name: "x",
        description: "",
        enabled: true,
        definitionJson: JSON.stringify(
          cronDefinition({
            blocks: [{ id: "launch", type: "create_session", config: { profileId: "profile-1", promptTemplate: "${{ broken" } }],
          }),
        ),
        inputsJson: "{}",
      }),
      Code.InvalidArgument,
    );
    expect(err.message).toContain("launch.promptTemplate");

    const cron = await expectCode(
      automations.createAutomation({
        name: "x",
        description: "",
        enabled: true,
        definitionJson: JSON.stringify(cronDefinition({ trigger: { kind: "cron", schedule: "nope", timezone: "UTC" } })),
        inputsJson: "{}",
      }),
      Code.InvalidArgument,
    );
    expect(cron.message).toContain("trigger.schedule");

    await expectCode(
      automations.createAutomation({
        name: "x",
        description: "",
        enabled: true,
        definitionJson: JSON.stringify(
          cronDefinition({ blocks: [{ id: "launch", type: "create_session", config: { profileId: "ghost", promptTemplate: "hi" } }] }),
        ),
        inputsJson: "{}",
      }),
      Code.InvalidArgument,
    );
  });

  test("a model/effort override pins the effective harness; foreign ids are rejected", async () => {
    const { automations } = clients(adminDeps());
    const created = await automations.createAutomation({
      name: "x",
      description: "",
      enabled: false,
      definitionJson: JSON.stringify(
        cronDefinition({
          blocks: [{ id: "launch", type: "create_session", config: { profileId: "profile-1", promptTemplate: "hi", model: "sonnet" } }],
        }),
      ),
      inputsJson: "{}",
    });
    const d = JSON.parse(created.automation!.version!.definitionJson) as AutomationDefinition;
    expect(d.blocks[0]!.config["harness"]).toBe("claude");

    const bad = await expectCode(
      automations.createAutomation({
        name: "x",
        description: "",
        enabled: false,
        definitionJson: JSON.stringify(
          cronDefinition({
            blocks: [{ id: "launch", type: "create_session", config: { profileId: "profile-1", promptTemplate: "hi", harness: "codex", model: "opus" } }],
          }),
        ),
        inputsJson: "{}",
      }),
      Code.InvalidArgument,
    );
    expect(bad.message).toContain("launch.model");
  });

  test("builtin protection matrix: save/archive/settings denied; inputs/overrides/enable allowed", async () => {
    const deps = adminDeps();
    deps.fake.automations.set("builtin-1", builtinStored());
    const { automations } = clients(deps);
    await expectCode(
      automations.saveVersion({ automationId: "builtin-1", definitionJson: JSON.stringify(cronDefinition()) }),
      Code.PermissionDenied,
    );
    await expectCode(automations.archiveAutomation({ id: "builtin-1" }), Code.PermissionDenied);
    await expectCode(
      automations.updateAutomationMeta({ id: "builtin-1", settingsJson: JSON.stringify({ endSessionsOnFinish: true }) }),
      Code.PermissionDenied,
    );
    const renamed = await automations.updateAutomationMeta({ id: "builtin-1", name: "PR review (ours)" });
    expect(renamed.automation?.name).toBe("PR review (ours)");
    const inputs = await automations.setInputs({ automationId: "builtin-1", inputsJson: JSON.stringify({ mention: "@bot" }) });
    expect(JSON.parse(inputs.automation!.inputsJson)).toEqual({ mention: "@bot" });
    const disabled = await automations.setAutomationEnabled({ id: "builtin-1", enabled: false });
    expect(disabled.automation?.enabled).toBe(false);
    const fetched = await automations.getAutomation({ lookup: { case: "builtinKey", value: "pr_review" } });
    expect(fetched.automation?.id).toBe("builtin-1");
  });

  test("SetBlockOverrides enforces tunable fields and re-validates the merged config", async () => {
    const deps = adminDeps();
    deps.fake.automations.set("builtin-1", builtinStored());
    const { automations } = clients(deps);
    const ok = await automations.setBlockOverrides({
      automationId: "builtin-1",
      overridesJson: JSON.stringify({ launch: { promptTemplate: "Review it carefully" } }),
    });
    expect(JSON.parse(ok.automation!.blockOverridesJson)).toEqual({ launch: { promptTemplate: "Review it carefully" } });

    const notTunable = await expectCode(
      automations.setBlockOverrides({ automationId: "builtin-1", overridesJson: JSON.stringify({ launch: { profileId: "other" } }) }),
      Code.InvalidArgument,
    );
    expect(notTunable.message).toContain("not tunable");

    await expectCode(
      automations.setBlockOverrides({ automationId: "builtin-1", overridesJson: JSON.stringify({ ghost: { promptTemplate: "x" } }) }),
      Code.InvalidArgument,
    );
    // Tunable but the merged config fails the block's schema (promptTemplate must be a string).
    await expectCode(
      automations.setBlockOverrides({ automationId: "builtin-1", overridesJson: JSON.stringify({ launch: { promptTemplate: 42 } }) }),
      Code.InvalidArgument,
    );
  });

  test("SetInputs rejects undeclared keys", async () => {
    const deps = adminDeps();
    deps.fake.automations.set("builtin-1", builtinStored());
    const { automations } = clients(deps);
    const err = await expectCode(
      automations.setInputs({ automationId: "builtin-1", inputsJson: JSON.stringify({ nope: 1 }) }),
      Code.InvalidArgument,
    );
    expect(err.message).toContain("inputs.nope");
  });

  test("Duplicate folds overrides into an editable user copy", async () => {
    const deps = adminDeps();
    const b = builtinStored();
    b.meta.blockOverrides = { launch: { promptTemplate: "overridden" } };
    deps.fake.automations.set("builtin-1", b);
    const { automations } = clients(deps);
    const copy = await automations.duplicateAutomation({ automationId: "builtin-1" });
    expect(copy.automation?.kind).toBe("user");
    expect(copy.automation?.name).toBe("PR review (copy)");
    expect(copy.automation?.enabled).toBe(false);
    const d = JSON.parse(copy.automation!.version!.definitionJson) as AutomationDefinition;
    expect(d.blocks[0]!.config["promptTemplate"]).toBe("overridden");
    expect(JSON.parse(copy.automation!.blockOverridesJson)).toEqual({});
    // The copy is editable.
    const saved = await automations.saveVersion({ automationId: copy.automation!.id, definitionJson: JSON.stringify(cronDefinition()) });
    expect(saved.automation?.currentVersion).toBe(2);
  });

  test("TestRender renders every block against the scope and reports filter verdicts", async () => {
    const deps = adminDeps();
    const { automations } = clients(deps);
    const created = await automations.createAutomation({
      name: "Gate",
      description: "",
      enabled: false,
      definitionJson: JSON.stringify(
        cronDefinition({
          trigger: { kind: "manual" },
          blocks: [
            { id: "gate", type: "filter", config: { conditions: { mode: "all", conditions: [{ path: "inputs.mention", op: "equals", value: "@engrams" }] } } },
            { id: "launch", type: "create_session", config: { profileId: "profile-1", promptTemplate: "Hi ${{ inputs.mention }}" } },
          ],
        }),
      ),
      inputsJson: "{}",
    });
    const pass = await automations.testRender({ automationId: created.automation!.id, sample: { case: undefined } });
    expect(pass.errors).toEqual([]);
    expect(pass.blocks.map((b) => b.blockId)).toEqual(["gate", "launch"]);
    expect(pass.blocks[0]!.filterPass).toBe(true);
    expect(JSON.parse(pass.blocks[1]!.renderedJson)).toMatchObject({ promptTemplate: "Hi @engrams" });
    expect(JSON.parse(pass.blocks[1]!.scopeJson)).toMatchObject({ inputs: { mention: "@engrams" } });

    const fail = await automations.testRender({
      automationId: created.automation!.id,
      inputsJson: JSON.stringify({ mention: "@other" }),
      sample: { case: undefined },
    });
    expect(fail.blocks.map((b) => b.blockId)).toEqual(["gate"]);
    expect(fail.blocks[0]!.filterPass).toBe(false);

    const broken = await automations.testRender({
      automationId: created.automation!.id,
      draftDefinitionJson: JSON.stringify(
        cronDefinition({ trigger: { kind: "manual" }, blocks: [{ id: "launch", type: "create_session", config: { profileId: "profile-1", promptTemplate: "${{ inputs.missing }}" } }] }),
      ),
      sample: { case: undefined },
    });
    expect(broken.errors[0]).toMatchObject({ blockId: "launch", field: "promptTemplate" });
  });

  test("DryRun and RunNow admit through dispatch; DryRun flags the run", async () => {
    const starts: string[] = [];
    const deps = adminDeps({ workflowStarter: { async start(_i, id) { starts.push(id); } } });
    const { automations } = clients(deps);
    const created = await automations.createAutomation({
      name: "Manual",
      description: "",
      enabled: true,
      definitionJson: JSON.stringify(cronDefinition({ trigger: { kind: "manual" } })),
      inputsJson: "{}",
    });
    const dry = await automations.dryRun({ automationId: created.automation!.id, sample: { case: undefined } });
    expect(dry.runId).toBe(`autorun:${created.automation!.id}:dryrun:fixed-id`);
    expect(deps.fake.runs.get(dry.runId)?.dryRun).toBe(true);
    const live = await automations.runNow({ automationId: created.automation!.id });
    expect(deps.fake.runs.get(live.runId)?.dryRun).toBe(false);
    expect(deps.fake.runs.get(live.runId)?.trigger.source).toBe("manual");
    expect(starts).toEqual([dry.runId, live.runId]);
  });

  test("ListInputKeyOptions goes through the injected source", async () => {
    const { automations } = clients(
      adminDeps({ inputKeyOptions: { async list(noun) { return [{ key: `${noun}-1`, label: "One" }]; } } }),
    );
    const res = await automations.listInputKeyOptions({ noun: "repository" });
    expect(res.options.map((o) => ({ key: o.key, label: o.label }))).toEqual([
      { key: "repository-1", label: "One" },
    ]);
  });
});

describe("AutomationRunService", () => {
  function run(id: string, status: string, createdAt: Date, extra: Partial<AutomationRunRow> = {}): AutomationRunRow {
    return {
      id,
      automationId: "automation-1",
      version: 1,
      trigger: { source: "manual", receivedAt: createdAt.toISOString() },
      deliveryKey: `manual:${id}`,
      concurrencyKey: null,
      renderedPrompt: null,
      renderedTitle: null,
      taskId: null,
      sessionId: null,
      status,
      error: null,
      scheduledFor: null,
      leaseOwner: null,
      leaseExpiresAt: null,
      startedAt: null,
      endedAt: null,
      dryRun: false,
      createdAt,
      ...extra,
    };
  }
  const at = (m: number) => new Date(NOW.getTime() - m * 60_000);

  function seeded() {
    const deps = adminDeps();
    const stored = builtinStored("automation-1");
    stored.meta.kind = "user";
    stored.meta.builtinKey = null;
    deps.fake.automations.set("automation-1", stored);
    for (const r of [
      run("r1", "completed", at(1)),
      run("r2", "filtered", at(2)),
      run("r3", "filtered", at(3)),
      run("r4", "failed", at(4)),
      run("r5", "running", at(5)),
    ]) {
      deps.fake.runs.set(r.id, r);
    }
    return deps;
  }

  test("ListRuns collapses consecutive filtered runs into windows", async () => {
    const { runsApi } = clients(seeded());
    const res = await runsApi.listRuns({ automationId: "automation-1" });
    expect(res.runs.map((r) => r.id)).toEqual(["r1", "r4", "r5"]);
    expect(res.filtered).toEqual([
      expect.objectContaining({ count: 2, beforeRunId: "r4" }),
    ]);
    const all = await runsApi.listRuns({ automationId: "automation-1", includeFiltered: true });
    expect(all.runs).toHaveLength(5);
    expect(all.filtered).toEqual([]);
  });

  test("GetRun returns step runs with session ids", async () => {
    const deps = seeded();
    const { runsApi } = clients(deps);
    deps.fake.runs.set("r1", run("r1", "completed", at(1)));
    const fake2 = fakeStore({
      automations: [...deps.fake.automations.values()],
      runs: [...deps.fake.runs.values()],
      steps: [
        { runId: "r1", blockId: "launch", attempt: 0, status: "succeeded", inputs: { a: 1 }, outputs: { session_id: "sess-1" }, error: null, startedAt: NOW, endedAt: NOW },
        { runId: "r1", blockId: "poll[2].tick", attempt: 1, status: "failed", inputs: null, outputs: null, error: "boom", startedAt: NOW, endedAt: NOW },
      ],
    });
    const api = clients({ ...deps, store: fake2.store }).runsApi;
    const res = await api.getRun({ runId: "r1" });
    expect(res.run?.brief?.status).toBe("completed");
    expect(res.run?.steps).toHaveLength(2);
    expect(res.run?.steps[0]).toMatchObject({ blockId: "launch", sessionId: "sess-1", outputsJson: JSON.stringify({ session_id: "sess-1" }) });
    expect(res.run?.steps[1]).toMatchObject({ blockId: "poll[2].tick", attempt: 1, error: "boom" });
    expect(res.run?.sessionIds).toEqual(["sess-1"]);
    void runsApi;
  });

  test("StopRun sends a keyed stop to the run mailbox only while it is live", async () => {
    const sent: Array<{ runId: string; message: AutomationInbox; key: string }> = [];
    const deps = seeded();
    deps.sender = { async send(runId, message, key) { sent.push({ runId, message, key }); } };
    const { runsApi } = clients(deps);
    expect((await runsApi.stopRun({ runId: "r5", reason: "operator" })).sent).toBe(true);
    expect(sent).toEqual([{ runId: "r5", message: { kind: "stop", reason: "operator" }, key: "autorun:r5:stop:fixed-id" }]);
    expect((await runsApi.stopRun({ runId: "r1" })).sent).toBe(false);
    await expectCode(runsApi.stopRun({ runId: "nope" }), Code.NotFound);
  });

  test("RetryRun starts a fresh run with the original trigger payload", async () => {
    const starts: string[] = [];
    const deps = seeded();
    deps.workflowStarter = { async start(_i, id) { starts.push(id); } };
    deps.fake.runs.set("r4", run("r4", "failed", at(4), { trigger: { source: "integration", eventKey: "issues.opened", payload: { n: 1 }, receivedAt: at(4).toISOString() } }));
    const { runsApi } = clients(deps);
    const res = await runsApi.retryRun({ runId: "r4", fromStepId: "launch" });
    expect(res.runId).toBe("autorun:automation-1:retry:fixed-id");
    expect(deps.fake.runs.get(res.runId)?.trigger).toMatchObject({ source: "manual", eventKey: "issues.opened", payload: { n: 1 } });
    expect(starts).toEqual([res.runId]);
  });
});

describe("triggerSummary", () => {
  test("renders each trigger kind", () => {
    expect(triggerSummary(cronDefinition())).toBe("Cron · 0 9 * * * America/Los_Angeles");
    expect(
      triggerSummary(cronDefinition({ trigger: { kind: "integration", provider: "github", connectionId: "c", eventKeys: ["pull_request.opened", "pull_request.synchronize"], scope: { values: ["a/b", "c/d"] } } })),
    ).toBe("Github · pull_request.opened, pull_request.synchronize · 2 scoped");
    expect(triggerSummary(cronDefinition({ trigger: { kind: "manual" } }))).toBe("Manual");
    expect(triggerSummary(cronDefinition({ trigger: { kind: "webhook", registrationId: "team-gh", events: ["push"] } }))).toBe("Webhook · team-gh · push");
  });
});

describe("AutomationService.EvalCode", () => {
  test("runs the real sandbox and returns value, logs, duration", async () => {
    const { automations } = clients(adminDeps());
    const response = await automations.evalCode({
      source: `export default ({ event }) => { console.log("seen", event.n); return event.n * 2; };`,
      mode: "value",
      inputJson: JSON.stringify({ event: { n: 21 } }),
    });
    expect(response.valueJson).toBe("42");
    expect(response.logs).toEqual(["log: seen 21"]);
  });

  test("validates mode and input_json; rate limits per user", async () => {
    const { automations } = clients(adminDeps());
    await expectCode(automations.evalCode({ source: "export default () => 1;", mode: "maybe", inputJson: "" }), Code.InvalidArgument);
    await expectCode(automations.evalCode({ source: "export default () => 1;", mode: "value", inputJson: "[1]" }), Code.InvalidArgument);

    let calls = 0;
    const limited = clients(adminDeps({ evalCode: async () => { calls += 1; return { ok: true, value: calls, durationMs: 1, logs: [] }; } }));
    for (let i = 0; i < EVAL_CODE_LIMIT_PER_MINUTE; i += 1) {
      await limited.automations.evalCode({ source: "export default () => 1;", mode: "value", inputJson: "" });
    }
    await expectCode(limited.automations.evalCode({ source: "export default () => 1;", mode: "value", inputJson: "" }), Code.ResourceExhausted);
  });
});

describe("integration triggers + catalogs", () => {
  const integrationDefinition = (scope?: { values: string[] } | { fromInput: string }) =>
    cronDefinition({
      trigger: { kind: "integration", provider: "github", connectionId: "conn-github", eventKeys: ["pull_request.opened", "issue_comment.created"], ...(scope ? { scope } : {}) },
    });

  test("create round-trips an integration trigger and rejects undeclared keys", async () => {
    const { automations } = clients(adminDeps());
    const created = await automations.createAutomation({ name: "PR watcher", description: "", enabled: true, definitionJson: JSON.stringify(integrationDefinition({ fromInput: "repos" })), inputsJson: "{}" });
    const d = JSON.parse(created.automation!.version!.definitionJson) as AutomationDefinition;
    expect(d.trigger).toMatchObject({ kind: "integration", scope: { fromInput: "repos" } });
    const err = await expectCode(
      automations.createAutomation({
        name: "x", description: "", enabled: true,
        definitionJson: JSON.stringify(cronDefinition({ trigger: { kind: "integration", provider: "github", connectionId: "c", eventKeys: ["made.up"] } })),
        inputsJson: "{}",
      }),
      Code.InvalidArgument,
    );
    expect(err.message).toContain("trigger.eventKeys");
  });

  test("listEventCatalog serves labels, schemas, samples, observed and hidden flags", async () => {
    const latest = { issue: { title: "from-ledger" } };
    const { automations } = clients(
      adminDeps({
        connections: {
          getDefault: async () => ({
            id: "conn-github", alias: "default", provider: "github", displayName: "GitHub (default)",
            isDefault: true, config: {}, enabled: true, testedAt: null, createdAt: NOW, updatedAt: NOW,
          }),
        },
        integrationEvents: {
          getLatest: async (_c: string, eventKey: string) =>
            eventKey === "issues.opened"
              ? { id: "evt-1", provider: "github", connectionId: "conn-github", eventKey, deliveryId: "d1", payload: latest, scopeValue: null, receivedAt: NOW }
              : null,
          listObservedEventKeys: async () => ["issues.opened"],
          list: async () => [],
          getById: async () => null,
          listObservedScopeValues: async () => [],
        },
      }),
    );
    const response = await automations.listEventCatalog({ provider: "github" });
    const issues = response.events.find((e) => e.key === "issues.opened");
    expect(issues).toMatchObject({ observed: true });
    expect(JSON.parse(issues!.sampleJson)).toEqual(latest);
    const pr = response.events.find((e) => e.key === "pull_request.opened");
    expect(pr).toMatchObject({ observed: false, hidden: false });
    expect(JSON.parse(pr!.sampleJson)).toBeTruthy();
    expect(response.events.find((e) => e.key === "installation.created")?.hidden).toBe(true);
    expect(response.scope).toMatchObject({ key: "repositories" });
  });

  test("listActionCatalog is a member-safe projection", async () => {
    const { automations } = clients(adminDeps());
    const response = await automations.listActionCatalog({ provider: "github" });
    const post = response.actions.find((a) => a.id === "create_issue_comment");
    expect(post).toBeDefined();
    expect(Object.keys(post!)).not.toContain("execute");
    await expectCode(automations.listActionCatalog({ provider: "notaprovider" }), Code.InvalidArgument);
  });

  test("listEventSamples reads the ledger for integration triggers", async () => {
    const deps = adminDeps({
      integrationEvents: {
        getLatest: async () => null,
        listObservedEventKeys: async () => [],
        list: async () => [
          { id: "evt-1", provider: "github", connectionId: "conn-github", eventKey: "pull_request.opened", deliveryId: "d1", payload: { a: 1 }, scopeValue: null, receivedAt: NOW },
          { id: "evt-2", provider: "github", connectionId: "conn-github", eventKey: "push", deliveryId: "d2", payload: {}, scopeValue: null, receivedAt: NOW },
        ],
        getById: async () => null,
        listObservedScopeValues: async () => [],
      },
    });
    const { automations } = clients(deps);
    const created = await automations.createAutomation({ name: "PR watcher", description: "", enabled: true, definitionJson: JSON.stringify(integrationDefinition()), inputsJson: "{}" });
    const res = await automations.listEventSamples({ automationId: created.automation!.id });
    // Only the declared keys of this trigger.
    expect(res.samples.map((s) => s.id)).toEqual(["evt-1"]);
  });
});

describe("WebhookRegistrationService", () => {
  test("seals a generated secret under the deterministic ref and returns it once", async () => {
    const puts: Array<{ name: string; value: string }> = [];
    const { registrations } = clients(
      adminDeps({
        orgSecret: { async putSecret(req) { puts.push(req); }, async deleteSecret() { return { deleted: true }; } },
        randomSecret: () => "s3cr3t",
      }),
    );
    const res = await registrations.createWebhookRegistration({ id: "team-hook", name: "Team", verificationScheme: "generic_hmac_sha256" });
    expect(res.secret).toBe("s3cr3t");
    expect(puts).toEqual([{ name: "webhook.team-hook.secret", value: "s3cr3t" }]);
    expect(res.registration?.verificationScheme).toBe("generic_hmac_sha256");
  });

  test("provider signature schemes and ingress-backed provider hints are rejected", async () => {
    const { registrations } = clients(adminDeps({ orgSecret: { async putSecret() {}, async deleteSecret() { return { deleted: true }; } } }));
    await expectCode(registrations.createWebhookRegistration({ id: "gh", name: "x", verificationScheme: "github_hmac_sha256" }), Code.InvalidArgument);
    await expectCode(registrations.createWebhookRegistration({ id: "gh", name: "x", verificationScheme: "generic_hmac_sha256", providerHint: "github" }), Code.InvalidArgument);
  });

  test("refuses deletion while an automation is bound, then deletes after archive", async () => {
    const deletes: string[] = [];
    const deps = adminDeps({ orgSecret: { async putSecret() {}, async deleteSecret(req) { deletes.push(req.name); return { deleted: true }; } } });
    const { automations, registrations } = clients(deps);
    await registrations.createWebhookRegistration({ id: "team-hook", name: "Team", verificationScheme: "generic_hmac_sha256" });
    const created = await automations.createAutomation({
      name: "Hooked", description: "", enabled: true,
      definitionJson: JSON.stringify(cronDefinition({ trigger: { kind: "webhook", registrationId: "team-hook", events: ["push"] } })),
      inputsJson: "{}",
    });
    await expectCode(registrations.deleteWebhookRegistration({ id: "team-hook" }), Code.FailedPrecondition);
    await automations.archiveAutomation({ id: created.automation!.id });
    expect((await registrations.deleteWebhookRegistration({ id: "team-hook" })).deleted).toBe(true);
    expect(deletes).toEqual(["webhook.team-hook.secret"]);
  });
});
