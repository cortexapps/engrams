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
