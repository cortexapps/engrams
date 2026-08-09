import { describe, expect, test } from "bun:test";

import type { PapercutInput, PapercutStore } from "../../db/papercuts.ts";
import { registerBuiltinTools } from "../builtin.ts";
import { compileToolManifest } from "../manifest.ts";
import { createToolRegistry } from "../registry.ts";

function papercutRecorder(): { store: PapercutStore; inserted: PapercutInput[] } {
  const inserted: PapercutInput[] = [];
  const idsByToolCall = new Map<string, string>();
  const store: PapercutStore = {
    async insert(row) {
      const key = row.toolCallId == null ? null : `${row.sessionId}:${row.toolCallId}`;
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
    expect(
      tool!.input.parse({
        questions: [
          {
            question: "Deploy now?",
            header: "Deploy",
            multiSelect: false,
            options: [{ label: "Yes", description: "Deploy it" }],
          },
        ],
      }),
    ).toEqual({
      questions: [
        {
          question: "Deploy now?",
          header: "Deploy",
          multiSelect: false,
          options: [{ label: "Yes", description: "Deploy it" }],
        },
      ],
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
    expect(tool!.output.parse({ decision: "reject", feedback: "Cover tests." })).toEqual({
      decision: "reject",
      feedback: "Cover tests.",
    });
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
      "Artifact",
      "papercut",
      "spawn_session",
      "send_session_message",
      "read_session",
      "interrupt_session",
      "terminate_session",
      "list_sessions",
      "wait_sessions",
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

  // The claude CLI validates every MCP tool's inputSchema as a top-level
  // `type: "object"` JSON Schema and rejects the WHOLE tools/list when one
  // tool deviates (an anyOf union took every injected tool down with it —
  // session 3acf9bd1). This pins the contract for all current + future tools.
  test("every compiled tool schema is a top-level object (MCP contract)", () => {
    const registry = createToolRegistry();
    registerBuiltinTools(registry);
    for (const tool of compileToolManifest(registry)) {
      expect(
        (tool.inputSchema as { type?: string }).type,
        `tool ${tool.name} must emit a type:"object" inputSchema`,
      ).toBe("object");
    }
  });

  test("coordination schemas enforce names, guest paths, cursors, and wait limits", () => {
    const registry = createToolRegistry();
    registerBuiltinTools(registry);

    const spawn = registry.get("spawn_session")!;
    const read = registry.get("read_session")!;
    const wait = registry.get("wait_sessions")!;
    const path = "/tmp/uploads/019fe2ff-0464-75f3-bb20-a8c1844579b9/design-notes.pdf";

    expect(
      spawn.input.parse({
        task_name: "api-tests",
        message: `Review ${path}`,
        idempotency_key: "spawn-1",
        file_paths: [path],
      }),
    ).toMatchObject({ task_name: "api-tests", file_paths: [path] });
    expect(
      spawn.input.parse({
        task_name: "map-1",
        message: "Sum the numbers",
        idempotency_key: "spawn-map-1",
        file_paths: ["/tmp/numbers.txt", "/workspace/map input.txt"],
      }),
    ).toMatchObject({ file_paths: ["/tmp/numbers.txt", "/workspace/map input.txt"] });
    expect(() =>
      spawn.input.parse({
        task_name: "../sibling",
        message: "escape",
        idempotency_key: "spawn-2",
      }),
    ).toThrow();
    expect(() =>
      spawn.input.parse({
        task_name: "safe",
        message: "escape",
        idempotency_key: "spawn-3",
        file_paths: ["/tmp/../secret"],
      }),
    ).toThrow();
    expect(() =>
      spawn.input.parse({
        task_name: "safe",
        message: "relative",
        idempotency_key: "spawn-4",
        file_paths: ["tmp/numbers.txt"],
      }),
    ).toThrow();
    expect(
      read.input.parse({
        session_id: "019fe2ff-0464-75f3-bb20-a8c1844579b9",
        after_cursor: "-1",
      }),
    ).toMatchObject({ after_cursor: "-1" });
    expect(() => wait.input.parse({ timeout_ms: 120_001 })).toThrow();
  });

  test("papercut is included for a profile with zero capabilities", () => {
    const registry = createToolRegistry();
    registerBuiltinTools(registry);

    const manifest = compileToolManifest(registry, []);
    expect(manifest.map((tool) => tool.name)).toEqual([
      "ask_user_question",
      "exit_plan_mode",
      "Artifact",
      "papercut",
      "spawn_session",
      "send_session_message",
      "read_session",
      "interrupt_session",
      "terminate_session",
      "list_sessions",
      "wait_sessions",
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
    expect(papercuts.inserted).toEqual([
      {
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
      },
    ]);
  });
});
