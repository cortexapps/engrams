import { describe, expect, test } from "bun:test";

import {
  buildWorkingNotesDocument,
  distillWorkingNotes,
  mergeWorkingNotes,
  readWorkingNotes,
  SPEC_NOTE_MARK_GLYPHS,
  SPEC_NOTES_MAX_CLUSTERS,
  SpecWorkingNotesError,
  untaggedBulletCount,
  validateWorkingNotes,
  workingNotesFromInput,
  type SpecWorkingNotes,
  type SpecWorkingNotesInput,
} from "./notes.ts";

const BEHAVIOR = "11111111-1111-4111-8111-111111111111";
const API = "22222222-2222-4222-8222-222222222222";

function input(): SpecWorkingNotesInput {
  return {
    clusters: [
      {
        id: "burst",
        theme: "burst semantics",
        sectionIds: [BEHAVIOR],
        bullets: [
          {
            id: "b1",
            mark: "verified",
            kind: "observation",
            text: "orgs get a burst credit pool, refills daily",
            provenance: "your call, firm",
          },
          {
            id: "b2",
            mark: "contradicted",
            kind: "observation",
            text: "the limiter already handles bursts",
            provenance: "limits.rs @ 8f2c1a4: per-user bucket only",
          },
        ],
      },
      {
        id: "refusal",
        theme: "refusal path",
        sectionIds: [BEHAVIOR, API],
        bullets: [
          {
            id: "b3",
            mark: "verified",
            kind: "requirement",
            text: "refusal names org and reset time, never a bare 429",
            provenance: "agreed in conversation",
          },
        ],
      },
      {
        id: "pile",
        theme: "untagged",
        bullets: [
          { id: "b4", mark: "unchecked", kind: "question", text: "org-hour granularity?" },
          { id: "b5", mark: "unchecked", kind: "tension", text: "meter feeds the autoscaler too?" },
        ],
      },
    ],
  };
}

function notes(): SpecWorkingNotes {
  return workingNotesFromInput(input());
}

describe("working notes validation", () => {
  test("accepts the stage's shape and defaults an untagged cluster", () => {
    const value = notes();
    validateWorkingNotes(value);
    expect(value.clusters[2]!.sectionIds).toEqual([]);
    expect(value.clusters[0]!.bullets[0]!.agentText).toBe(
      "orgs get a burst credit pool, refills daily",
    );
  });

  test("a verified or contradicted bullet must name its receipt", () => {
    for (const mark of ["verified", "contradicted"] as const) {
      const value = notes();
      value.clusters[0]!.bullets[0]!.mark = mark;
      value.clusters[0]!.bullets[0]!.provenance = null;
      expect(() => validateWorkingNotes(value)).toThrow(SpecWorkingNotesError);
    }
  });

  test("an unchecked bullet needs no receipt", () => {
    const value = notes();
    expect(value.clusters[2]!.bullets[0]!.provenance).toBeNull();
    validateWorkingNotes(value);
  });

  test("bullet ids are unique across every cluster", () => {
    const value = notes();
    value.clusters[1]!.bullets[0]!.id = "b1";
    expect(() => validateWorkingNotes(value)).toThrow(/used twice/);
  });

  test("text stays one line and cannot smuggle an open-question marker", () => {
    const withBreak = notes();
    withBreak.clusters[0]!.bullets[0]!.text = "two\nlines";
    expect(() => validateWorkingNotes(withBreak)).toThrow(/one line/);
    const withMarker = notes();
    withMarker.clusters[0]!.bullets[0]!.text = "see {{open-question:x}}";
    expect(() => validateWorkingNotes(withMarker)).toThrow(/open-question marker/);
  });

  test("the cluster ceiling is enforced", () => {
    const value = notes();
    while (value.clusters.length <= SPEC_NOTES_MAX_CLUSTERS) {
      value.clusters.push({
        id: `c${value.clusters.length}`,
        theme: "extra",
        sectionIds: [],
        bullets: [
          {
            id: `x${value.clusters.length}`,
            mark: "unchecked",
            kind: "observation",
            text: "extra",
            provenance: null,
            agentText: "extra",
          },
        ],
      });
    }
    expect(() => validateWorkingNotes(value)).toThrow(/at most/);
  });
});

