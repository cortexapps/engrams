/** Answers a decisions request with one structured chat completion, for a
 * router with no native decisions endpoint.
 *
 * The numbers are the model's own estimates, not calibrated probabilities:
 * a graph that gates on a threshold needs it tuned for the chosen model.
 */

import { APICallError, generateObject, RetryError, type LanguageModel } from "ai";
import { z } from "zod";

import {
  DecisionsApiError,
  type DecisionAnswer,
  type DecisionQuestion,
  type DecisionsRequest,
  type DecisionsResponse,
} from "./decisions.ts";
import { isPermanentModelError } from "./errors.ts";

const SYSTEM_PROMPT = [
  "You answer questions about the given state. Return one answer per question id.",
  "A noul question: `noul` is the probability from 0 to 1 that the answer is yes.",
  "A choice question: `choice` is exactly one of the option labels in `criteria`.",
  "A score question: `score` is the zero-based index of the level in `criteria` that fits best; the levels go from lowest to highest.",
  "`confidence` is your probability from 0 to 1 that the answer is right.",
].join("\n");

/** One call, no SDK retries: the decide block makes the one retry. */
const TIMEOUT_MS = 20_000;

const unit = (value: number): number => Math.max(0, Math.min(1, value));

function answerSchema(question: DecisionQuestion) {
  switch (question.type) {
    case "noul":
      return z.object({ noul: z.number() });
    case "choice":
      // A plain string: the labels are in the prompt, and a strict schema
      // limits the enum values it accepts. The decide block rejects a label
      // that is not an option.
      return z.object({ choice: z.string(), confidence: z.number() });
    case "score":
      return z.object({ score: z.number(), confidence: z.number() });
  }
}

function toAnswer(question: DecisionQuestion, raw: Record<string, unknown>): DecisionAnswer {
  switch (question.type) {
    case "noul":
      return { type: "noul", noul: unit(raw["noul"] as number) };
    case "choice":
      return { type: "choice", choice: raw["choice"] as string, confidence: unit(raw["confidence"] as number) };
    case "score":
      return {
        type: "score",
        score: Math.max(0, Math.min(question.criteria.length - 1, raw["score"] as number)),
        confidence: unit(raw["confidence"] as number),
      };
  }
}

export async function decideByChat(
  model: LanguageModel,
  request: DecisionsRequest & { model: string },
): Promise<DecisionsResponse> {
  const questions = Object.entries(request.questions);
  try {
    const { object, usage } = await generateObject({
      model,
      schema: z.object(Object.fromEntries(questions.map(([id, question]) => [id, answerSchema(question)]))),
      system: SYSTEM_PROMPT,
      prompt: JSON.stringify({ state: request.state, questions: request.questions }),
      maxRetries: 0,
      abortSignal: AbortSignal.timeout(TIMEOUT_MS),
    });
    const answers = object as Record<string, Record<string, unknown>>;
    return {
      model: request.model,
      answers: Object.fromEntries(questions.map(([id, question]) => [id, toAnswer(question, answers[id]!)])),
      usage: { input_tokens: usage.inputTokens ?? 0, output_tokens: usage.outputTokens ?? 0 },
    };
  } catch (error) {
    // A network fault, a timeout, a rate limit, or an answer that does not
    // fit the schema is worth one more call.
    const cause = RetryError.isInstance(error) ? error.lastError : error;
    throw new DecisionsApiError(
      `decision by chat completion failed: ${error instanceof Error ? error.message : String(error)}`,
      APICallError.isInstance(cause) ? (cause.statusCode ?? null) : null,
      !isPermanentModelError(error),
    );
  }
}
