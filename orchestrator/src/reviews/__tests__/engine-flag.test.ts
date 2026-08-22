import { describe, expect, test } from "bun:test";

import { EngineFlagError, setReviewEngine, type EngineFlagStore } from "../engine-flag.ts";
import type { AutomationRow } from "../../db/automations.ts";
import type { EnrollmentRow, ReviewEngine } from "../../db/enrollments.ts";

const NOW = new Date("2026-08-22T00:00:00Z");

function enrollment(overrides: Partial<EnrollmentRow> = {}): EnrollmentRow {
  return {
    repo: "engrams/engrams",
    triggerMode: "auto",
    autofix: "manual",
    profileId: null,
    engine: "legacy",
    createdAt: NOW,
    updatedAt: NOW,
    ...overrides,
  };
}

function builtinRow(enabled: boolean, repos: Record<string, unknown> = {}): AutomationRow {
  return {
    id: "b1",
    name: "PR review",
    description: "",
    kind: "builtin",
    builtinKey: "pr_review",
    enabled,
    currentVersion: 2,
    inputs: { repos, mention: "@engrams" },
    blockOverrides: {},
    concurrency: null,
    endSessionsOnFinish: true,
    nextFireAt: null,
    createdByUserId: null,
    createdAt: NOW,
    updatedAt: NOW,
    archivedAt: null,
    version: {
      automationId: "b1",
      version: 2,
      trigger: { kind: "manual" },
      blocks: [],
      inputsSchema: [],
      settings: { endSessionsOnFinish: true },
      createdByUserId: null,
      createdAt: NOW,
    },
  } as unknown as AutomationRow;
}

function harness(builtin: AutomationRow | null, enroll: EnrollmentRow) {
  const inputsWrites: Array<Record<string, unknown>> = [];
  const enabledWrites: boolean[] = [];
  const engineWrites: ReviewEngine[] = [];
  let current = builtin;
  const store: EngineFlagStore = {
    async getByBuiltinKey() {
      return current;
    },
    async setInputs(_id, inputs) {
      inputsWrites.push(inputs);
      if (current) current = { ...current, inputs } as AutomationRow;
      return current;
    },
    async setEnabled(_id, enabled) {
      enabledWrites.push(enabled);
      if (current) current = { ...current, enabled } as AutomationRow;
      return current;
    },
  };
  const deps = {
    setEnrollmentEngine: async (repo: string, engine: ReviewEngine) => {
      engineWrites.push(engine);
      return { ...enroll, repo, engine };
    },
    builtins: store,
    log: { info: () => {}, warn: () => {} },
  };
  return { deps, inputsWrites, enabledWrites, engineWrites };
}

describe("setReviewEngine", () => {
  test("→ automation: adds the repo to repos (mode/autofix from enrollment) and enables the built-in", async () => {
    const h = harness(builtinRow(false), enrollment({ triggerMode: "auto", autofix: "manual" }));
    const result = await setReviewEngine("engrams/engrams", "automation", h.deps);
    expect(h.engineWrites).toEqual(["automation"]);
    expect(h.inputsWrites.at(-1)!["repos"]).toEqual({
      "engrams/engrams": { mode: "auto", autofix: true },
    });
    expect(h.enabledWrites).toEqual([true]);
    expect(result.builtinEnabled).toBe(true);
  });

  test("→ automation on an on_request/off repo derives the right policy and leaves an enabled built-in enabled", async () => {
    const h = harness(
      builtinRow(true, { "other/repo": { mode: "auto", autofix: false } }),
      enrollment({ triggerMode: "manual", autofix: "off" }),
    );
    await setReviewEngine("engrams/engrams", "automation", h.deps);
    expect(h.inputsWrites.at(-1)!["repos"]).toEqual({
      "other/repo": { mode: "auto", autofix: false },
      "engrams/engrams": { mode: "on_request", autofix: false },
    });
    expect(h.enabledWrites).toEqual([]); // already enabled
  });

  test("→ legacy: removes the repo from repos, leaves the built-in enabled", async () => {
    const h = harness(
      builtinRow(true, {
        "engrams/engrams": { mode: "auto", autofix: true },
        "other/repo": { mode: "auto", autofix: false },
      }),
      enrollment(),
    );
    await setReviewEngine("engrams/engrams", "legacy", h.deps);
    expect(h.inputsWrites.at(-1)!["repos"]).toEqual({
      "other/repo": { mode: "auto", autofix: false },
    });
    expect(h.enabledWrites).toEqual([]);
  });

  test("→ legacy when the repo was never in the map is a no-op on the built-in", async () => {
    const h = harness(builtinRow(true, { "other/repo": { mode: "auto", autofix: false } }), enrollment());
    await setReviewEngine("engrams/engrams", "legacy", h.deps);
    expect(h.inputsWrites).toEqual([]);
  });

  test("an unenrolled repo throws", async () => {
    const deps = {
      setEnrollmentEngine: async () => null,
      builtins: harness(builtinRow(false), enrollment()).deps.builtins,
      log: { info: () => {}, warn: () => {} },
    };
    await expect(setReviewEngine("x/y", "automation", deps)).rejects.toBeInstanceOf(EngineFlagError);
  });

  test("an unseeded built-in throws", async () => {
    const h = harness(null, enrollment());
    await expect(setReviewEngine("engrams/engrams", "automation", h.deps)).rejects.toBeInstanceOf(
      EngineFlagError,
    );
  });
});
