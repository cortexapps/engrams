import { describe, expect, test } from "bun:test";

import {
  evaluatePublishGate,
  sectionIsSettled,
  type PublishGateQuestion,
  type PublishGateSection,
} from "./publish-gate.ts";

function section(
  overrides: Partial<PublishGateSection> & Pick<PublishGateSection, "id">,
): PublishGateSection {
  return {
    title: overrides.title ?? overrides.id,
    layerKey: "intent",
    required: true,
    state: "confirmed",
    naReason: null,
    ...overrides,
  };
}

function question(
  overrides: Partial<PublishGateQuestion> & Pick<PublishGateQuestion, "id">,
): PublishGateQuestion {
  return {
    sectionId: "sec-data",
    sectionTitle: "Data model",
    text: "Do banked burst credits survive a plan downgrade?",
    ...overrides,
  };
}

describe("publish gate", () => {
  test("a spec whose required sections are confirmed is ready", () => {
    const gate = evaluatePublishGate({
      sections: [section({ id: "sec-problem" }), section({ id: "sec-data" })],
      openQuestions: [],
      gapCheckStale: false,
    });

    expect(gate).toEqual({
      ready: true,
      blockers: [],
      settledRequiredCount: 2,
      requiredCount: 2,
      openQuestions: [],
      acknowledgmentRequired: false,
      gapCheckRunRequired: false,
    });
  });

  test("an unconfirmed required section blocks and names its section (R34)", () => {
    const gate = evaluatePublishGate({
      sections: [
        section({ id: "sec-problem" }),
        section({ id: "sec-data", title: "Data model", state: "drafted" }),
        section({ id: "sec-api", title: "API surface", state: "empty", layerKey: "contract" }),
      ],
      openQuestions: [],
      gapCheckStale: false,
    });

    expect(gate.ready).toBe(false);
    expect(gate.settledRequiredCount).toBe(1);
    expect(gate.requiredCount).toBe(3);
    expect(gate.blockers).toEqual([
      {
        sectionId: "sec-data",
        sectionTitle: "Data model",
        layerKey: "intent",
        state: "drafted",
        reason: "drafted",
      },
      {
        sectionId: "sec-api",
        sectionTitle: "API surface",
        layerKey: "contract",
        state: "empty",
        reason: "empty",
      },
    ]);
  });

  test("n/a with a reason settles a required section, and n/a alone does not", () => {
    const withReason = evaluatePublishGate({
      sections: [section({ id: "sec-perf", state: "n/a", naReason: "No user-visible latency." })],
      openQuestions: [],
      gapCheckStale: false,
    });
    expect(withReason.ready).toBe(true);
    expect(withReason.settledRequiredCount).toBe(1);

    const withoutReason = evaluatePublishGate({
      sections: [
        section({ id: "sec-perf", title: "Performance", state: "n/a", naReason: "   " }),
      ],
      openQuestions: [],
      gapCheckStale: false,
    });
    expect(withoutReason.ready).toBe(false);
    expect(withoutReason.blockers[0]?.reason).toBe("na_without_reason");
  });

  test("an optional section never blocks, whatever its state", () => {
    const gate = evaluatePublishGate({
      sections: [
        section({ id: "sec-problem" }),
        section({ id: "sec-notes", required: false, state: "empty" }),
      ],
      openQuestions: [],
      gapCheckStale: false,
    });

    expect(gate.ready).toBe(true);
    expect(gate.requiredCount).toBe(1);
  });

  test("open questions do not block, but they need an acknowledgment (R35)", () => {
    const gate = evaluatePublishGate({
      sections: [section({ id: "sec-problem" })],
      openQuestions: [question({ id: "q-1" }), question({ id: "q-2" }), question({ id: "q-3" })],
      gapCheckStale: false,
    });

    expect(gate.ready).toBe(true);
    expect(gate.acknowledgmentRequired).toBe(true);
    expect(gate.openQuestions).toHaveLength(3);
  });

  test("a blocked gate asks for blockers, not for an acknowledgment", () => {
    const gate = evaluatePublishGate({
      sections: [section({ id: "sec-data", state: "drafted" })],
      openQuestions: [question({ id: "q-1" })],
      gapCheckStale: true,
    });

    expect(gate.ready).toBe(false);
    expect(gate.acknowledgmentRequired).toBe(false);
    expect(gate.gapCheckRunRequired).toBe(false);
  });

  test("a stale gap check makes the run part of the publish (R30)", () => {
    const gate = evaluatePublishGate({
      sections: [section({ id: "sec-problem" })],
      openQuestions: [],
      gapCheckStale: true,
    });

    expect(gate.ready).toBe(true);
    expect(gate.gapCheckRunRequired).toBe(true);
  });

  test("sectionIsSettled reads confirmed and n/a with a reason", () => {
    expect(sectionIsSettled(section({ id: "a", state: "confirmed" }))).toBe(true);
    expect(sectionIsSettled(section({ id: "a", state: "n/a", naReason: "Out of scope." }))).toBe(
      true,
    );
    expect(sectionIsSettled(section({ id: "a", state: "n/a" }))).toBe(false);
    expect(sectionIsSettled(section({ id: "a", state: "drafted" }))).toBe(false);
    expect(sectionIsSettled(section({ id: "a", state: "empty" }))).toBe(false);
  });
});
