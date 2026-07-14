import { createContext, useContext } from "react";
import type { QuestionProtocol } from "./buildMessages";

/**
 * ADR 0054: the interactive-question actions, provided by `SessionThread`
 * (which owns the `answerQuestion` mutation + optimistic state) and consumed by
 * the `UserQuestionCard` deep in the assistant-ui tree (rendered via
 * `SystemMessage`). Mirrors `composer-actions.ts` — the mutation hook belongs
 * at the thread level, next to sendPrompt/interrupt.
 */
export interface QuestionActions {
  /**
   * Submit the user's answer to a deferred `AskUserQuestion`. `answers` is
   * keyed by question text (the wire contract — finding #8); each value is the
   * list of selected option labels (1 for single-select, N for multi-select).
   * Fire-and-forget: generic cards use CompleteToolCall and resolve from
   * `tool_result_submitted`; historical cards use AnswerQuestion and resolve
   * from `question_answered`. No-op once already submitted.
   */
  submitAnswer: (
    via: QuestionProtocol,
    toolCallId: string,
    answers: Record<string, string[]>,
  ) => void;
  /**
   * tool_call_ids the user has answered this session but whose authoritative
   * resolving event hasn't landed yet — the card shows its receipt
   * optimistically. This remains useful for the browser response/SSE race and
   * for slower legacy harness confirmation.
   */
  answeredToolCallIds: ReadonlySet<string>;
  /** Terminal session (completed/failed/dead/host_lost) — answering is blocked
   *  (there's no live sandbox to resume into). */
  sendBlocked: boolean;
}

export const QuestionActionsContext = createContext<QuestionActions>({
  submitAnswer: () => {},
  answeredToolCallIds: new Set(),
  sendBlocked: false,
});

export const useQuestionActions = (): QuestionActions => useContext(QuestionActionsContext);
