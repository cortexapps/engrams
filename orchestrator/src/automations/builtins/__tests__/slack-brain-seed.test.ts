/** The Slack thread brain is seeded DISABLED with an empty channel map and
 * the default Slack connection (ADR 0119 phase 4.6). Nothing fires until an
 * operator flags a channel and enables the row.
 */

import { describe, expect, test } from "bun:test";

import type { AutomationRow, AutomationVersionRow } from "../../../db/automations.ts";
import { SLACK_BRAIN_BUILTIN, SLACK_BRAIN_BUILTIN_KEY } from "../slack-brain.ts";
import { seedBuiltinAutomations, type BuiltinSeedStore } from "../seed.ts";

const NOW = new Date("2026-08-21T00:00:00Z");

function harness() {
  const rows = new Map<string, AutomationRow>();
  const providers: string[] = [];
  const store: BuiltinSeedStore = {
    async getByBuiltinKey(key) {
      return [...rows.values()].find((r) => r.builtinKey === key) ?? null;
    },
    async create(input) {
      const id = `auto-${input.builtinKey}`;
      const version: AutomationVersionRow = {
        automationId: id,
        version: 1,
        trigger: input.definition.trigger,
        blocks: input.definition.blocks,
        entrypoints: input.definition.entrypoints ?? [],
        inputsSchema: input.definition.inputsSchema,
        settings: input.definition.settings,
        createdByUserId: null,
        createdAt: NOW,
      };
      rows.set(id, {
        version,
        id,
        name: input.name,
        description: input.description ?? "",
        enabled: input.enabled,
        kind: input.kind ?? "builtin",
        builtinKey: input.builtinKey ?? null,
        currentVersion: 1,
        inputs: input.inputs ?? {},
        blockOverrides: {},
        endSessionsOnFinish: input.definition.settings.endSessionsOnFinish ?? true,
        nextFireAt: null,
        lastFiredAt: null,
        draftSessionId: null,
        createdByUserId: null,
        createdAt: NOW,
        updatedAt: NOW,
        archivedAt: null,
      });
      return rows.get(id)!;
    },
    async saveVersion(id) {
      return rows.get(id)!;
    },
    async setInputs(id, inputs) {
      rows.set(id, { ...rows.get(id)!, inputs });
      return rows.get(id)!;
    },
    async setBlockOverrides(id) {
      return rows.get(id)!;
    },
    async updateMeta(id, patch) {
      const row = rows.get(id)!;
      rows.set(id, { ...row, name: patch.name ?? row.name, description: patch.description ?? row.description });
      return rows.get(id)!;
    },
  };
  const deps = {
    store,
    connections: {
      async ensureDefault(provider: string) {
        providers.push(provider);
        return { id: `conn-${provider}` };
      },
    },
    log: { info() {}, warn() {} },
  };
  return { rows, providers, deps };
}

describe("seedBuiltinAutomations — slack_brain", () => {
  test("fresh: disabled, empty channel map, default Slack connection", async () => {
    const h = harness();
    const result = await seedBuiltinAutomations({ ...h.deps, builtins: [SLACK_BRAIN_BUILTIN] });
    expect(result.created).toEqual([SLACK_BRAIN_BUILTIN_KEY]);
    const row = h.rows.get(`auto-${SLACK_BRAIN_BUILTIN_KEY}`)!;
    expect(row.enabled).toBe(false);
    expect(row.kind).toBe("builtin");
    expect(row.inputs).toEqual({ channels: {}, default_profile: "", idle_timeout: 3600, max_turns: 50 });
    expect(row.version.trigger).toMatchObject({
      kind: "integration",
      provider: "slack",
      connectionId: "conn-slack",
      scope: { fromInput: "channels" },
    });
    expect(h.providers).toEqual(["slack"]);
    // A thread's session outlives its run.
    expect(row.version.settings.endSessionsOnFinish).toBe(false);
  });

  test("a row still named by a previous product name is renamed; a custom name is kept", async () => {
    const h = harness();
    await seedBuiltinAutomations({ ...h.deps, builtins: [SLACK_BRAIN_BUILTIN] });
    const id = `auto-${SLACK_BRAIN_BUILTIN_KEY}`;
    h.rows.set(id, { ...h.rows.get(id)!, name: "Slack thread brain" });
    const renamed = await seedBuiltinAutomations({ ...h.deps, builtins: [SLACK_BRAIN_BUILTIN] });
    expect(renamed.renamed).toEqual([SLACK_BRAIN_BUILTIN_KEY]);
    expect(h.rows.get(id)!.name).toBe("Slack threads");

    h.rows.set(id, { ...h.rows.get(id)!, name: "Ops helper" });
    const kept = await seedBuiltinAutomations({ ...h.deps, builtins: [SLACK_BRAIN_BUILTIN] });
    expect(kept.renamed).toBeUndefined();
    expect(h.rows.get(id)!.name).toBe("Ops helper");
  });

  test("re-run is a no-op", async () => {
    const h = harness();
    await seedBuiltinAutomations({ ...h.deps, builtins: [SLACK_BRAIN_BUILTIN] });
    const again = await seedBuiltinAutomations({ ...h.deps, builtins: [SLACK_BRAIN_BUILTIN] });
    expect(again).toEqual({ created: [], bumped: [], unchanged: [SLACK_BRAIN_BUILTIN_KEY] });
  });
});
