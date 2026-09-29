import { describe, expect, test } from "bun:test";

import { EngineFlagError, repoPolicy, setReviewEngine } from "../engine-flag.ts";
import type { ApplyReviewEngineResult } from "../../db/review-engine.ts";
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

function harness(result: ApplyReviewEngineResult) {
  const applied: Array<{ repo: string; engine: ReviewEngine }> = [];
  const logged: string[] = [];
  const deps = {
    writer: {
      async apply(repo: string, engine: ReviewEngine) {
        applied.push({ repo, engine });
        return result;
      },
    },
    log: { info: (_b: Record<string, unknown>, m: string) => logged.push(m), warn: () => {} },
  };
  return { deps, applied, logged };
}

describe("repoPolicy", () => {
  test("maps the enrollment's trigger and autofix onto the built-in's repos map shape", () => {
    expect(repoPolicy({ triggerMode: "auto", autofix: "manual" })).toEqual({ mode: "auto", autofix: true });
    expect(repoPolicy({ triggerMode: "manual", autofix: "off" })).toEqual({ mode: "on_request", autofix: false });
    expect(repoPolicy({ triggerMode: "auto", autofix: "auto" })).toEqual({ mode: "auto", autofix: true });
  });
});

describe("setReviewEngine", () => {
  test("hands the flip to the one writer and reports what it did", async () => {
    const h = harness({
      kind: "applied",
      enrollment: enrollment({ engine: "automation" }),
      builtinUpdated: true,
      builtinEnabled: true,
    });
    const result = await setReviewEngine("engrams/engrams", "automation", h.deps);
    expect(h.applied).toEqual([{ repo: "engrams/engrams", engine: "automation" }]);
    expect(result).toMatchObject({ builtinUpdated: true, builtinEnabled: true });
    expect(result.enrollment.engine).toBe("automation");
    expect(h.logged).toHaveLength(1);
  });

  test("a no-op on the built-in is not logged as a reconcile", async () => {
    const h = harness({ kind: "applied", enrollment: enrollment(), builtinUpdated: false, builtinEnabled: true });
    await setReviewEngine("engrams/engrams", "legacy", h.deps);
    expect(h.logged).toEqual([]);
  });

  test("an unenrolled repo throws", async () => {
    const h = harness({ kind: "not_enrolled" });
    await expect(setReviewEngine("x/y", "automation", h.deps)).rejects.toBeInstanceOf(EngineFlagError);
  });

  test("an unseeded built-in throws", async () => {
    const h = harness({ kind: "not_seeded" });
    await expect(setReviewEngine("engrams/engrams", "automation", h.deps)).rejects.toBeInstanceOf(
      EngineFlagError,
    );
  });
});
