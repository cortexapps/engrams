import { describe, expect, test } from "bun:test";

import { registerBuiltinTools } from "../builtin.ts";
import { compileToolManifest } from "../manifest.ts";
import { createToolRegistry } from "../registry.ts";

describe("built-in tools", () => {
  test("registers ask_user_question with the canonical AUQ contract", () => {
    const registry = createToolRegistry();
    registerBuiltinTools(registry);

    const tool = registry.get("ask_user_question");
    expect(tool).toBeDefined();
    expect(tool).toMatchObject({
      handling: "session",
      execution: "deferred",
      nativeBindings: {
        claude: "AskUserQuestion",
        codex: "requestUserInput",
      },
      presenters: {
        slack: "questionEffect",
        web: "UserQuestionCard",
      },
    });
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

  test("emits both native bindings and the canonical input schema", () => {
    const registry = createToolRegistry();
    registerBuiltinTools(registry);

    expect(compileToolManifest(registry)).toEqual([{
      name: "ask_user_question",
      description: "Ask the user one or more structured questions.",
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
        claude: "AskUserQuestion",
        codex: "requestUserInput",
      },
    }]);
  });
});
