import { describe, expect, test } from "bun:test";

import { inputFieldSchema, type InputFieldSpec } from "../engine/definition.ts";
import {
  InputValidationError,
  inputErrorField,
  isValidMapKey,
  validateInputValues,
} from "../inputs.ts";

/** LITERAL copy of web/src/lib/automation-inputs.fixture.ts (the PR-review
 * built-in's schema as the web renders it). The two validators are pinned to
 * each other through this fixture: what the Inputs tab accepts is what
 * SetInputs accepts. Keep byte-identical with the web file. */
const REVIEW_INPUTS_SCHEMA_JSON = `[
  {
    "key": "repos",
    "label": "Repositories",
    "type": "map",
    "keyNoun": "repository",
    "required": true,
    "help": "Which repositories this review watches, and how each one triggers.",
    "valueShape": {
      "mode": { "type": "enum", "label": "Mode", "values": ["auto", "on_request"], "default": "on_request" },
      "autofix": { "type": "boolean", "label": "Autofix", "default": false }
    }
  },
  { "key": "profile", "label": "Reviewer profile", "type": "string", "default": "pr_reviewer" },
  { "key": "mention", "label": "Mention", "type": "string", "default": "@engrams" },
  {
    "key": "categories",
    "label": "Categories",
    "type": "list",
    "valueShape": {
      "element": {
        "type": "enum",
        "values": ["functional-correctness", "security", "performance", "maintainability", "testing", "docs"]
      }
    },
    "default": ["functional-correctness", "security"]
  },
  { "key": "instructions", "label": "Instructions", "type": "string", "multiline": true }
]`;

const REVIEW_SCHEMA: InputFieldSpec[] = (JSON.parse(REVIEW_INPUTS_SCHEMA_JSON) as unknown[]).map(
  (raw) => inputFieldSchema.parse(raw),
);

function field(overrides: Partial<InputFieldSpec> & Pick<InputFieldSpec, "key" | "type">): InputFieldSpec {
  return { label: overrides.key, ...overrides };
}

describe("validateInputValues — the review schema (shared fixture)", () => {
  test("the engine schema accepts the web fixture verbatim, multiline included", () => {
    expect(REVIEW_SCHEMA.find((f) => f.key === "instructions")?.multiline).toBe(true);
    expect(REVIEW_SCHEMA).toHaveLength(5);
  });

  test("a valid org configuration passes", () => {
    expect(
      validateInputValues(REVIEW_SCHEMA, {
        repos: {
          "engrams/engrams": { mode: "auto", autofix: false },
          "cortex/brain-backend": { mode: "on_request", autofix: true },
        },
        profile: "pr_reviewer",
        mention: "@engrams",
        categories: ["security", "docs"],
        instructions: "Be terse.\nPrefer small diffs.",
      }),
    ).toEqual([]);
  });

  test("required map missing / empty → required", () => {
    expect(validateInputValues(REVIEW_SCHEMA, { profile: "p" })).toEqual([
      { key: "repos", code: "required", message: "Repositories is required" },
    ]);
    expect(validateInputValues(REVIEW_SCHEMA, { repos: {} })[0]?.code).toBe("required");
  });

  test("map rows: bad key, non-object row, enum + boolean fields — routed by path", () => {
    const errors = validateInputValues(REVIEW_SCHEMA, {
      repos: {
        "not-a-repo": { mode: "auto", autofix: false },
        "acme/x": "yes",
        "acme/y": { mode: "sometimes", autofix: "no" },
      },
    });
    expect(errors).toEqual([
      { key: "repos", path: "not-a-repo", code: "map_key", message: "not a valid owner/repo" },
      { key: "repos", path: "acme/x", code: "shape", message: "row must be an object" },
      { key: "repos", path: "acme/y.mode", code: "enum", message: "must be one of auto, on_request" },
      { key: "repos", path: "acme/y.autofix", code: "type", message: "must be on or off" },
    ]);
    expect(errors.map(inputErrorField)).toEqual([
      "inputs.repos.not-a-repo",
      "inputs.repos.acme/x",
      "inputs.repos.acme/y.mode",
      "inputs.repos.acme/y.autofix",
    ]);
  });

  test("list elements validate against the element enum, routed by index", () => {
    expect(
      validateInputValues(REVIEW_SCHEMA, {
        repos: { "a/b": { mode: "auto", autofix: false } },
        categories: ["security", "vibes"],
      }),
    ).toEqual([
      { key: "categories", path: "1", code: "enum", message: expect.stringContaining("must be one of") },
    ]);
  });
});

