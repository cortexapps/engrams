import { z } from "zod";

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

/** Register tools that every production session receives. */
export function registerBuiltinTools(registry: ToolRegistry = tools): void {
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
}
