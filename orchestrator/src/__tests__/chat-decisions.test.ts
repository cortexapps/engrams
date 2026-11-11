/** A decisions request answered by a structured chat completion, and the
 * LiteLLM model list. */

import { describe, expect, test } from "bun:test";
import { APICallError } from "ai";
import { MockLanguageModelV4 } from "ai/test";

import { decideByChat } from "../model-routers/chat-decisions.ts";
import { DecisionsApiError } from "../model-routers/decisions.ts";
import { parseLiteLlmCatalog } from "../providers/litellm/router.ts";

const REQUEST = {
  model: "gpt-mini",
  state: "the build is red on main",
  questions: {
    urgent: { type: "noul" as const, instructions: "Is this urgent?" },
    team: {
      type: "choice" as const,
      instructions: "Which team owns it?",
      criteria: { infra: "build and deploy", web: null },
    },
    risk: { type: "score" as const, instructions: "How risky?", criteria: ["low", "medium", "high"] },
  },
};

function modelAnswering(answer: unknown): MockLanguageModelV4 {
  return new MockLanguageModelV4({
    doGenerate: {
      content: [{ type: "text", text: JSON.stringify(answer) }],
      finishReason: { unified: "stop", raw: undefined },
      usage: {
        inputTokens: { total: 40, noCache: 40, cacheRead: undefined, cacheWrite: undefined },
        outputTokens: { total: 12, text: 12, reasoning: undefined },
      },
      warnings: [],
    },
  });
}

async function failure(promise: Promise<unknown>): Promise<DecisionsApiError> {
  try {
    await promise;
  } catch (error) {
    expect(error).toBeInstanceOf(DecisionsApiError);
    return error as DecisionsApiError;
  }
  throw new Error("expected a failure");
}

describe("decision by chat completion", () => {
  test("maps the model's object onto typed answers and clamps the numbers", async () => {
    const model = modelAnswering({
      urgent: { noul: 1.4 },
      team: { choice: "infra", confidence: 0.8 },
      risk: { score: 7, confidence: -1 },
    });
    const res = await decideByChat(model, REQUEST);
    expect(res.model).toBe("gpt-mini");
    expect(res.answers).toEqual({
      urgent: { type: "noul", noul: 1 },
      team: { type: "choice", choice: "infra", confidence: 0.8 },
      risk: { type: "score", score: 2, confidence: 0 },
    });
    expect(res.usage).toEqual({ input_tokens: 40, output_tokens: 12 });
    // The model reads the state and the questions as sent.
    expect(JSON.stringify(model.doGenerateCalls[0]!.prompt)).toContain("the build is red on main");
  });

  test("a choice outside the options is a retryable failure", async () => {
    const model = modelAnswering({
      urgent: { noul: 0.5 },
      team: { choice: "sales", confidence: 0.9 },
      risk: { score: 1, confidence: 0.5 },
    });
    expect((await failure(decideByChat(model, REQUEST))).retryable).toBe(true);
  });

  test("an API rejection that a retry cannot fix is permanent", async () => {
    const model = new MockLanguageModelV4({
      doGenerate: async () => {
        throw new APICallError({
          message: "unknown model",
          url: "https://llm.example.com/v1/chat/completions",
          requestBodyValues: {},
          statusCode: 404,
          isRetryable: false,
        });
      },
    });
    const error = await failure(decideByChat(model, REQUEST));
    expect(error.retryable).toBe(false);
    expect(error.status).toBe(404);
  });
});

describe("LiteLLM model list", () => {
  test("keeps each model id once, with its owner", () => {
    const body = new TextEncoder().encode(JSON.stringify({
      object: "list",
      data: [
        { id: "claude-sonnet", object: "model", owned_by: "anthropic" },
        { id: "gpt-mini", object: "model" },
        { id: "claude-sonnet", object: "model", owned_by: "anthropic" },
      ],
    }));
    const models = parseLiteLlmCatalog(body);
    expect(models.map((model) => [model.modelId, model.author])).toEqual([
      ["claude-sonnet", "anthropic"],
      ["gpt-mini", null],
    ]);
  });

  test("a body that is not a model list is an error", () => {
    expect(() => parseLiteLlmCatalog(new TextEncoder().encode('{"error":"nope"}'))).toThrow();
  });
});