describe("validateInputValues — every rule", () => {
  test("scalars", () => {
    const schema = [
      field({ key: "s", type: "string" }),
      field({ key: "n", type: "number" }),
      field({ key: "b", type: "boolean" }),
      field({ key: "e", type: "enum", values: ["x", "y"] }),
      field({ key: "r", type: "secret_ref" }),
      field({ key: "j", type: "json" }),
    ];
    expect(validateInputValues(schema, { s: "ok", n: 1.5, b: true, e: "y", r: "GH", j: { k: 1 } })).toEqual([]);
    expect(validateInputValues(schema, { s: 1, n: "1", b: "yes", e: "z", r: 2, j: [1] }).map((e) => e.key)).toEqual([
      "s",
      "n",
      "b",
      "e",
      "r",
      "j",
    ]);
    // NaN / Infinity are not numbers.
    expect(validateInputValues(schema, { n: Number.NaN })[0]?.message).toBe("must be a number");
  });

  test("empty values are skipped unless required (matches the web)", () => {
    const schema = [field({ key: "s", type: "string" }), field({ key: "req", type: "string", required: true })];
    expect(validateInputValues(schema, { s: "", req: "x" })).toEqual([]);
    expect(validateInputValues(schema, { s: null, req: "x" })).toEqual([]);
    expect(validateInputValues(schema, { req: "" })[0]?.code).toBe("required");
  });

  test("map without a noun accepts any non-blank key; list with no element shape is free strings", () => {
    const schema = [
      field({ key: "m", type: "map", valueShape: { v: { type: "number" } } }),
      field({ key: "l", type: "list" }),
    ];
    expect(validateInputValues(schema, { m: { anything: { v: 2 } }, l: ["a", "b"] })).toEqual([]);
    expect(validateInputValues(schema, { m: { " ": { v: 2 } } })[0]?.code).toBe("map_key");
    expect(validateInputValues(schema, { l: ["a", 2] })[0]).toMatchObject({ key: "l", path: "1", code: "type" });
  });

  test("map key nouns", () => {
    expect(isValidMapKey("repository", "Acme/Repo.js")).toBe(true);
    expect(isValidMapKey("repository", "acme")).toBe(false);
    expect(isValidMapKey("channel", "C0123456789")).toBe(true);
    expect(isValidMapKey("channel", "#eng-reviews")).toBe(true);
    expect(isValidMapKey("channel", "general")).toBe(false);
    expect(isValidMapKey("team", "ENG")).toBe(true);
    expect(isValidMapKey("team", "eng")).toBe(false);
    expect(isValidMapKey(undefined, "")).toBe(false);
  });

  test("undeclared keys are not this function's concern", () => {
    expect(validateInputValues([field({ key: "a", type: "string" })], { a: "x", stray: 1 })).toEqual([]);
  });

  test("InputValidationError carries every routed line", () => {
    const error = new InputValidationError(
      validateInputValues(REVIEW_SCHEMA, { repos: { bad: { mode: "auto", autofix: true } } }),
    );
    expect(error.message).toBe(
      "inputs rejected by the automation's schema: inputs.repos.bad: not a valid owner/repo",
    );
  });
});

describe("validateInputValues — number bounds", () => {
  const schema: InputFieldSpec[] = [
    { key: "idle_timeout", label: "Idle", type: "number", min: 60, max: 86400 },
    { key: "floor_only", label: "Floor", type: "number", min: 1 },
    { key: "cap_only", label: "Cap", type: "number", max: 10 },
  ];
  test("in range passes; out of range is a range error with the web's wording", () => {
    expect(validateInputValues(schema, { idle_timeout: 3600, floor_only: 1, cap_only: 10 })).toEqual([]);
    expect(validateInputValues(schema, { idle_timeout: 90_000 })).toEqual([
      { key: "idle_timeout", code: "range", message: "must be between 60 and 86400" },
    ]);
    expect(validateInputValues(schema, { idle_timeout: 5 })).toEqual([
      { key: "idle_timeout", code: "range", message: "must be between 60 and 86400" },
    ]);
    expect(validateInputValues(schema, { floor_only: 0 })).toEqual([
      { key: "floor_only", code: "range", message: "must be at least 1" },
    ]);
    expect(validateInputValues(schema, { cap_only: 11 })).toEqual([
      { key: "cap_only", code: "range", message: "must be at most 10" },
    ]);
  });
});
