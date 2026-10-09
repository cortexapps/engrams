/** The decide block: typed, calibrated decisions from a System One model
 * (TypeSafe's Jev) through the OpenRouter Decisions API.
 *
 * One block asks one or more questions about one `state` in a single call —
 * the questions are answered in parallel, so a second question adds almost
 * no latency. Three question types:
 *   - `choice`  picks one option; answers with the option's `value`, its
 *               `confidence`, a probability per option and the options
 *               ranked best first;
 *   - `score`   rates the state against 2-10 ordered levels;
 *   - `yes_no`  answers with the probability of "yes".
 * The answers are data a `branch`/`filter` gates on (`confidence gt 0.7`),
 * so the decision logic stays in the graph, not in a prompt.
 *
 * Uses: route a request to a profile or a team, triage an alert or an issue
 * into a label, rate a PR's risk or an incident's severity, decide "is this
 * noise?" before an expensive session.
 *
 * An org without OpenRouter gets `{ decided: false }` — a normal output, so a
 * graph falls back deterministically and the org keeps today's behavior.
 *
 * The call runs inside the block's checkpointed step: a call whose result was
 * recorded is never made again on replay. The guarantee is at-least-once, not
 * exactly-once — a crash after the provider answered and before the step was
 * recorded repeats the call (the Decisions API has no idempotency key).
 *
 * An editor dry run makes the call for real: the block is read-only and
 * costs fractions of a cent, and a dry run is how an author tries their
 * questions before saving.
 */

import { z } from "zod";

import type {
  DecisionAnswer,
  DecisionQuestion,
  DecisionsResponse,
  DecisionText,
} from "../../../integrations/openrouter-decisions.ts";
import { registerBlock, type BlockOutcome } from "./registry.ts";

/** Pinned so a model release never silently changes a graph's thresholds;
 * an author opts into a newer one through `model`. */
export const DEFAULT_DECISION_MODEL = "typesafe/jev-1.13";
export const MAX_DECIDE_QUESTIONS = 16;
export const MAX_CHOICE_OPTIONS = 255;

const decisionText = z.union([
  z.string().min(1),
  z.record(z.string(), z.unknown()),
  z.array(z.unknown()),
]);

/** One choice option, as a graph supplies it. */
export interface ChoiceOption {
  /** What the block outputs when this option wins (a profile id, a label).
   * Unique within the question. */
  value: string;
  /** What the model reads. Default: `value`. Unique within the question. */
  label?: string;
  /** What the option covers — the main lever for telling similar options
   * apart. A string or a structured object. */
  description?: DecisionText;
}

// Every field a run can fill in (`instructions` or an option's `value` from a
// template, `options` from a `$ref` to an earlier block, a templated level)
// is lenient in the SCHEMA: save-time validation strips run-time values —
// a nested required field or an array element too — and relaxes only
// top-level keys. `prepareQuestions` enforces all of it on the resolved
// config, before any call.
const choiceOptionSchema = z.object({
  value: z.string().min(1).optional(),
  label: z.string().min(1).optional(),
  description: decisionText.optional(),
});

const questionSchema = z.discriminatedUnion("type", [
  z.object({
    type: z.literal("choice"),
    instructions: decisionText.optional(),
    options: z.array(choiceOptionSchema).max(MAX_CHOICE_OPTIONS).optional(),
  }),
  z.object({
    type: z.literal("score"),
    instructions: decisionText.optional(),
    /** Ordered lowest → highest. */
    levels: z.array(decisionText).max(10).optional(),
  }),
  z.object({
    type: z.literal("yes_no"),
    instructions: decisionText.optional(),
    /** What "yes" and "no" mean; set both or neither. */
    yes: decisionText.optional(),
    no: decisionText.optional(),
  }),
]);
export type DecideQuestion = z.infer<typeof questionSchema>;

