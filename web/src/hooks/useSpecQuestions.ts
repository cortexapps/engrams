import { useMutation, useQueryClient } from "@tanstack/react-query";

import { specRequest } from "@/lib/spec-api";
import { specPublishKey } from "./useSpecPublish";

interface QuestionResponse {
  question: {
    id: string;
    sectionId: string;
    text: string;
    state: string;
    resolvedBy: string | null;
  };
}

/**
 * The human exits for an open question. Resolve writes the answer into the
 * section at the question's anchor; dismiss closes a question that no longer
 * applies. Before these, a person had no way to act on a question at all.
 */
export function useResolveSpecQuestion(specId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (input: { questionId: string; answer: string }) =>
      specRequest<QuestionResponse>(
        `/specs/${encodeURIComponent(specId)}/questions/${encodeURIComponent(input.questionId)}/resolve`,
        {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ answer: input.answer }),
        },
      ),
    onSettled: () => invalidateQuestionReaders(queryClient, specId),
  });
}

export function useDismissSpecQuestion(specId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (questionId: string) =>
      specRequest<QuestionResponse>(
        `/specs/${encodeURIComponent(specId)}/questions/${encodeURIComponent(questionId)}/dismiss`,
        { method: "POST", headers: { "content-type": "application/json" }, body: "{}" },
      ),
    onSettled: () => invalidateQuestionReaders(queryClient, specId),
  });
}

function invalidateQuestionReaders(queryClient: ReturnType<typeof useQueryClient>, specId: string) {
  void queryClient.invalidateQueries({ queryKey: specPublishKey(specId) });
  void queryClient.invalidateQueries({ queryKey: ["spec", specId, "rail"] });
}
