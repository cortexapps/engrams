import { describe, expect, it } from "vitest";

import {
  buildInputsPayload,
  defaultMapRow,
  inputErrorFromServer,
  inputsEqual,
  isValidMapKey,
  listElementField,
  mapValueFields,
  normalizeMapKey,
  parseInputsSchema,
  resolveInputValues,
  validateInputs,
} from "@/lib/automation-inputs";
import { REVIEW_INPUTS_SCHEMA } from "@/lib/automation-inputs.fixture";

describe("parseInputsSchema", () => {
  it("parses the review-shaped schema and drops malformed entries", () => {
    const schema = parseInputsSchema([
      ...REVIEW_INPUTS_SCHEMA,
      { key: 1 },
      { key: "x", type: "nope" },
      "junk",
    ]);
    expect(schema.map((s) => s.key)).toEqual([
      "repos",
      "profile",
      "mention",
      "categories",
      "instructions",
    ]);
    expect(schema[0]).toMatchObject({ type: "map", keyNoun: "repository", required: true });
    expect(schema[4]).toMatchObject({ multiline: true });
  });

  it("exposes map value fields and list element specs", () => {
    const schema = parseInputsSchema(REVIEW_INPUTS_SCHEMA);
    expect(mapValueFields(schema[0]!).map((f) => [f.key, f.type])).toEqual([
      ["mode", "enum"],
      ["autofix", "boolean"],
    ]);
    expect(defaultMapRow(schema[0]!)).toEqual({ mode: "on_request", autofix: false });
    expect(listElementField(schema[3]!)).toMatchObject({ type: "enum" });
    expect(listElementField(schema[4]!)).toMatchObject({ type: "string" });
  });
});

describe("resolveInputValues", () => {
  const schema = parseInputsSchema(REVIEW_INPUTS_SCHEMA);

  it("layers stored values over schema defaults and ignores wrong shapes", () => {
    const values = resolveInputValues(schema, {
      repos: { "engrams/engrams": { mode: "auto", autofix: false } },
      mention: 42, // wrong shape → default
      categories: ["security"],
    });
    expect(values).toEqual({
      repos: { "engrams/engrams": { mode: "auto", autofix: false } },
      profile: "pr_reviewer",
      mention: "@engrams",
      categories: ["security"],
      instructions: "",
    });
  });

  it("tolerates an unparseable stored blob", () => {
    expect(resolveInputValues(schema, "not an object")["mention"]).toBe("@engrams");
  });
});

describe("validateInputs", () => {
  const schema = parseInputsSchema(REVIEW_INPUTS_SCHEMA);
  const good = resolveInputValues(schema, {
    repos: { "engrams/engrams": { mode: "auto", autofix: true } },
  });

  it("accepts a valid value set", () => {
    expect(validateInputs(schema, good)).toEqual([]);
  });

  it("enforces required, enum membership, map key shape, and row fields", () => {
    expect(validateInputs(schema, { ...good, repos: {} })).toEqual([
      { key: "repos", message: "Repositories is required" },
    ]);
    expect(validateInputs(schema, { ...good, categories: ["security", "vibes"] })).toEqual([
      { key: "categories", path: "1", message: expect.stringContaining("must be one of") },
    ]);
    const bad = validateInputs(schema, {
      ...good,
      repos: {
        "not a repo": { mode: "auto", autofix: true },
        "acme/repo": { mode: "sometimes", autofix: "yes" },
      },
    });
    expect(bad).toEqual([
      { key: "repos", path: "not a repo", message: expect.stringContaining("owner/repo") },
      { key: "repos", path: "acme/repo.mode", message: expect.stringContaining("must be one of") },
      { key: "repos", path: "acme/repo.autofix", message: "must be on or off" },
    ]);
  });
});

describe("map keys", () => {
  it("validates per noun and normalizes repos to lowercase", () => {
    expect(isValidMapKey("repository", "Engrams/Engrams")).toBe(true);
    expect(isValidMapKey("repository", "engrams")).toBe(false);
    expect(isValidMapKey("channel", "C0123456789")).toBe(true);
    expect(isValidMapKey("channel", "#eng-reviews")).toBe(true);
    expect(isValidMapKey("channel", "eng")).toBe(false);
    expect(isValidMapKey("team", "ENG")).toBe(true);
    expect(isValidMapKey("team", "eng")).toBe(false);
    expect(isValidMapKey(undefined, "anything")).toBe(true);
    expect(normalizeMapKey("repository", " Engrams/Engrams ")).toBe("engrams/engrams");
    expect(normalizeMapKey("team", " ENG ")).toBe("ENG");
  });
});

describe("payload + errors + equality", () => {
  const schema = parseInputsSchema(REVIEW_INPUTS_SCHEMA);

  it("builds a payload with only schema keys", () => {
    const payload = JSON.parse(
      buildInputsPayload(schema, { mention: "@bot", stray: 1, repos: {} }),
    ) as Record<string, unknown>;
    expect(payload).toEqual({ mention: "@bot", repos: {} });
  });

  it("routes server errors by field key with an optional path", () => {
    expect(inputErrorFromServer("inputs.repos.acme/repo.mode", "bad", schema)).toEqual({
      key: "repos",
      path: "acme/repo.mode",
      message: "bad",
    });
    expect(inputErrorFromServer("mention", "too long", schema)).toEqual({
      key: "mention",
      message: "too long",
    });
    expect(inputErrorFromServer("inputs.unknown", "x", schema)).toBeNull();
  });

  it("compares structurally regardless of key order", () => {
    expect(inputsEqual({ a: { y: 1, x: 2 }, b: [1] }, { b: [1], a: { x: 2, y: 1 } })).toBe(true);
    expect(inputsEqual({ a: 1 }, { a: 2 })).toBe(false);
  });
});
