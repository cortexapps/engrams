import { describe, expect, test } from "bun:test";

import type {
  AutomationMetaRow,
  AutomationVersionRow,
  WebhookRegistrationRow,
} from "../../db/automations.ts";
import type { IntegrationConnectionRow } from "../../db/integration-connections.ts";
import type { AutomationTrigger } from "../../db/schema.ts";
import { registerEngineBlocks } from "../engine/blocks/index.ts";
import { evaluateFilter } from "../engine/conditions.ts";
import type { AutomationDefinition, BlockDef } from "../engine/definition.ts";
import {
  filterGroupFromLegacyWebhookFilter,
  LEGACY_FILTER_BLOCK_ID,
  RETIRED_SCHEME_DISABLED_REASON,
  retireProviderWebhookSchemes,
  type RetireWebhookDeps,
  type RetireWebhookStore,
} from "../retire-provider-webhooks.ts";
import { matchesWebhookFilter } from "../webhook.ts";

registerEngineBlocks();

const NOW = new Date("2026-08-21T12:00:00Z");
const CREATE_SESSION: BlockDef = {
  id: "create_session",
  type: "create_session",
  config: { profileId: "profile-1", promptTemplate: "Triage ${{ event.raw.issue.title }}" },
};

interface StoredAutomation {
  meta: AutomationMetaRow;
  versions: Map<number, AutomationVersionRow>;
}

function meta(id: string, kind: "user" | "builtin" = "user"): AutomationMetaRow {
  return {
    id,
    name: id,
    description: "",
    enabled: true,
    kind,
    builtinKey: null,
    currentVersion: 1,
    inputs: {},
    endSessionsOnFinish: false,
    createdByUserId: "admin",
    nextFireAt: null,
    lastFiredAt: null,
    createdAt: NOW,
    updatedAt: NOW,
    archivedAt: null,
  };
}

function version(automationId: string, trigger: AutomationTrigger): AutomationVersionRow {
  return {
    automationId,
    version: 1,
    trigger,
    blocks: [CREATE_SESSION],
    inputsSchema: [],
    settings: { endSessionsOnFinish: false },
    createdByUserId: "admin",
    createdAt: NOW,
  };
}

function registration(
  id: string,
  scheme: string,
  providerHint: string | null,
  disabledReason: string | null = null,
): WebhookRegistrationRow {
  return {
    id,
    name: id,
    // The retired schemes can no longer be TYPED on a fresh row; stored rows
    // from before the retirement still carry them.
    verification: { scheme: scheme as "generic_hmac_sha256", secretRef: `webhook.${id}.secret` },
    providerHint,
    disabledReason,
    createdByUserId: "admin",
    createdAt: NOW,
    updatedAt: NOW,
  };
}

function connection(provider: string): IntegrationConnectionRow {
  return {
    id: `conn-${provider}`,
    alias: "default",
    provider,
    displayName: `${provider} (default)`,
    isDefault: true,
    config: {},
    enabled: true,
    testedAt: null,
    createdAt: NOW,
    updatedAt: NOW,
  };
}

