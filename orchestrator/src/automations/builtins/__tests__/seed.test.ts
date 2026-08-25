import { describe, expect, test } from "bun:test";

import type { AutomationRow, AutomationVersionRow } from "../../../db/automations.ts";
import type { AutomationDefinition } from "../../engine/definition.ts";
import type { BuiltinAutomation } from "../../engine/builtins.ts";
import { PR_REVIEW_BUILTIN, PR_REVIEW_BUILTIN_KEY } from "../pr-review.ts";
import { definitionContentHash, seedBuiltinAutomations, type BuiltinSeedStore } from "../seed.ts";

const NOW = new Date("2026-08-21T00:00:00Z");

function harness(
  options: {
    enrollments?: Array<{ repo: string; triggerMode: string; autofix: string }>;
    existing?: { definition: AutomationDefinition; inputs: Record<string, unknown>; overrides: Record<string, Record<string, unknown>> };
    raceOnCreate?: boolean;
  } = {},
) {
  const rows = new Map<string, AutomationRow>();
  const versions = new Map<string, AutomationVersionRow[]>();
  const calls: string[] = [];

  const put = (definition: AutomationDefinition, inputs: Record<string, unknown>, overrides: Record<string, Record<string, unknown>>, currentVersion: number) => {
    const id = "auto-pr-review";
    const version: AutomationVersionRow = {
      automationId: id,
      version: currentVersion,
      trigger: definition.trigger,
      blocks: definition.blocks,
      entrypoints: definition.entrypoints ?? [],
      inputsSchema: definition.inputsSchema,
      settings: definition.settings,
      createdByUserId: null,
      createdAt: NOW,
    };
    rows.set(id, {
      version,
      id,
      name: "PR review",
      description: "",
      enabled: false,
      kind: "builtin",
      builtinKey: PR_REVIEW_BUILTIN_KEY,
      currentVersion,
      inputs,
      blockOverrides: overrides,
      endSessionsOnFinish: true,
      nextFireAt: null,
      lastFiredAt: null,
      createdByUserId: null,
      createdAt: NOW,
      updatedAt: NOW,
      archivedAt: null,
    });
    versions.set(id, [...(versions.get(id) ?? []), version]);
  };
  if (options.existing) put(options.existing.definition, options.existing.inputs, options.existing.overrides, 1);

  const store: BuiltinSeedStore = {
    async getByBuiltinKey(key) {
      return [...rows.values()].find((r) => r.builtinKey === key) ?? null;
    },
    async create(input) {
      calls.push("create");
      if (options.raceOnCreate) {
        const err = Object.assign(new Error("duplicate key"), { code: "23505" });
        throw err;
      }
      put(input.definition, input.inputs ?? {}, {}, 1);
      return rows.get("auto-pr-review")!;
    },
    async saveVersion(id, definition) {
      calls.push("saveVersion");
      const row = rows.get(id)!;
      const next = row.currentVersion + 1;
      const version: AutomationVersionRow = {
        automationId: id,
        version: next,
        trigger: definition.trigger,
        blocks: definition.blocks,
        entrypoints: definition.entrypoints ?? [],
        inputsSchema: definition.inputsSchema,
        settings: definition.settings,
        createdByUserId: null,
        createdAt: NOW,
      };
      versions.get(id)!.push(version);
      rows.set(id, { ...row, currentVersion: next, version });
      return rows.get(id)!;
    },
    async setInputs(id, inputs) {
      calls.push("setInputs");
      rows.set(id, { ...rows.get(id)!, inputs });
      return rows.get(id)!;
    },
    async setBlockOverrides(id, overrides) {
      calls.push("setBlockOverrides");
      rows.set(id, { ...rows.get(id)!, blockOverrides: overrides });
      return rows.get(id)!;
    },
  };

  const deps = {
    store,
    connections: { async ensureDefault() { return { id: "conn-github" }; } },
    enrollments: { async list() { return options.enrollments ?? []; } },
    log: { info() {}, warn() {} },
  };
  return { store, rows, versions, calls, deps };
}