export const decideConfigSchema = z.object({
  /** What the questions are about: text, or a structured object (a thread,
   * an alert, a diff summary). */
  state: decisionText,
  questions: z
    .record(z.string().regex(/^[a-z][a-z0-9_]*$/), questionSchema)
    .refine((q) => Object.keys(q).length >= 1 && Object.keys(q).length <= MAX_DECIDE_QUESTIONS, {
      message: `ask between 1 and ${MAX_DECIDE_QUESTIONS} questions`,
    }),
  model: z.string().min(1).optional(),
  /** What a failed call does after one immediate retry. `fail` (default)
   * fails the block, so its retry policy applies. `undecided` returns
   * `{ decided: false }` and lets the graph fall back — for a decision that
   * must never block the run. */
  onError: z.enum(["fail", "undecided"]).optional(),
});
export type DecideConfig = z.infer<typeof decideConfigSchema>;

/** A wire request plus the label → value map per choice question. */
interface PreparedRequest {
  questions: Record<string, DecisionQuestion>;
  valuesByLabel: Record<string, Map<string, string>>;
  levels: Record<string, DecisionText[]>;
}

/** Map the block's questions onto the wire, enforcing what the schema could
 * not at save time. Pure; exported for tests and the benchmark. */
export function prepareQuestions(
  questions: Record<string, DecideQuestion>,
): { ok: true; value: PreparedRequest } | { ok: false; error: string } {
  const out: PreparedRequest = { questions: {}, valuesByLabel: {}, levels: {} };
  for (const [id, q] of Object.entries(questions)) {
    if (q.instructions === undefined) return { ok: false, error: `question "${id}" has no instructions` };
    switch (q.type) {
      case "choice": {
        if (!q.options || q.options.length === 0) {
          return { ok: false, error: `choice question "${id}" has no options` };
        }
        // Labels are author-controlled object keys: build the wire map from
        // entries (an own property even for "__proto__"), never by assignment.
        const criteria: Array<[string, DecisionText | null]> = [];
        const byLabel = new Map<string, string>();
        const values = new Set<string>();
        for (const option of q.options) {
          if (option.value === undefined) {
            return { ok: false, error: `choice question "${id}" has an option with no value` };
          }
          if (values.has(option.value)) {
            // Two options with one value would merge their probabilities.
            return { ok: false, error: `choice question "${id}" repeats the option value "${option.value}"` };
          }
          values.add(option.value);
          const label = option.label ?? option.value;
          if (byLabel.has(label)) {
            return { ok: false, error: `choice question "${id}" repeats the option label "${label}"` };
          }
          byLabel.set(label, option.value);
          criteria.push([label, option.description ?? null]);
        }
        out.questions[id] = {
          type: "choice",
          instructions: q.instructions,
          criteria: Object.fromEntries(criteria),
        };
        out.valuesByLabel[id] = byLabel;
        break;
      }
      case "score": {
        if (!q.levels || q.levels.length < 2) {
          return { ok: false, error: `score question "${id}" needs at least 2 levels` };
        }
        out.questions[id] = { type: "score", instructions: q.instructions, criteria: q.levels };
        out.levels[id] = q.levels;
        break;
      }
      case "yes_no": {
        if ((q.yes === undefined) !== (q.no === undefined)) {
          return { ok: false, error: `yes_no question "${id}" sets one of yes/no; set both or neither` };
        }
        out.questions[id] =
          q.yes !== undefined && q.no !== undefined
            ? { type: "noul", instructions: q.instructions, criteria: { true: q.yes, false: q.no } }
            : { type: "noul", instructions: q.instructions };
        break;
      }
    }
  }
  return { ok: true, value: out };
}

/** Translate the wire answers into the block's outputs. Pure; exported for
 * tests and the benchmark. */
