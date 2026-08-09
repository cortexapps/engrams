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
  empty: { state: "empty", naReason: null },
  drafted: { state: "drafted", naReason: null },
  confirmed: { state: "confirmed", naReason: null },
  "n/a": { state: "n/a", naReason: "The change has no data migration." },
};

const legalTransitions: ReadonlyArray<readonly [SectionState, SectionState]> = [
  ["empty", "drafted"],
  ["empty", "n/a"],
  ["drafted", "confirmed"],
  ["drafted", "n/a"],
  ["confirmed", "drafted"],
  ["confirmed", "n/a"],
  ["n/a", "drafted"],
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
    expect(() => transitionSectionState(values.empty, "n/a", context)).toThrow(
      "must have a reason",
    );
  });

  test("rejects n/a when the template does not allow it", () => {
    expect(() =>
      transitionSectionState(values.empty, "n/a", { ...context, allowsNa: false }, "No change."),
    ).toThrow("does not allow");
  });

  test("a human edit drafts empty, confirmed, and n/a sections", () => {
    for (const state of ["empty", "confirmed", "n/a"] as const) {
      expect(applyHumanSectionEdit(values[state], context)?.value).toEqual(values.drafted);
    }
    expect(applyHumanSectionEdit(values.drafted, context)).toBeNull();
  });

  test("drafted content is provisional when an upstream section is not confirmed", () => {
    const change = transitionSectionState(values.empty, "drafted", {
      ...context,
      unconfirmedUpstreamSectionIds: ["problem"],
    });
    expect(change.transcriptChip.provisional).toBe(true);
  });

  test("undo restores the prior state and rejects a stale action", () => {
    const change = transitionSectionState(values.drafted, "confirmed", context);
    const undoChange = applySectionStateUndo(change.value, change.transcriptChip.undo, context);
    expect(undoChange.value).toEqual(values.drafted);
    expect(() => applySectionStateUndo(values.empty, change.transcriptChip.undo, context)).toThrow(
      "does not match",
    );
  });
});