describe("seedBuiltinAutomations", () => {
  test("fresh: creates the built-in disabled, resolves the default connection, lifts enrollments into repos", async () => {
    const h = harness({
      enrollments: [
        { repo: "acme/app", triggerMode: "auto", autofix: "off" },
        { repo: "acme/infra", triggerMode: "manual", autofix: "auto" },
      ],
    });
    const result = await seedBuiltinAutomations({ ...h.deps, builtins: [PR_REVIEW_BUILTIN] });

    expect(result.created).toEqual([PR_REVIEW_BUILTIN_KEY]);
    const row = h.rows.get("auto-pr-review")!;
    expect(row.enabled).toBe(false);
    expect(row.kind).toBe("builtin");
    expect(row.inputs["repos"]).toEqual({
      "acme/app": { mode: "auto", autofix: false },
      "acme/infra": { mode: "on_request", autofix: true },
    });
    expect(row.inputs["mention"]).toBe("@engrams");
    const v1 = h.versions.get("auto-pr-review")![0]!;
    expect(v1.trigger).toMatchObject({ kind: "integration", provider: "github", connectionId: "conn-github" });
  });

  test("a legacy enrollment row the schema rejects drops just that row and never blocks the seed (phase 4.3b)", async () => {
    // review_enrollment.repo is a free text PK; a malformed legacy value
    // must not stop the built-in from seeding for every healthy repo.
    const warnings: Array<Record<string, unknown>> = [];
    const h = harness({
      enrollments: [
        { repo: "acme/app", triggerMode: "auto", autofix: "off" },
        { repo: "not a repo", triggerMode: "auto", autofix: "off" },
      ],
    });
    const result = await seedBuiltinAutomations({
      ...h.deps,
      log: { info() {}, warn: (b: Record<string, unknown>) => void warnings.push(b) },
      builtins: [PR_REVIEW_BUILTIN],
    });
    expect(result.created).toEqual([PR_REVIEW_BUILTIN_KEY]);
    const row = h.rows.get("auto-pr-review")!;
    expect(row.inputs["repos"]).toEqual({ "acme/app": { mode: "auto", autofix: false } });
    expect(warnings).toEqual([
      expect.objectContaining({ key: "repos", path: "not a repo", builtinKey: PR_REVIEW_BUILTIN_KEY }),
    ]);
    // The shipped schema's own defaults are valid: no other key fell back.
    expect(warnings).toHaveLength(1);
  });

  test("re-run with an unchanged shipped definition is a no-op", async () => {
    const h = harness();
    await seedBuiltinAutomations({ ...h.deps, builtins: [PR_REVIEW_BUILTIN] });
    const again = await seedBuiltinAutomations({ ...h.deps, builtins: [PR_REVIEW_BUILTIN] });
    expect(again).toEqual({ created: [], bumped: [], unchanged: [PR_REVIEW_BUILTIN_KEY] });
    expect(h.calls.filter((c) => c !== "create")).toEqual([]);
  });

  test("a replica losing the create race is tolerated", async () => {
    const h = harness({ raceOnCreate: true });
    const result = await seedBuiltinAutomations({ ...h.deps, builtins: [PR_REVIEW_BUILTIN] });
    expect(result.created).toEqual([]);
  });

  test("a changed shipped definition bumps the version, keeps org inputs, and three-way merges overrides", async () => {
    // Stored: the current shipped definition with org edits layered on.
    const stored = structuredClone(PR_REVIEW_BUILTIN.definition);
    if (stored.trigger.kind === "integration") stored.trigger.connectionId = "conn-github";
    // A tunable field on a finalize-hook block: its override must survive a
    // bump like a graph block's (the seeder walks the same target set).
    const markHookTunable = (d: typeof stored) => {
      const hook = d.settings.onFinalize!.find((h) => h.block.id === "report_failure")!;
      hook.block.tunable = ["reason"];
    };
    markHookTunable(stored);
    const h = harness({
      existing: {
        definition: stored,
        inputs: { repos: { "acme/app": { mode: "auto", autofix: false } }, mention: "@reviewbot", profile: "pr_reviewer", categories: [], instructions: "" },
        overrides: {
          // A real edit (differs from old default) — kept.
          find: { deadlineSeconds: 900 },
          // Equal to the OLD shipped default — not an edit; follows the new default.
          verify: { deadlineSeconds: 7200 },
          // A real edit on a finalize-hook block — kept.
          report_failure: { reason: "custom: ${{ run.error }}" },
        },
      },
    });

    // The new shipped definition: verify's deadline default changes and a
    // new input key appears.
    const shipped: BuiltinAutomation = {
      ...PR_REVIEW_BUILTIN,
      definition: (() => {
        const d = structuredClone(PR_REVIEW_BUILTIN.definition);
        markHookTunable(d);
        const then = d.blocks.find((b) => b.id === "has_candidates")!.then!;
        const verify = then.find((b) => b.id === "verify")!;
        verify.config = { ...verify.config, deadlineSeconds: 3600 };
        d.inputsSchema.push({ key: "new_knob", label: "New knob", type: "string", default: "x" });
        return d;
      })(),
      async defaultInputs() {
        return { ...(await PR_REVIEW_BUILTIN.defaultInputs()), new_knob: "x" };
      },
    };

    const result = await seedBuiltinAutomations({ ...h.deps, builtins: [shipped] });

    expect(result.bumped).toEqual([{ key: PR_REVIEW_BUILTIN_KEY, from: 1, to: 2 }]);
    const row = h.rows.get("auto-pr-review")!;
    expect(row.currentVersion).toBe(2);
    // Org inputs preserved; only the NEW key got its default.
    expect(row.inputs["mention"]).toBe("@reviewbot");
    expect(row.inputs["repos"]).toEqual({ "acme/app": { mode: "auto", autofix: false } });
    expect(row.inputs["new_knob"]).toBe("x");
    // Overrides: the real edit survives; the un-edited one follows the new default.
    expect(row.blockOverrides).toEqual({
      find: { deadlineSeconds: 900 },
      report_failure: { reason: "custom: ${{ run.error }}" },
    });
  });

  test("content hash ignores key order and is stable", () => {
    const a = definitionContentHash(PR_REVIEW_BUILTIN.definition);
    const reordered = JSON.parse(JSON.stringify(PR_REVIEW_BUILTIN.definition)) as AutomationDefinition;
    const b = definitionContentHash({ ...reordered, settings: { ...reordered.settings } });
    expect(a).toBe(b);
  });
});
