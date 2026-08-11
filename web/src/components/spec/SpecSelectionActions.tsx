import { useEffect, useState } from "react";
import type { Editor } from "@tiptap/core";
import type { Node as ProseMirrorNode } from "@tiptap/pm/model";
import { BubbleMenu } from "@tiptap/react/menus";
import {
  createSectionRelativeAnchor,
  isRangeInSectionBody,
  selectionSliceFingerprint,
  serializeSectionRelativeAnchor,
  type SpecSelectionAction,
  type SpecSelectionActionPayload,
  type SpecSelectionSpan,
} from "@engrams/spec-document";
import type * as Y from "yjs";

import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";

const PRESET_INSTRUCTIONS: Readonly<Record<Exclude<SpecSelectionAction, "custom">, string>> = {
  refine: "Refine this passage.",
  wrong: "This passage is wrong.",
  cut: "Remove this passage.",
  ask: "Answer in chat about this passage.",
};

export interface SpecSelectionActions {
  onAction: (payload: SpecSelectionActionPayload) => void;
}

export function SpecSelectionBubbleMenu({
  editor,
  doc,
  specId,
  revision,
  actions,
}: {
  editor: Editor;
  doc: Y.Doc;
  specId: string;
  revision: string;
  actions: SpecSelectionActions;
}) {
  const [selection, setSelection] = useState<SpecSelectionSpan | null>(() =>
    currentSelection(editor, doc, specId, revision),
  );

  useEffect(() => {
    const update = () => {
      const next = currentSelection(editor, doc, specId, revision);
      setSelection((current) => (sameSelection(current, next) ? current : next));
    };
    editor.on("selectionUpdate", update);
    editor.on("transaction", update);
    return () => {
      editor.off("selectionUpdate", update);
      editor.off("transaction", update);
    };
  }, [doc, editor, revision, specId]);

  if (!selection) return null;
  return (
    <BubbleMenu
      editor={editor}
      pluginKey="spec-selection-actions"
      updateDelay={0}
      options={{ placement: "top" }}
      shouldShow={() => currentSelection(editor, doc, specId, revision) !== null}
    >
      <SpecSelectionMenu specId={specId} selection={selection} onAction={actions.onAction} />
    </BubbleMenu>
  );
}

export function SpecSelectionMenu({
  specId,
  selection,
  onAction,
}: {
  specId: string;
  selection: SpecSelectionSpan | null;
  onAction: (payload: SpecSelectionActionPayload) => void;
}) {
  const [instruction, setInstruction] = useState("");
  if (!selection) return null;

  const submit = (action: SpecSelectionAction, text: string) => {
    onAction({ specId, action, instruction: text, span: selection });
    setInstruction("");
  };

  return (
    <div role="dialog" aria-label="Actions for selected text" className="spec-selection-menu">
      <div className="spec-selection-presets" aria-label="Selection actions">
        <Button
          type="button"
          size="xs"
          variant="ghost"
          onClick={() => submit("refine", PRESET_INSTRUCTIONS.refine)}
        >
          Refine
        </Button>
        <Button
          type="button"
          size="xs"
          variant="ghost"
          onClick={() => submit("wrong", PRESET_INSTRUCTIONS.wrong)}
        >
          This is wrong…
        </Button>
        <Button
          type="button"
          size="xs"
          variant="ghost"
          onClick={() => submit("cut", PRESET_INSTRUCTIONS.cut)}
        >
          Cut
        </Button>
        <Button
          type="button"
          size="xs"
          variant="ghost"
          onClick={() => submit("ask", PRESET_INSTRUCTIONS.ask)}
        >
          Ask about this
        </Button>
      </div>
      <form
        className="spec-selection-custom"
        onSubmit={(event) => {
          event.preventDefault();
          const value = instruction.trim();
          if (value.length > 0) submit("custom", value);
        }}
      >
        <Textarea
          rows={2}
          value={instruction}
          aria-label="Custom selection instruction"
          placeholder="Tell the agent what to change…"
          onChange={(event) => setInstruction(event.target.value)}
        />
        <Button type="submit" size="sm" disabled={instruction.trim().length === 0}>
          Send
        </Button>
      </form>
    </div>
  );
}

export function createSpecSelectionSpan(
  document: ProseMirrorNode,
  ydoc: Y.Doc,
  from: number,
  to: number,
  specId: string,
  revision: string,
): SpecSelectionSpan | null {
  if (from >= to) return null;
  let sectionId: string | null = null;
  document.forEach((node, position) => {
    if (
      sectionId === null &&
      node.type.name === "section" &&
      typeof node.attrs.id === "string" &&
      from > position &&
      to < position + node.nodeSize
    ) {
      sectionId = node.attrs.id;
    }
  });
  if (sectionId === null || !isRangeInSectionBody(document, sectionId, from, to)) return null;
  const selectedText = document.textBetween(from, to, "\n");
  if (selectedText.trim().length === 0) return null;
  return {
    specId,
    sectionId,
    revision,
    startAnchor: serializeSectionRelativeAnchor(createSectionRelativeAnchor(ydoc, sectionId, from)),
    endAnchor: serializeSectionRelativeAnchor(createSectionRelativeAnchor(ydoc, sectionId, to)),
    selectedText,
    sliceFingerprint: selectionSliceFingerprint(document, from, to),
  };
}

function currentSelection(
  editor: Editor,
  doc: Y.Doc,
  specId: string,
  revision: string,
): SpecSelectionSpan | null {
  const { from, to } = editor.state.selection;
  return createSpecSelectionSpan(editor.state.doc, doc, from, to, specId, revision);
}

export function sameSelection(
  left: SpecSelectionSpan | null,
  right: SpecSelectionSpan | null,
): boolean {
  if (left === right) return true;
  if (left === null || right === null) return false;
  return (
    left.specId === right.specId &&
    left.sectionId === right.sectionId &&
    left.revision === right.revision &&
    left.startAnchor === right.startAnchor &&
    left.endAnchor === right.endAnchor &&
    left.selectedText === right.selectedText &&
    left.sliceFingerprint === right.sliceFingerprint
  );
}
