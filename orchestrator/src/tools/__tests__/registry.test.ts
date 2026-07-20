import { describe, expect, test } from "bun:test";
import { z } from "zod";

import { compileToolManifest } from "../manifest.ts";
import { createToolRegistry, toolCapabilities } from "../registry.ts";

describe("tool registry", () => {
  test("duplicate names are rejected", () => {
    const registry = createToolRegistry();
    const definition = {
      name: "save_memory",
      description: "Save a note.",
      input: z.object({ text: z.string() }),
      output: z.object({ saved: z.boolean() }),
      handling: "handled" as const,
      execution: "sync" as const,
      handler: async () => ({ saved: true }),
    };

    registry.register(definition);
    expect(() => registry.register(definition)).toThrow("tool already registered: save_memory");
  });

  test("handled tools require a handler instead of presenters alone", () => {
    const registry = createToolRegistry();

    expect(() =>
      registry.register({
        name: "broken_handled",
        description: "Has presentation but no implementation.",
        input: z.object({}),
        output: z.object({ ok: z.boolean() }),
        handling: "handled",
        execution: "sync",
        presenters: { web: "BrokenCard" },
      } as never),
    ).toThrow("handled tool broken_handled requires a handler");
  });

  test("session tools forbid handlers and are implicitly deferred", () => {
    const registry = createToolRegistry();

    expect(() =>
      registry.register({
        name: "broken_session",
        description: "Must be completed by a surface.",
        input: z.object({}),
        output: z.object({ ok: z.boolean() }),
        handling: "session",
        handler: async () => ({ ok: true }),
      } as never),
    ).toThrow("session tool broken_session forbids a handler");

    expect(() =>
      registry.register({
        name: "sync_session",
        description: "Cannot be synchronous.",
        input: z.object({}),
        output: z.object({ ok: z.boolean() }),
        handling: "session",
        execution: "sync",
        presenters: { web: "QuestionCard" },
      } as never),
    ).toThrow("session tool sync_session must use deferred execution");

    const registered = registry.register({
      name: "ask_user_question",
      description: "Ask the user a question.",
      input: z.object({ question: z.string() }),
      output: z.object({ answer: z.string() }),
      handling: "session",
      presenters: { web: "QuestionCard" },
    });
    expect(registered.execution).toBe("deferred");
  });

  test("toolCapabilities returns each registered non-null capability", () => {
    const registry = createToolRegistry();
    for (const [name, capability] of [
      ["ungated", undefined],
      ["review_finding", "engram:pr_review"],
      ["review_verdict", "engram:pr_review"],
      ["other", "engram:other"],
    ] as const) {
      registry.register({
        name,
        description: name,
        input: z.object({}),
        output: z.object({ ok: z.boolean() }),
        handling: "handled",
        execution: "sync",
        capability,
        handler: async () => ({ ok: true }),
      });
    }

    expect(toolCapabilities(registry)).toEqual(new Set([
      "engram:pr_review",
      "engram:other",
    ]));
  });
});

describe("compileToolManifest", () => {
  test("compiles the exact harness manifest shape from zod schemas", () => {
    const registry = createToolRegistry();
    registry.register({
      name: "save_memory",
      description: "Save a note.",
      input: z.object({ text: z.string() }),
      output: z.object({ saved: z.boolean() }),
      handling: "handled",
      execution: "sync",
      handler: async () => ({ saved: true }),
      nativeBindings: { claude: "Remember" },
    });

    expect(compileToolManifest(registry)).toEqual([
      {
        name: "save_memory",
        description: "Save a note.",
        inputSchema: {
          $schema: "https://json-schema.org/draft/2020-12/schema",
          type: "object",
          properties: { text: { type: "string" } },
          required: ["text"],
          additionalProperties: false,
        },
        execution: "sync",
        nativeBindings: { claude: "Remember" },
      },
    ]);
  });
});
