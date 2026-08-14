import { describe, expect, test } from "bun:test";

import {
  applyHumanSectionEdit,
  applySectionStateUndo,
  SectionStateTransitionError,
  transitionSectionState,
  type SectionState,
  type SectionStateContext,
  type SectionStateValue,
} from "../section-state.ts";

const context: SectionStateContext = {
  specId: "00000000-0000-4000-8000-000000000001",
  sectionId: "failure-modes",
  sectionTitle: "Failure modes",
  allowsNa: true,
};

const values: Record<SectionState, SectionStateValue> = {
  open: { state: "open", naReason: null },
  proposed: { state: "proposed", naReason: null },
  settled: { state: "settled", naReason: null },
  "n/a": { state: "n/a", naReason: "The change has no data migration." },
};

const legalTransitions: ReadonlyArray<readonly [SectionState, SectionState]> = [
  ["open", "proposed"],
  ["open", "n/a"],
  ["proposed", "settled"],
  ["proposed", "open"],
  ["proposed", "n/a"],
  ["settled", "proposed"],
  ["settled", "n/a"],
  ["n/a", "proposed"],
];

describe("section state machine", () => {
  for (const [from, to] of legalTransitions) {
    test(`allows ${from} to ${to} and returns an undo chip`, () => {
      const change = transitionSectionState(
        values[from],
        to,
        context,
        to === "n/a" ? "  The change has no data migration.  " : undefined,
      );

      expect(change.value).toEqual(values[to]);
      expect(change.transcriptChip).toMatchObject({
        kind: "spec_section_state_changed",
        before: values[from],
        after: values[to],
        undo: {
          kind: "restore_section_state",
          expected: values[to],
          restore: values[from],
        },
      });
    });
  }

  test("rejects every other direct transition", () => {
    for (const from of Object.keys(values) as SectionState[]) {
      for (const to of Object.keys(values) as SectionState[]) {
        if (legalTransitions.some((pair) => pair[0] === from && pair[1] === to)) continue;
        expect(() =>
          transitionSectionState(
            values[from],
            to,
            context,
            to === "n/a" ? "Not applicable." : undefined,
          ),
        ).toThrow(SectionStateTransitionError);
      }
    }
  });

  test("rejects n/a without a reason", () => {
    expect(() => transitionSectionState(values.open, "n/a", context)).toThrow(
      "must have a reason",
    );
  });

  test("rejects n/a when the template does not allow it", () => {
    expect(() =>
      transitionSectionState(values.open, "n/a", { ...context, allowsNa: false }, "No change."),
    ).toThrow("does not allow");
  });

  test("a human edit proposes open, settled, and n/a sections", () => {
    for (const state of ["open", "settled", "n/a"] as const) {
      expect(applyHumanSectionEdit(values[state], context)?.value).toEqual(values.proposed);
    }
    expect(applyHumanSectionEdit(values.proposed, context)).toBeNull();
  });

  test("keeps the direct open-to-settled transition illegal", () => {
    expect(() => transitionSectionState(values.open, "settled", context)).toThrow(
      SectionStateTransitionError,
    );
  });

  test("undo restores the prior state and rejects a stale action", () => {
    const change = transitionSectionState(values.proposed, "settled", context);
    const undoChange = applySectionStateUndo(change.value, change.transcriptChip.undo, context);
    expect(undoChange.value).toEqual(values.proposed);
    expect(() => applySectionStateUndo(values.open, change.transcriptChip.undo, context)).toThrow(
      "does not match",
    );
  });
});
