import { describe, expect, test } from "bun:test";

import { registerEngineBlocks } from "../blocks/index.ts";
import { mergeBuiltinOverrides } from "../builtins.ts";
import {
  applyBlockOverrides,
  BlockOverrideError,
  type AutomationDefinition,
} from "../definition.ts";
import { previewDefinition } from "../preview.ts";

registerEngineBlocks();

function def(overrides: Partial<AutomationDefinition> = {}): AutomationDefinition {
  return {
    engine: 1,
    trigger: { kind: "manual" },
    blocks: [
      {
        id: "launch",
        type: "create_session",
        config: { profileId: "p", promptTemplate: "Review ${{ inputs.repo }}", deadline: 10 },
        tunable: ["promptTemplate", "model"],
      },
      {
        id: "gate",
        type: "branch",
        config: { conditions: { mode: "all", conditions: [{ path: "inputs.verify", op: "is_true" }] } },
        then: [
          {
            id: "verify",
            type: "send_prompt",
            config: { session: { blockId: "launch" }, promptTemplate: "Verify", waitFor: { kind: "run_end" } },
            tunable: ["promptTemplate"],
          },
        ],
      },
    ],
    inputsSchema: [
      { key: "repo", label: "Repo", type: "string", default: "a/b" },
      { key: "verify", label: "Verify", type: "boolean", default: true },
    ],
    settings: { endSessionsOnFinish: false },
    ...overrides,
  };
}

describe("applyBlockOverrides", () => {
  test("merges tunable fields, including inside nested lists, and leaves the graph intact", () => {
    const merged = applyBlockOverrides(def(), {
      launch: { promptTemplate: "Custom" },
      verify: { promptTemplate: "Verify harder" },
    });
    expect(merged.blocks[0]!.config).toMatchObject({ promptTemplate: "Custom", profileId: "p" });
    expect(merged.blocks[1]!.then![0]!.config).toMatchObject({ promptTemplate: "Verify harder" });
    // The original is untouched (a pinned version is immutable).
    expect(def().blocks[0]!.config["promptTemplate"]).toBe("Review ${{ inputs.repo }}");
  });

  test("rejects unknown blocks, non-tunable fields, and schema-breaking values", () => {
    expect(() => applyBlockOverrides(def(), { ghost: { promptTemplate: "x" } })).toThrow(BlockOverrideError);
    try {
      applyBlockOverrides(def(), { launch: { profileId: "other" } });
      throw new Error("expected");
    } catch (e) {
      expect(e).toBeInstanceOf(BlockOverrideError);
      expect((e as BlockOverrideError).field).toBe("profileId");
    }
    expect(() => applyBlockOverrides(def(), { launch: { promptTemplate: 42 } })).toThrow(BlockOverrideError);
  });

  test("no overrides is identity", () => {
    const d = def();
    expect(applyBlockOverrides(d, {})).toEqual(d);
  });
});

describe("mergeBuiltinOverrides (three-way)", () => {
  test("keeps a real org edit, drops an override equal to the old default, drops one equal to the new default", () => {
    const old = { promptTemplate: "v1 prompt", model: "opus" };
    const next = { promptTemplate: "v2 prompt", model: "sonnet" };
    const override = { promptTemplate: "v1 prompt", model: "haiku" };
    expect(mergeBuiltinOverrides(old, next, override)).toEqual({ model: "haiku" });
    expect(mergeBuiltinOverrides(old, next, { model: "sonnet" })).toEqual({});
    expect(mergeBuiltinOverrides(old, next, { promptTemplate: "mine" })).toEqual({ promptTemplate: "mine" });
  });

  test("structural values compare deeply", () => {
    const old = { categories: ["a", "b"] };
    const next = { categories: ["a", "b", "c"] };
    expect(mergeBuiltinOverrides(old, next, { categories: ["a", "b"] })).toEqual({});
    expect(mergeBuiltinOverrides(old, next, { categories: ["z"] })).toEqual({ categories: ["z"] });
  });
});

describe("previewDefinition", () => {
  const base = {
    automationId: "a",
    automationName: "Preview",
    trigger: { kind: "manual" as const, receivedAt: "2026-08-21T00:00:00Z" },
    aliases: [],
  };

  test("renders each block against its scope and follows branch decisions", async () => {
    const res = await previewDefinition({ ...base, definition: def(), inputs: { repo: "x/y", verify: true } });
    expect(res.errors).toEqual([]);
    expect(res.blocks.map((b) => b.blockId)).toEqual(["launch", "gate", "verify"]);
    expect(res.blocks[0]!.rendered).toMatchObject({ promptTemplate: "Review x/y" });
    expect(res.blocks[0]!.scope).toMatchObject({ inputs: { repo: "x/y" } });
    // After launch, steps.launch exists (empty) so the verify scope shows it.
    expect(res.blocks[2]!.scope).toMatchObject({ steps: { launch: {}, gate: { taken: "then" } } });

    const skipped = await previewDefinition({ ...base, definition: def(), inputs: { repo: "x/y", verify: false } });
    expect(skipped.blocks.map((b) => b.blockId)).toEqual(["launch", "gate"]);
  });

  test("a filter miss stops the walk with filterPass=false", async () => {
    const d = def({
      blocks: [
        { id: "only", type: "filter", config: { conditions: { mode: "all", conditions: [{ path: "inputs.verify", op: "is_true" }] } } },
        ...def().blocks,
      ],
    });
    const res = await previewDefinition({ ...base, definition: d, inputs: { repo: "q", verify: false } });
    expect(res.blocks.map((b) => b.blockId)).toEqual(["only"]);
    expect(res.blocks[0]!.filterPass).toBe(false);
  });

  test("render errors are addressed to the block and field, and the walk continues", async () => {
    const d = def({
      blocks: [
        { id: "launch", type: "create_session", config: { profileId: "p", promptTemplate: "${{ inputs.nope }}" } },
        { id: "after", type: "end_session", config: { session: { blockId: "launch" } } },
      ],
    });
    const res = await previewDefinition({ ...base, definition: d, inputs: {} });
    expect(res.errors).toEqual([expect.objectContaining({ blockId: "launch", field: "promptTemplate" })]);
    expect(res.blocks.map((b) => b.blockId)).toEqual(["launch", "after"]);
  });
});