function harness(seed: {
  automations: Array<{ meta: AutomationMetaRow; trigger: AutomationTrigger }>;
  registrations?: WebhookRegistrationRow[];
  connections?: string[];
}) {
  const automations = new Map<string, StoredAutomation>(
    seed.automations.map(({ meta: m, trigger }) => [
      m.id,
      { meta: m, versions: new Map([[1, version(m.id, trigger)]]) },
    ]),
  );
  const registrations = new Map((seed.registrations ?? []).map((r) => [r.id, r]));
  const connections = new Set(seed.connections ?? ["github", "slack"]);
  const logs: string[] = [];

  const currentTrigger = (row: StoredAutomation): AutomationTrigger =>
    row.versions.get(row.meta.currentVersion)!.trigger;

  const store: RetireWebhookStore = {
    async listBoundToWebhookRegistration(registrationId) {
      return [...automations.values()]
        .filter((row) => {
          const trigger = currentTrigger(row);
          return trigger.kind === "webhook" && trigger.registrationId === registrationId;
        })
        .map((row) => row.meta);
    },
    async getVersion(automationId, v) {
      return automations.get(automationId)?.versions.get(v) ?? null;
    },
    async replaceCurrentDefinition(automationId, definition: AutomationDefinition) {
      const row = automations.get(automationId);
      if (!row) return null;
      const next = row.meta.currentVersion + 1;
      row.versions.set(next, {
        automationId,
        version: next,
        trigger: definition.trigger,
        blocks: definition.blocks,
        inputsSchema: definition.inputsSchema,
        settings: definition.settings,
        createdByUserId: null,
        createdAt: NOW,
      });
      row.meta = { ...row.meta, currentVersion: next };
      return row.meta;
    },
    async listRegistrations() {
      return [...registrations.values()];
    },
    async setRegistrationDisabledReason(id, reason) {
      const row = registrations.get(id);
      if (!row) return false;
      registrations.set(id, { ...row, disabledReason: reason });
      return true;
    },
  };

  const deps: RetireWebhookDeps = {
    store,
    connections: {
      async ensureDefault(provider) {
        connections.add(provider);
        return connection(provider);
      },
    },
    log: {
      info: (_b, m) => void logs.push(`info:${m}`),
      warn: (_b, m) => void logs.push(`warn:${m}`),
    },
  };

  return { deps, automations, registrations, logs, currentTrigger };
}

describe("filterGroupFromLegacyWebhookFilter", () => {
  test("preserves matchesWebhookFilter semantics over event.raw", () => {
    const cases: Array<{ filter: Record<string, unknown>; payload: Record<string, unknown> }> = [
      { filter: { action: "opened" }, payload: { action: "opened" } },
      { filter: { action: "opened" }, payload: { action: "closed" } },
      { filter: { "issue.number": 7 }, payload: { issue: { number: 7 } } },
      { filter: { "issue.number": 7 }, payload: { issue: { number: "7" } } },
      { filter: { "pull_request.draft": false }, payload: { pull_request: { draft: false } } },
      { filter: { "pull_request.draft": false }, payload: { pull_request: {} } },
      { filter: { "labels": ["bug", "p1"] }, payload: { labels: ["bug", "p1"] } },
      { filter: { "labels": ["bug", "p1"] }, payload: { labels: ["p1", "bug"] } },
      { filter: { "repo": { full_name: "a/b" } }, payload: { repo: { full_name: "a/b" } } },
      { filter: { "repo": { full_name: "a/b" } }, payload: { repo: { full_name: "a/b", id: 1 } } },
      { filter: { action: "opened", "sender.type": "User" }, payload: { action: "opened", sender: { type: "User" } } },
      { filter: { action: "opened", "sender.type": "User" }, payload: { action: "opened", sender: { type: "Bot" } } },
      { filter: { missing: null }, payload: {} },
      { filter: {}, payload: { anything: true } },
    ];
    for (const { filter, payload } of cases) {
      const legacy = matchesWebhookFilter(payload, filter);
      const translated = evaluateFilter(filterGroupFromLegacyWebhookFilter(filter), {
        event: { raw: payload },
      });
      expect(translated, JSON.stringify({ filter, payload })).toBe(legacy);
    }
  });
});

