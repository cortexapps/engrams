import { describe, expect, test } from "bun:test";

import { registerEngineBlocks } from "../blocks/index.ts";
import { DefinitionError, validateDefinition, type AutomationDefinition } from "../definition.ts";

registerEngineBlocks();

function def(overrides: Partial<AutomationDefinition> = {}): unknown {
  return {
    engine: 1,
    trigger: { kind: "cron", schedule: "0 9 * * 1-5", timezone: "UTC" },
    blocks: [
      {
        id: "launch",
        type: "create_session",
        config: { profileId: "p1", promptTemplate: "Do the thing for ${{ trigger.automation.name }}" },
      },
    ],
    inputsSchema: [],
    settings: { endSessionsOnFinish: false },
    ...overrides,
  };
}

describe("validateDefinition", () => {
  test("accepts a minimal user definition", () => {
    const parsed = validateDefinition(def(), { kind: "user" });
    expect(parsed.blocks[0]!.type).toBe("create_session");
  });

  test("rejects duplicate block ids across nesting", () => {
    const raw = def({
      blocks: [
        { id: "a", type: "filter", config: { conditions: { mode: "all", conditions: [] } } },
        {
          id: "b",
          type: "branch",
          config: { conditions: { mode: "all", conditions: [] } },
          then: [{ id: "a", type: "end_session", config: { session: { blockId: "x" } } }],
        },
      ],
    } as Partial<AutomationDefinition>);
    expect(() => validateDefinition(raw, { kind: "user" })).toThrow(/duplicate block id/);
  });

  test("rejects unknown block types and bad configs with a block+field address", () => {
    expect(() =>
      validateDefinition(def({ blocks: [{ id: "x", type: "teleport", config: {} }] } as never), {
        kind: "user",
      }),
    ).toThrow(/unknown block type/);
    try {
      validateDefinition(
        def({ blocks: [{ id: "x", type: "create_session", config: {} }] } as never),
        { kind: "user" },
      );
      throw new Error("expected DefinitionError");
    } catch (error) {
      expect(error).toBeInstanceOf(DefinitionError);
      expect((error as DefinitionError).blockId).toBe("x");
    }
  });

  test("system block types are builtin-only", () => {
    const raw = def({
      blocks: [{ id: "x", type: "system.review_policy_gate", config: { reviewId: "r-1" } }],
    } as never);
    expect(() => validateDefinition(raw, { kind: "user" })).toThrow(/reserved for built-in/);
    // A built-in may reference it (phase 4.2 registers the review blocks),
    // and its config schema is enforced like any other block's.
    expect(validateDefinition(raw, { kind: "builtin" }).blocks[0]!.type).toBe(
      "system.review_policy_gate",
    );
    const badConfig = def({
      blocks: [{ id: "x", type: "system.review_policy_gate", config: {} }],
    } as never);
    expect(() => validateDefinition(badConfig, { kind: "builtin" })).toThrow(DefinitionError);
    // An unregistered system type is still unknown for a built-in.
    const unknown = def({ blocks: [{ id: "x", type: "system.not_a_thing", config: {} }] } as never);
    expect(() => validateDefinition(unknown, { kind: "builtin" })).toThrow(/unknown block type/);
  });

  test("rejects invalid Liquid templates with the offending field", () => {
    const raw = def({
      blocks: [
        {
          id: "x",
          type: "create_session",
          config: { profileId: "p", promptTemplate: "${{ event.title | upcase" },
        },
      ],
    } as never);
    try {
      validateDefinition(raw, { kind: "user" });
      throw new Error("expected DefinitionError");
    } catch (error) {
      expect(error).toBeInstanceOf(DefinitionError);
      expect((error as DefinitionError).field).toBe("promptTemplate");
    }
  });

  test("branch/loop nesting rules", () => {
    expect(() =>
      validateDefinition(
        def({
          blocks: [{ id: "x", type: "branch", config: { conditions: { mode: "all", conditions: [] } } }],
        } as never),
        { kind: "user" },
      ),
    ).toThrow(/then/);
    expect(() =>
      validateDefinition(
        def({
          blocks: [
            {
              id: "x",
              type: "end_session",
              config: { session: { blockId: "y" } },
              body: [],
            },
          ],
        } as never),
        { kind: "user" },
      ),
    ).toThrow(/only loop blocks/);
  });
});
