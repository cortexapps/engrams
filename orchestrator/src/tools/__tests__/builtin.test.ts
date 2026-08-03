import { describe, expect, test } from "bun:test";

import type {
  PapercutInput,
  PapercutStore,
} from "../../db/papercuts.ts";
import { registerBuiltinTools } from "../builtin.ts";
import { compileToolManifest } from "../manifest.ts";
import { createToolRegistry } from "../registry.ts";

function papercutRecorder(): { store: PapercutStore; inserted: PapercutInput[] } {
  const inserted: PapercutInput[] = [];
  const idsByToolCall = new Map<string, string>();
  const store: PapercutStore = {
    async insert(row) {
      const key = row.toolCallId == null
        ? null
        : `${row.sessionId}:${row.toolCallId}`;
      const existing = key == null ? undefined : idsByToolCall.get(key);
      if (existing) return existing;

      const id = `papercut-${inserted.length + 1}`;
      inserted.push(row);
      if (key != null) idsByToolCall.set(key, id);
      return id;
    },
    async list() {
      return [];
    },
    async get() {
      return null;
    },
    async setArchived() {},
  };
  return { store, inserted };
}

describe("built-in tools", () => {
  test("registers ask_user_question with the canonical AUQ contract", () => {
    const registry = createToolRegistry();
    registerBuiltinTools(registry);

    const tool = registry.get("ask_user_question");
    expect(tool).toBeDefined();
    expect(tool).toMatchObject({
      handling: "session",
      execution: "deferred",
      // No claude binding: the CLI removed the AskUserQuestion built-in from
      // headless mode, so claude gets this tool through the injected MCP path.
      nativeBindings: {
        codex: "requestUserInput",
      },
      presenters: {
        slack: "questionEffect",
        web: "UserQuestionCard",
      },
    });
    expect(tool!.nativeBindings?.claude).toBeUndefined();
    expect(tool!.input.parse({
      questions: [{
        question: "Deploy now?",
        header: "Deploy",
        multiSelect: false,
        options: [{ label: "Yes", description: "Deploy it" }],
      }],
    })).toEqual({
      questions: [{
        question: "Deploy now?",
        header: "Deploy",
        multiSelect: false,
        options: [{ label: "Yes", description: "Deploy it" }],
      }],
    });
    expect(tool!.output.parse({ "Deploy now?": ["Yes"] })).toEqual({
      "Deploy now?": ["Yes"],
    });
  });

  test("registers exit_plan_mode as a deferred session tool with no native bindings", () => {
    const registry = createToolRegistry();
    registerBuiltinTools(registry);

    const tool = registry.get("exit_plan_mode");
    expect(tool).toBeDefined();
    expect(tool).toMatchObject({
      handling: "session",
      execution: "deferred",
      presenters: {
        slack: "planEffect",
        web: "PlanCard",
      },
    });
    // No bindings at all (ADR 0107 + the headless built-in removals): every
    // harness receives the tool through the injected deferred path.
    expect(tool!.nativeBindings).toBeUndefined();
    expect(tool!.input.parse({ plan: "# Plan\n\n1. Do the thing." })).toEqual({
      plan: "# Plan\n\n1. Do the thing.",
    });
    expect(tool!.output.parse({ decision: "approve" })).toEqual({
      decision: "approve",
    });
    expect(tool!.output.parse({ decision: "reject", feedback: "Cover tests." }))
      .toEqual({ decision: "reject", feedback: "Cover tests." });
    expect(() => tool!.output.parse({ decision: "maybe" })).toThrow();
    expect(() => tool!.input.parse({})).toThrow();
  });

  test("emits the codex binding and the canonical input schema", () => {
    const registry = createToolRegistry();
    registerBuiltinTools(registry);

    const manifest = compileToolManifest(registry);
    expect(manifest.map((tool) => tool.name)).toEqual([
      "ask_user_question",
      "exit_plan_mode",
      "papercut",
    ]);
    expect(manifest[0]).toEqual({
      name: "ask_user_question",
      description:
        "Ask the user one or more structured questions and wait for their " +
        "answers. Use this whenever you need a decision, clarification, or " +
        "preference from the user before you continue.",
      inputSchema: {
        $schema: "https://json-schema.org/draft/2020-12/schema",
        type: "object",
        properties: {
          questions: {
            type: "array",
            items: {
              type: "object",
              properties: {
                question: { type: "string" },
                header: { type: "string" },
                multiSelect: { type: "boolean" },
                options: {
                  type: "array",
                  items: {
                    type: "object",
                    properties: {
                      label: { type: "string" },
                      description: { type: "string" },
                    },
                    required: ["label", "description"],
                    additionalProperties: false,
                  },
                },
              },
              required: ["question", "header", "multiSelect", "options"],
              additionalProperties: false,
            },
          },
        },
        required: ["questions"],
        additionalProperties: false,
      },
      execution: "deferred",
      nativeBindings: {
        codex: "requestUserInput",
      },
    });
  });

  test("papercut is included for a profile with zero capabilities", () => {
    const registry = createToolRegistry();
    registerBuiltinTools(registry);

    const manifest = compileToolManifest(registry, []);
    expect(manifest.map((tool) => tool.name)).toEqual([
      "ask_user_question",
      "exit_plan_mode",
      "papercut",
    ]);
    expect(manifest.find((tool) => tool.name === "papercut")).toMatchObject({
      execution: "sync",
      nativeBindings: {},
    });
  });

  test("papercut handler persists once and returns the same id on step replay", async () => {
    const registry = createToolRegistry();
    const papercuts = papercutRecorder();
    registerBuiltinTools(registry, { papercuts: papercuts.store });

    const tool = registry.get("papercut");
    if (!tool || tool.handling !== "handled") throw new Error("papercut tool not registered");
    const ctx = {
      sessionId: "session-1",
      taskId: "task-1",
      profileId: "profile-1",
      userId: "user-1",
      capabilities: [],
      toolCallId: "call-1",
      toolName: "papercut",
    };
    const args = tool.input.parse({
      summary: "Command output was unclear",
      description: "The error omitted the failing file; a path would have helped.",
      category: "tooling",
      severity: "medium",
      tags: ["errors", "cli"],
    });
    const first = await tool.handler(ctx, args);
    const replay = await tool.handler(ctx, args);

    expect(first).toEqual({ logged: true, id: "papercut-1" });
    expect(replay).toEqual(first);
    expect(papercuts.inserted).toEqual([{
      summary: "Command output was unclear",
      description: "The error omitted the failing file; a path would have helped.",
      category: "tooling",
      severity: "medium",
      tags: ["errors", "cli"],
      sessionId: "session-1",
      toolCallId: "call-1",
      taskId: "task-1",
      profileId: "profile-1",
      userId: "user-1",
    }]);
  });
});
