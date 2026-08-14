import { NodeViewWrapper, type NodeViewProps } from "@tiptap/react";

import { Button } from "@/components/ui/button";
import { Text } from "@/components/ui/text";
import { useSectionNodeViewContext } from "./SectionNodeView";

export function OpenQuestionCard({ editor, getPos, node }: NodeViewProps) {
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
              Raised in this section
            </Text>
          </span>
          <Button
            type="button"
            variant="outline"
            size="xs"
            onClick={() =>
              editor.commands.focus(typeof getPos === "function" ? getPos() : undefined, {
                scrollIntoView: false,
              })
            }
          >
            Take it
          </Button>
        </>
      )}
    </NodeViewWrapper>
  );
}
