import { describe, expect, test } from "bun:test";

import {
  renderAlternativesConsidered,
  SpecAlternativesError,
  validateSpecAlternatives,
  type AlternativesDecidedTranscriptChip,
  type AlternativesProposedTranscriptChip,
  type SpecAlternativeOption,
} from "./alternatives.ts";

const SPEC_ID = "00000000-0000-4000-8000-0000000000a1";

function option(key: string, title: string): SpecAlternativeOption {
  return {
    key,
    title,
    tradeoffs: [
      { sign: "+", text: `${key} gains` },
      { sign: "-", text: `${key} costs` },
      { sign: "~", text: `${key} caveat` },
    ],
  };
}

function proposal(
  options = [option("A", "Second org-level bucket"), option("B", "Hierarchical limiter")],
): AlternativesProposedTranscriptChip {
  return {
    kind: "spec_alternatives_proposed",
    specId: SPEC_ID,
    sectionId: "alternatives",
    setId: "set-1",
    options,
    comparison: {
      provenance: "verified against gateway/limits.rs @ 8f2c1a4",
      rows: [
        {
          axis: "Blast radius",
          cells: options.map((candidate, index) => ({
            optionKey: candidate.key,
            value: `${index + 2} files`,
          })),
        },
      ],
    },
    leanKey: "B",
  };
}

function decision(
  pickedKey: string | null = "B",
  reason = "One code path, and the team tier drops out free later.",
): AlternativesDecidedTranscriptChip {
  return {
    kind: "spec_alternatives_decided",
    specId: SPEC_ID,
    sectionId: "alternatives",
    setId: "set-1",
    pickedKey,
    reason,
    decidedBy: "author",
  };
}

describe("validateSpecAlternatives", () => {
  test("accepts two or three cards with three trade-off lines each", () => {
    expect(() => validateSpecAlternatives(proposal())).not.toThrow();
    expect(() =>
      validateSpecAlternatives(
        proposal([option("A", "A"), option("B", "B"), option("C", "C")]),
      ),
    ).not.toThrow();
  });

  test("refuses one card and refuses four", () => {
    expect(() => validateSpecAlternatives(proposal([option("A", "A")]))).toThrow(
      SpecAlternativesError,
    );
    expect(() =>
      validateSpecAlternatives(
        proposal([option("A", "A"), option("B", "B"), option("C", "C"), option("D", "D")]),
      ),
    ).toThrow(SpecAlternativesError);
  });

  test("refuses a card that does not carry exactly three trade-off lines", () => {
    const short = option("A", "Second bucket");
    short.tradeoffs = short.tradeoffs.slice(0, 2);
    expect(() => validateSpecAlternatives(proposal([short, option("B", "B")]))).toThrow(
      /exactly 3 trade-off lines/,
    );
  });

  test("refuses a comparison axis that misses an option", () => {
    const value = proposal();
    value.comparison.rows[0]!.cells = [{ optionKey: "A", value: "2 files" }];
    expect(() => validateSpecAlternatives(value)).toThrow(/one value for every option/);
  });

  test("refuses a lean, or an axis, that names an unknown option", () => {
    const unknownLean = proposal();
    unknownLean.leanKey = "Z";
    expect(() => validateSpecAlternatives(unknownLean)).toThrow(/unknown option: Z/);

    const unknownCell = proposal();
    unknownCell.comparison.rows[0]!.cells = [
      { optionKey: "A", value: "2 files" },
      { optionKey: "Z", value: "3 files" },
    ];
    expect(() => validateSpecAlternatives(unknownCell)).toThrow(/unknown option: Z/);
  });

  test("refuses duplicate card keys and an empty provenance caption", () => {
    expect(() =>
      validateSpecAlternatives(proposal([option("A", "First"), option("A", "Second")])),
    ).toThrow(/used twice/);

    const blank = proposal();
    blank.comparison.provenance = "  ";
    expect(() => validateSpecAlternatives(blank)).toThrow(/provenance caption/);
  });
});

describe("renderAlternativesConsidered", () => {
  test("writes every option, the comparison, and the reason the winner won", () => {
    const set = proposal([
      option("A", "Second org-level bucket"),
      option("B", "Hierarchical limiter"),
      option("C", "Admission control at the scheduler"),
    ]);
    const markdown = renderAlternativesConsidered(set, decision());

    for (const candidate of set.options) {
      expect(markdown).toContain(`### ${candidate.key} · ${candidate.title}`);
      for (const tradeoff of candidate.tradeoffs) expect(markdown).toContain(tradeoff.text);
    }
    expect(markdown).toContain("verified against gateway/limits.rs @ 8f2c1a4");
    expect(markdown).toContain("### Selected: B · Hierarchical limiter");
    expect(markdown).toContain("One code path, and the team tier drops out free later.");
  });

  test("names a hybrid when no card won", () => {
    const markdown = renderAlternativesConsidered(
      proposal(),
      decision(null, "The author took A's meter path with B's walk."),
    );
    expect(markdown).toContain("### Selected: a hybrid");
    expect(markdown).toContain("The author took A's meter path with B's walk.");
  });

  test("refuses a decision that names an option the set does not hold", () => {
    expect(() => renderAlternativesConsidered(proposal(), decision("Z"))).toThrow(
      SpecAlternativesError,
    );
  });
});