describe("the notes document", () => {
  test("round-trips through the notes schema", () => {
    const document = buildWorkingNotesDocument(notes());
    document.check();
    expect(readWorkingNotes(document)).toEqual(notes());
  });

  test("the untagged pile is the readiness gauge", () => {
    expect(untaggedBulletCount(notes())).toBe(2);
  });

  test("every mark has a glyph for the canvas", () => {
    expect(SPEC_NOTE_MARK_GLYPHS).toEqual({ verified: "✓", contradicted: "✗", unchecked: "?" });
  });
});

describe("merging an agent replacement over a person's correction", () => {
  test("the correction wins, is reported once, and becomes the new baseline", () => {
    const current = notes();
    current.clusters[0]!.bullets[0]!.text = "orgs get a burst credit pool, refills weekly";
    const first = mergeWorkingNotes(current, notes());
    expect(first.corrections).toEqual([
      {
        bulletId: "b1",
        agentText: "orgs get a burst credit pool, refills daily",
        personText: "orgs get a burst credit pool, refills weekly",
        keptAgainstDrop: false,
      },
    ]);
    expect(first.notes.clusters[0]!.bullets[0]!.text).toBe(
      "orgs get a burst credit pool, refills weekly",
    );
    const second = mergeWorkingNotes(first.notes, notes());
    expect(second.corrections).toEqual([]);
  });

  test("a corrected bullet the agent drops survives in its cluster", () => {
    const current = notes();
    current.clusters[0]!.bullets[0]!.text = "corrected by hand";
    const replacement = notes();
    replacement.clusters[0]!.bullets.splice(0, 1);
    const merged = mergeWorkingNotes(current, replacement);
    expect(merged.corrections[0]!.keptAgainstDrop).toBe(true);
    const kept = merged.notes.clusters[0]!.bullets.map((bullet) => bullet.text);
    expect(kept).toContain("corrected by hand");
  });

  test("a corrected bullet whose cluster is gone falls into the untagged pile", () => {
    const current = notes();
    current.clusters[1]!.bullets[0]!.text = "corrected by hand";
    const replacement = notes();
    replacement.clusters.splice(1, 1);
    const merged = mergeWorkingNotes(current, replacement);
    const pile = merged.notes.clusters.find((cluster) => cluster.sectionIds.length === 0);
    expect(pile?.bullets.map((bullet) => bullet.text)).toContain("corrected by hand");
    expect(untaggedBulletCount(merged.notes)).toBe(3);
  });

  test("an untouched bullet takes the agent's new wording", () => {
    const replacement = notes();
    replacement.clusters[0]!.bullets[0]!.text = "restated by the agent";
    replacement.clusters[0]!.bullets[0]!.agentText = "restated by the agent";
    const merged = mergeWorkingNotes(notes(), replacement);
    expect(merged.corrections).toEqual([]);
    expect(merged.notes.clusters[0]!.bullets[0]!.text).toBe("restated by the agent");
  });
});

describe("distillation", () => {
  test("only tagged clusters produce material, and each is grouped by theme", () => {
    const result = distillWorkingNotes(notes());
    expect(result.sections.map((section) => section.sectionId)).toEqual([BEHAVIOR, API]);
    const behavior = result.sections.find((section) => section.sectionId === BEHAVIOR)!;
    expect(behavior.markdown).toContain("### burst semantics");
    expect(behavior.markdown).toContain("### refusal path");
    expect(behavior.markdown).toContain(
      "Verified — orgs get a burst credit pool, refills daily (your call, firm)",
    );
    expect(behavior.markdown).toContain("Requirement candidate — refusal names org");
  });

  test("a refuted claim never enters the spec but stays counted", () => {
    const result = distillWorkingNotes(notes());
    expect(result.refutedBullets).toBe(1);
    for (const section of result.sections) {
      expect(section.markdown).not.toContain("the limiter already handles bursts");
    }
  });

  test("a destination whose clusters hold only refuted claims stays empty", () => {
    const value = notes();
    value.clusters = [
      {
        ...value.clusters[0]!,
        bullets: [value.clusters[0]!.bullets[1]!],
      },
    ];
    const result = distillWorkingNotes(value);
    expect(result.sections).toEqual([]);
    expect(result.refutedBullets).toBe(1);
  });

  test("untagged bullets are reported, not invented into a section", () => {
    const result = distillWorkingNotes(notes());
    expect(result.untaggedBullets).toBe(2);
    for (const section of result.sections) {
      expect(section.markdown).not.toContain("org-hour granularity");
    }
  });
});
