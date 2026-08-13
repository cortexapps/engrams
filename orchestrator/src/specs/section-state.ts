import type {
  RestoreSectionStateUndo,
  SectionState,
  SectionStateTranscriptChip,
  SectionStateValue,
} from "@engrams/spec-document";

export type {
  RestoreSectionStateUndo,
  SectionState,
  SectionStateTranscriptChip,
  SectionStateValue,
} from "@engrams/spec-document";

export const SECTION_STATES = [
  "open",
  "proposed",
  "settled",
  "n/a",
] as const satisfies readonly SectionState[];

export interface SectionStateContext {
  specId: string;
  sectionId: string;
  sectionTitle: string;
  allowsNa: boolean;
}

export interface SectionStateChange {
  value: SectionStateValue;
  transcriptChip: SectionStateTranscriptChip;
}

export class SectionStateTransitionError extends Error {
  constructor(
    readonly code:
      | "illegal_transition"
      | "na_not_allowed"
      | "na_reason_required"
      | "na_reason_forbidden"
      | "stale_undo",
    message: string,
  ) {
    super(message);
    this.name = "SectionStateTransitionError";
  }
}

const LEGAL_TARGETS: Readonly<Record<SectionState, ReadonlySet<SectionState>>> = {
  open: new Set(["proposed", "n/a"]),
  proposed: new Set(["settled", "open", "n/a"]),
  settled: new Set(["proposed", "n/a"]),
  "n/a": new Set(["proposed"]),
};

function normalizedValue(state: SectionState, naReason?: string | null): SectionStateValue {
  const reason = naReason?.trim() || null;
  if (state === "n/a") {
    if (reason == null) {
      throw new SectionStateTransitionError(
        "na_reason_required",
        "A section in the n/a state must have a reason.",
      );
    }
    return { state, naReason: reason };
  }
  if (reason != null) {
    throw new SectionStateTransitionError(
      "na_reason_forbidden",
      "Only a section in the n/a state can have an n/a reason.",
    );
  }
  return { state, naReason: null };
}

function equalState(left: SectionStateValue, right: SectionStateValue): boolean {
  return left.state === right.state && left.naReason === right.naReason;
}

function chipFor(
  context: SectionStateContext,
  before: SectionStateValue,
  after: SectionStateValue,
): SectionStateTranscriptChip {
  return {
    kind: "spec_section_state_changed",
    specId: context.specId,
    sectionId: context.sectionId,
    sectionTitle: context.sectionTitle,
    before,
    after,
    undo: {
      kind: "restore_section_state",
      specId: context.specId,
      sectionId: context.sectionId,
      expected: after,
      restore: before,
    },
  };
}

/** Apply a person or agent state action and return its required transcript chip. */
export function transitionSectionState(
  current: SectionStateValue,
  target: SectionState,
  context: SectionStateContext,
  naReason?: string | null,
): SectionStateChange {
  if (target === "n/a" && !context.allowsNa) {
    throw new SectionStateTransitionError(
      "na_not_allowed",
      `Section ${context.sectionTitle} does not allow the n/a state.`,
    );
  }

  const next = normalizedValue(target, naReason);
  if (!LEGAL_TARGETS[current.state].has(target)) {
    throw new SectionStateTransitionError(
      "illegal_transition",
      `A section cannot move from ${current.state} to ${target}.`,
    );
  }

  return { value: next, transcriptChip: chipFor(context, current, next) };
}

/**
 * Apply a human document edit. A first edit proposes an open section. An edit
 * also proposes settled or n/a content because the old decision is stale.
 */
export function applyHumanSectionEdit(
  current: SectionStateValue,
  context: SectionStateContext,
): SectionStateChange | null {
  if (current.state === "proposed") return null;
  return transitionSectionState(current, "proposed", context);
}

/** Apply an undo only if no later state action changed the section. */
export function applySectionStateUndo(
  current: SectionStateValue,
  undo: RestoreSectionStateUndo,
  context: SectionStateContext,
): SectionStateChange {
  if (
    context.specId !== undo.specId ||
    context.sectionId !== undo.sectionId ||
    !equalState(current, undo.expected)
  ) {
    throw new SectionStateTransitionError(
      "stale_undo",
      "This undo action does not match the current section state.",
    );
  }

  const restored = normalizedValue(undo.restore.state, undo.restore.naReason);
  return { value: restored, transcriptChip: chipFor(context, current, restored) };
}