describe("retireProviderWebhookSchemes", () => {
  test("rewrites github-app-bound automations onto the GitHub integration trigger with a filter block", async () => {
    const h = harness({
      automations: [
        {
          meta: meta("auto-gh"),
          trigger: {
            kind: "webhook",
            registrationId: "github-app",
            events: ["issues.opened", "pull_request.opened"],
            filter: { "sender.type": "User", "issue.number": 7 },
          },
        },
      ],
    });

    const result = await retireProviderWebhookSchemes(h.deps);

    expect(result.rewritten).toEqual(["auto-gh"]);
    const row = h.automations.get("auto-gh")!;
    expect(row.meta.currentVersion).toBe(2);
    const v2 = row.versions.get(2)!;
    expect(v2.trigger).toEqual({
      kind: "integration",
      provider: "github",
      connectionId: "conn-github",
      eventKeys: ["issues.opened", "pull_request.opened"],
    });
    expect(v2.blocks.map((b) => b.id)).toEqual([LEGACY_FILTER_BLOCK_ID, "create_session"]);
    expect(v2.blocks[0]!.config).toEqual({
      conditions: {
        mode: "all",
        conditions: [
          { path: "event.raw.sender.type", op: "equals", value: "User" },
          { path: "event.raw.issue.number", op: "equals", value: 7 },
        ],
      },
    });

    // Idempotent: a second boot finds nothing bound and writes nothing.
    const again = await retireProviderWebhookSchemes(h.deps);
    expect(again).toEqual({ rewritten: [], disabledRegistrations: [] });
    expect(h.automations.get("auto-gh")!.meta.currentVersion).toBe(2);
  });

  test("a filter-less automation gets no filter block", async () => {
    const h = harness({
      automations: [
        {
          meta: meta("auto-plain"),
          trigger: { kind: "webhook", registrationId: "github-app", events: ["push"] },
        },
      ],
    });
    await retireProviderWebhookSchemes(h.deps);
    const v2 = h.automations.get("auto-plain")!.versions.get(2)!;
    expect(v2.blocks.map((b) => b.id)).toEqual(["create_session"]);
  });

  test("a custom provider-scheme registration is NEVER re-pointed: disabled with a reason, automations untouched, nothing deleted", async () => {
    // The App's default connection exists and would even verify — coverage is
    // still unknowable (the user wired this hook to repos of their choosing),
    // so the only honest move is a visible stop.
    const h = harness({
      automations: [
        {
          meta: meta("auto-custom"),
          trigger: { kind: "webhook", registrationId: "team-gh", events: ["issues.opened"] },
        },
      ],
      registrations: [registration("team-gh", "github_hmac_sha256", "github")],
      connections: ["github"],
    });

    const result = await retireProviderWebhookSchemes(h.deps);

    expect(result.rewritten).toEqual([]);
    expect(result.disabledRegistrations).toEqual(["team-gh"]);
    expect(h.registrations.get("team-gh")!.disabledReason).toBe(RETIRED_SCHEME_DISABLED_REASON);
    // The automation keeps its webhook trigger at version 1 — no new version,
    // no integration trigger, no firehose.
    expect(h.automations.get("auto-custom")!.meta.currentVersion).toBe(1);
    expect(h.currentTrigger(h.automations.get("auto-custom")!)).toMatchObject({
      kind: "webhook",
      registrationId: "team-gh",
    });
    // The operator log names the automations that now need re-creating.
    expect(h.logs.some((l) => l.startsWith("warn:") && l.includes("re-created"))).toBe(true);
  });

  test("disabling is idempotent and leaves bound automations at their current version", async () => {
    const h = harness({
      automations: [
        {
          meta: meta("auto-slack"),
          trigger: { kind: "webhook", registrationId: "team-slack", events: ["app_mention"] },
        },
      ],
      registrations: [registration("team-slack", "slack_v0", "slack")],
    });

    const result = await retireProviderWebhookSchemes(h.deps);

    expect(result.disabledRegistrations).toEqual(["team-slack"]);
    expect(h.registrations.get("team-slack")!.disabledReason).toBe(RETIRED_SCHEME_DISABLED_REASON);
    expect(h.automations.get("auto-slack")!.meta.currentVersion).toBe(1);

    // Idempotent: already disabled → no second write, no log noise.
    const again = await retireProviderWebhookSchemes(h.deps);
    expect(again.disabledRegistrations).toEqual([]);
  });

  test("a provider-scheme registration without a provider hint is disabled", async () => {
    const h = harness({
      automations: [],
      registrations: [registration("mystery", "github_hmac_sha256", null)],
    });
    const result = await retireProviderWebhookSchemes(h.deps);
    expect(result.disabledRegistrations).toEqual(["mystery"]);
  });

  test("generic registrations are untouched", async () => {
    const h = harness({
      automations: [],
      registrations: [registration("generic-1", "generic_hmac_sha256", null)],
    });
    const result = await retireProviderWebhookSchemes(h.deps);
    expect(result).toEqual({ rewritten: [], disabledRegistrations: [] });
    expect(h.registrations.get("generic-1")!.disabledReason).toBeNull();
  });
});
