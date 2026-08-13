import { sha256 } from "@noble/hashes/sha2.js";
import { bytesToHex } from "@noble/hashes/utils.js";
import type { Node as ProseMirrorNode, ResolvedPos } from "prosemirror-model";

export type SectionState = "open" | "proposed" | "settled" | "n/a";

export interface SectionStateValue {
  state: SectionState;
  naReason: string | null;
}

export interface RestoreSectionStateUndo {
  kind: "restore_section_state";
  specId: string;
  sectionId: string;
  expected: SectionStateValue;
  restore: SectionStateValue;
}

export interface SectionStateTranscriptChip {
  kind: "spec_section_state_changed";
  specId: string;
  sectionId: string;
  sectionTitle: string;
  before: SectionStateValue;
  after: SectionStateValue;
  undo: RestoreSectionStateUndo;
}

export type SpecSelectionAction = "refine" | "wrong" | "cut" | "ask" | "custom";

/** A stable selection that can survive edits outside its Yjs-relative range. */
export interface SpecSelectionSpan {
  specId: string;
  sectionId: string;
  revision: string;
  startAnchor: string;
  endAnchor: string;
  selectedText: string;
  sliceFingerprint: string;
}

/** Role-neutral UI command. The parent decides which users can send it. */
export interface SpecSelectionActionPayload {
  specId: string;
  action: SpecSelectionAction;
  instruction: string;
  span: SpecSelectionSpan;
}

/** Visible record for an agent edit that replaced one selected range. */
export interface TrackedEditTranscriptChip {
  kind: "spec_tracked_edit";
  specId: string;
  sectionId: string;
  before: string;
  after: string;
}

export type SpecTranscriptChip =
  | SectionStateTranscriptChip
  | TrackedEditTranscriptChip;

/** Hash the complete selected slice and its structural boundaries. */
export function selectionSliceFingerprint(
  document: ProseMirrorNode,
  from: number,
  to: number,
): string {
  const slice = document.slice(from, to, true);
  const canonical = canonicalValue({
    version: 1,
    content: slice.content.toJSON(),
    openStart: slice.openStart,
    openEnd: slice.openEnd,
    start: boundaryValue(document.resolve(from)),
    end: boundaryValue(document.resolve(to)),
  });
  return bytesToHex(sha256(new TextEncoder().encode(JSON.stringify(canonical))));
}

function boundaryValue(position: ResolvedPos): object {
  return {
    depth: position.depth,
    atParentStart: position.parentOffset === 0,
    atParentEnd: position.parentOffset === position.parent.content.size,
    insideText: position.textOffset > 0,
    marks: position.marks().map((mark) => mark.toJSON()),
    nodeBefore: boundaryNode(position.nodeBefore),
    nodeAfter: boundaryNode(position.nodeAfter),
    path: Array.from({ length: position.depth + 1 }, (_, depth) => ({
      type: position.node(depth).type.name,
      attrs: position.node(depth).attrs,
    })),
  };
}

function boundaryNode(node: ProseMirrorNode | null): object | null {
  return node
    ? {
        type: node.type.name,
        attrs: node.attrs,
        marks: node.marks.map((mark) => mark.toJSON()),
        isText: node.isText,
      }
    : null;
}

function canonicalValue(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonicalValue);
  if (value !== null && typeof value === "object") {
    return Object.fromEntries(
      Object.keys(value)
        .sort()
        .map((key) => [key, canonicalValue(Reflect.get(value, key))]),
    );
  }
  return value;
}
