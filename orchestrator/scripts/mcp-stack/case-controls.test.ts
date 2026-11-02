import { expect, test } from "bun:test";
import {
  assertHistoryPreserved,
  cancellationRequest,
  requireKillVariants,
} from "./case-controls.ts";

test("verified history must retain exact state, result and availability", () => {
  const history = {
    state: "succeeded",
    resultUnavailable: false,
    resultJson: '{"outcome":"succeeded"}',
  };
  expect(() => assertHistoryPreserved(history, history)).not.toThrow();
  for (const after of [
    { ...history, state: "canceled" },
    { ...history, resultUnavailable: true },
    { ...history, resultJson: "{}" },
  ])
    expect(() => assertHistoryPreserved(history, after)).toThrow();
});
test("cancellation uses current exact journal revision", () => {
  expect(
    cancellationRequest({
      executionId: "execution",
      journalRevision: 9,
      state: "dispatching",
    }),
  ).toEqual({ executionId: "execution", expectedJournalRevision: 9 });
  expect(() =>
    cancellationRequest({
      executionId: "execution",
      journalRevision: 0,
      state: "succeeded",
    }),
  ).toThrow();
});
test("disabled variants are mandatory prerequisites, never silently skipped", () => {
  expect(() =>
    requireKillVariants({
      configurations: { coordinator: { normal: {} } },
      operatorCandidates: { normal: {} },
    }),
  ).toThrow("NEEDS_CONTEXT");
});