export function mapAnswers(
  prepared: PreparedRequest,
  answers: Record<string, DecisionAnswer>,
): { ok: true; value: Record<string, unknown> } | { ok: false; error: string } {
  const out: Record<string, unknown> = {};
  for (const [id, question] of Object.entries(prepared.questions)) {
    const answer = answers[id];
    if (!answer || answer.type !== question.type) {
      return { ok: false, error: `the model returned no ${question.type} answer for "${id}"` };
    }
    switch (answer.type) {
      case "choice": {
        const byLabel = prepared.valuesByLabel[id]!;
        const value = byLabel.get(answer.choice);
        if (value === undefined) {
          return { ok: false, error: `the model chose "${answer.choice}", not an option of "${id}"` };
        }
        const byValue = new Map<string, number>();
        for (const [label, p] of Object.entries(answer.probabilities ?? {})) {
          const v = byLabel.get(label);
          if (v !== undefined) byValue.set(v, p);
        }
        if (!byValue.has(value)) byValue.set(value, answer.confidence ?? 1);
        const ranked = [...byValue.entries()].sort((a, b) => b[1] - a[1]).map(([v]) => v);
        const probabilities = Object.fromEntries(byValue);
        out[id] = {
          value,
          label: answer.choice,
          confidence: answer.confidence ?? byValue.get(value),
          probabilities,
          ranked,
        };
        break;
      }
      case "score": {
        const levels = prepared.levels[id]!;
        const index = Math.max(0, Math.min(levels.length - 1, Math.round(answer.score)));
        out[id] = {
          score: answer.score,
          level: index,
          level_text: levels[index],
          confidence: answer.confidence ?? null,
          probabilities: answer.probabilities ?? {},
        };
        break;
      }
      case "noul":
        out[id] = { yes: answer.noul, answer: answer.noul >= 0.5 };
        break;
    }
  }
  return { ok: true, value: out };
}

function isRetryable(error: unknown): boolean {
  return (
    error instanceof Error &&
    "retryable" in error &&
    (error as Error & { retryable: unknown }).retryable === true
  );
}

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function registerDecideBlock(): void {
  registerBlock<DecideConfig>({
    type: "decide",
    requiresRouter: "openrouter",
    outputs: ["decided", "reason", "model", "answers", "cost"],
    configSchema: decideConfigSchema,
    async execute(config, ctx): Promise<BlockOutcome> {
      const runtime = ctx.deps.decisions;
      if (!runtime) {
        return {
          kind: "error",
          code: "decisions_runtime_unavailable",
          message: "the decisions runtime is not installed",
          retryable: false,
        };
      }
      const prepared = prepareQuestions(config.questions);
      if (!prepared.ok) {
        return { kind: "error", code: "decide_config_invalid", message: prepared.error, retryable: false };
      }
      if (!(await runtime.connected())) {
        return { kind: "ok", outputs: { decided: false, reason: "router_not_connected" } };
      }
      const request = {
        model: config.model ?? DEFAULT_DECISION_MODEL,
        state: config.state,
        questions: prepared.value.questions,
      };
      let response: DecisionsResponse;
      try {
        try {
          response = await runtime.decide(request);
        } catch (error) {
          if (!isRetryable(error)) throw error;
          response = await runtime.decide(request);
        }
      } catch (error) {
        if (config.onError === "undecided") {
          return { kind: "ok", outputs: { decided: false, reason: `decision_failed: ${message(error)}` } };
        }
        return {
          kind: "error",
          code: "decision_failed",
          message: message(error),
          retryable: isRetryable(error),
        };
      }
      const answers = mapAnswers(prepared.value, response.answers);
      if (!answers.ok) {
        if (config.onError === "undecided") {
          return { kind: "ok", outputs: { decided: false, reason: `decision_failed: ${answers.error}` } };
        }
        return { kind: "error", code: "decision_invalid", message: answers.error, retryable: false };
      }
      return {
        kind: "ok",
        outputs: {
          decided: true,
          model: response.model,
          answers: answers.value,
          cost: response.usage.cost ?? null,
        },
      };
    },
  });
}
