import { z } from "zod";

import { makePapercutStore, type PapercutStore } from "../db/papercuts.ts";
import { tools, type ToolRegistry } from "./registry.ts";

const QuestionOptionSchema = z.object({
  label: z.string(),
  description: z.string(),
});

const QuestionSchema = z.object({
  question: z.string(),
  header: z.string(),
  multiSelect: z.boolean(),
  options: z.array(QuestionOptionSchema),
});

export const QuestionsSchema = z.object({
  questions: z.array(QuestionSchema),
});

export const AnswersSchema = z.record(z.string(), z.array(z.string()));

export const PlanSchema = z.object({
  plan: z.string().describe("The complete implementation plan, as markdown"),
});

export const PlanDecisionSchema = z.object({
  decision: z.enum(["approve", "reject"]),
  feedback: z.string().optional(),
});

export interface BuiltinToolDeps {
  papercuts: PapercutStore;
}

/** Register tools that every production session receives. */
export function registerBuiltinTools(
  registry: ToolRegistry = tools,
  deps?: BuiltinToolDeps,
): void {
  registry.register({
    name: "ask_user_question",
    description: "Ask the user one or more structured questions.",
    input: QuestionsSchema,
    output: AnswersSchema,
    handling: "session",
    execution: "deferred",
    presenters: {
      slack: "questionEffect",
      web: "UserQuestionCard",
    },
    nativeBindings: {
      claude: "AskUserQuestion",
      codex: "requestUserInput",
    },
  });

  // ADR 0106. Native-bound to claude ExitPlanMode; deliberately NO codex
  // binding — codex and custom harnesses receive it as an injected dynamic
  // tool through the generic deferred path.
  registry.register({
    name: "exit_plan_mode",
    description:
      "Present your finished implementation plan for user approval. " +
      "Call this only in plan mode, with the complete plan as markdown. " +
      "The user approves the plan (then implement it) or rejects it with " +
      "feedback (then revise the plan).",
    input: PlanSchema,
    output: PlanDecisionSchema,
    handling: "session",
    execution: "deferred",
    presenters: {
      slack: "planEffect",
      web: "PlanCard",
    },
    nativeBindings: {
      claude: "ExitPlanMode",
    },
  });

  registry.register({
    name: "papercut",
    description:
      "Log a papercut — a small, concrete friction you hit while working " +
      "(confusing error, missing or awkward tooling, docs gap, slow/flaky command, " +
      "environment quirk you had to work around). One short call, then continue your main task.",
    input: z.object({
      summary: z.string().describe("One-line summary of the friction"),
      description: z.string().describe(
        "What was painful, what you tried, and what would have helped",
      ),
      category: z.enum(["tooling", "environment", "docs", "workflow", "other"]),
      severity: z.enum(["low", "medium", "high"]).optional(),
      tags: z.array(z.string()).optional(),
    }),
    output: z.object({ logged: z.boolean(), id: z.string() }),
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      const id = await (deps?.papercuts ?? makePapercutStore()).insert({
        summary: args.summary,
        description: args.description,
        category: args.category,
        severity: args.severity ?? null,
        tags: args.tags ?? [],
        sessionId: ctx.sessionId,
        toolCallId: ctx.toolCallId,
        taskId: ctx.taskId ?? null,
        profileId: ctx.profileId ?? null,
        userId: ctx.userId ?? null,
      });
      return { logged: true, id };
    },
  });
}
