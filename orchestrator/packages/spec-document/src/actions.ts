export type SectionState = "empty" | "drafted" | "confirmed" | "n/a";

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
  provisional: boolean;
  undo: RestoreSectionStateUndo;
}

export type SpecSelectionAction = "refine" | "wrong" | "cut" | "ask" | "custom";

/** A stable selection that can survive edits outside its Yjs-relative range. */
export interface SpecSelectionSpan {
  sectionId: string;
  startAnchor: string;
  endAnchor: string;
  selectedText: string;
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
