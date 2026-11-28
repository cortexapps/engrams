import { expect, test } from "bun:test";
import {
  assertHistoryPreserved,
  cancellationRequest,
  requireKillVariants,
  assertKillPublication,
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
test("only real orchestrator switches satisfy mandatory kill prerequisites", () => {
  const complete = {
    configurations: {
      orchestrator: { normal: {}, "global-disabled": {}, "zone-disabled": {} },
      coordinator: { normal: {} },
    },
    operatorCandidates: { normal: {} },
  };
  expect(() => requireKillVariants(complete)).not.toThrow();
  const wrongRole = {
    configurations: {
      orchestrator: { normal: {} },
      coordinator: { normal: {}, "global-disabled": {}, "zone-disabled": {} },
    },
    operatorCandidates: {
      normal: {},
      "global-disabled": {},
      "zone-disabled": {},
    },
  };
  expect(() => requireKillVariants(wrongRole)).toThrow("NEEDS_CONTEXT");
  expect(() =>
    requireKillVariants({
      configurations: { orchestrator: { normal: {} } },
    }),
  ).toThrow("NEEDS_CONTEXT");
});
test("kill cannot republish or change the actual operator generation", () => {
  const before = {
    admission_open: true,
    pending: null,
    published: {
      generation: 1,
      identity: `sha256:${"a".repeat(64)}`,
      candidate: {
        deployment_identity: "deployment",
        network_identity: "network",
        trust_identity: "trust",
      },
    },
  };
  expect(() =>
    assertKillPublication(before, structuredClone(before)),
  ).not.toThrow();
  expect(() =>
    assertKillPublication(before, {
      ...before,
      published: { ...before.published, generation: 2 },
    }),
  ).toThrow();
  expect(() =>
    assertKillPublication(before, { ...before, admission_open: false }),
  ).toThrow();
});
