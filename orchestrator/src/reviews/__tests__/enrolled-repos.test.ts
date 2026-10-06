import { describe, expect, test } from "bun:test";

import type { AutomationRow } from "../../db/automations.ts";
import { makeEnrolledRepos, policyOf } from "../enrolled-repos.ts";

function builtin(inputs: Record<string, unknown>, archived = false): AutomationRow {
  return {
    inputs,
    archivedAt: archived ? new Date("2026-10-01T00:00:00Z") : null,
    builtinKey: "pr_review",
  } as unknown as AutomationRow;
}

describe("makeEnrolledRepos", () => {
  test("reads the built-in's repos map, case-insensitively, and normalizes the policy", async () => {
    const repos = makeEnrolledRepos({
      async getByBuiltinKey() {
        return builtin({ repos: { "Acme/Widgets": { mode: "auto", autofix: true }, "acme/api": {} } });
      },
    });
    expect(await repos.get("acme/widgets")).toEqual({ mode: "auto", autofix: true });
    expect(await repos.get("ACME/API")).toEqual({ mode: "on_request", autofix: false });
    expect(await repos.get("acme/other")).toBeNull();
  });

  test("no built-in, an archived one, or a malformed map lists nothing", async () => {
    expect(await makeEnrolledRepos({ async getByBuiltinKey() { return null; } }).get("a/b")).toBeNull();
    expect(
      await makeEnrolledRepos({ async getByBuiltinKey() { return builtin({ repos: { "a/b": {} } }, true); } }).get("a/b"),
    ).toBeNull();
    expect(await makeEnrolledRepos({ async getByBuiltinKey() { return builtin({ repos: ["a/b"] }); } }).get("a/b")).toBeNull();
  });

  test("policyOf tolerates a non-object entry", () => {
    expect(policyOf("auto")).toBeNull();
    expect(policyOf({ mode: "on_request", autofix: "yes" })).toEqual({ mode: "on_request", autofix: false });
  });
});
