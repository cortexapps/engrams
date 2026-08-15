import { useState } from "react";
import { NodeViewWrapper, type NodeViewProps } from "@tiptap/react";

import { Button } from "@/components/ui/button";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Text } from "@/components/ui/text";
import { Textarea } from "@/components/ui/textarea";
import { useSectionNodeViewContext } from "./SectionNodeView";

export function OpenQuestionCard({ node }: NodeViewProps) {
  const context = useSectionNodeViewContext();
  const questionId = String(node.attrs.questionId);
  const question = context?.surface.openQuestions.find((candidate) => candidate.id === questionId);
  const resolved = question?.resolved ?? true;
  return (
    <NodeViewWrapper
      as="span"
      className="spec-mode-open-question"
      data-question-id={questionId}
      data-resolved={resolved ? "true" : undefined}
      contentEditable={false}
    >
      {resolved ? null : (
        <>
          <span className="spec-mode-open-question-flag" aria-hidden="true">
            ⚑
          </span>
          <span className="spec-mode-open-question-copy">
            <Text as="span" variant="body">
              {question?.text ?? "Open question"}
            </Text>
            <Text as="span" variant="code" tone="muted">
              Open question
            </Text>
          </span>
          {context?.questionActions ? (
            <span className="spec-mode-open-question-actions">
              <ResolveQuestionAction
                questionId={questionId}
                onResolve={context.questionActions.resolve}
              />
              <Button
                type="button"
                variant="ghost"
                size="xs"
                onClick={() => context.questionActions?.dismiss(questionId)}
              >
                Dismiss
              </Button>
            </span>
          ) : null}
        </>
      )}
    </NodeViewWrapper>
  );
}

/** Answer the question; the answer lands in the section at this anchor. */
function ResolveQuestionAction({
  questionId,
  onResolve,
}: {
  questionId: string;
  onResolve: (questionId: string, answer: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [answer, setAnswer] = useState("");
  const valid = answer.trim().length > 0;
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button type="button" variant="outline" size="xs">
          Answer
        </Button>
      </PopoverTrigger>
      <PopoverContent align="start" className="spec-mode-question-answer">
        <Text as="label" variant="label" htmlFor={`answer-${questionId}`}>
          Your answer joins the section here and closes the question.
        </Text>
        <Textarea
          id={`answer-${questionId}`}
          value={answer}
          onChange={(event) => setAnswer(event.target.value)}
          placeholder="State the decision"
        />
        <Button
          type="button"
          size="sm"
          disabled={!valid}
          onClick={() => {
            if (!valid) return;
            onResolve(questionId, answer.trim());
            setAnswer("");
            setOpen(false);
          }}
        >
          Answer and close
        </Button>
      </PopoverContent>
    </Popover>
  );
}
