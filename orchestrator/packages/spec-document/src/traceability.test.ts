import { describe, expect, test } from "bun:test";

import {
  analyzeTraceability,
  applyOutsideIn,
  TraceabilityInputError,
  type GapFinding,
  type TraceabilityInput,
  type TraceabilityLayer,
  type TraceabilitySection,
} from "./traceability.ts";

const LAYERS: TraceabilityLayer[] = [
  { key: "intent", title: "Intent" },
  { key: "contract", title: "Contract" },
  { key: "system", title: "System" },
];

function section(
  overrides: Partial<TraceabilitySection> & Pick<TraceabilitySection, "id" | "key" | "layerKey">,
): TraceabilitySection {
  return {
    title: overrides.title ?? overrides.key,
    state: "proposed",
    blocks: [],
    ...overrides,
  };
}

function blocks(...texts: string[]) {
  return texts.map((text, index) => ({ index, text }));
}

/** A spec that cites every requirement in both layers below the ledger. */
function healthyInput(): TraceabilityInput {
  return {
    layers: LAYERS,
    sections: [
      section({
        id: "sec-req",
        key: "requirements",
        title: "Requirements",
        layerKey: "intent",
        blocks: blocks(
          "- R1: an org caps its concurrent sandboxes",
          "- N1: the limiter adds under 1ms p99 to session create",
        ),
      }),
      section({
        id: "sec-behavior",
        key: "behavior",
        title: "Behavior",
        layerKey: "contract",
        blocks: blocks(
          "Creating a sandbox above the org ceiling is refused with a named reset time (R1).",
          "The refusal path is measured on every create call so the budget holds (N1).",
        ),
      }),
      section({
        id: "sec-design",
        key: "design",
        title: "Design",
        layerKey: "system",
        blocks: blocks(
          "A Postgres counter row per org backs the ceiling walk and serves R1 directly.",
          "The walk is timed with a histogram so the N1 budget is observable in production.",
        ),
      }),
    ],
  };
}

describe("analyzeTraceability", () => {
  test("covers every requirement when both layers cite it", () => {
    const result = analyzeTraceability(healthyInput());

    expect(result.findings).toEqual([]);
    expect(result.matrix.layers.map((layer) => layer.key)).toEqual(["contract", "system"]);
    expect(result.matrix.rows).toHaveLength(2);
    for (const row of result.matrix.rows) {
      expect(row.verdict).toBe("covered");
      expect(row.cells.every((cell) => cell.covered)).toBe(true);
      expect(row.cells.every((cell) => cell.note === null)).toBe(true);
    }
    const first = result.matrix.rows[0]!;
    expect(first.requirementId).toBe("R1");
    expect(first.cells[0]!.citations[0]!.sectionId).toBe("sec-behavior");
  });

  test("a requirement with no coverage in a layer is a gap", () => {
    const input = healthyInput();
    const design = input.sections[2]!;
    const result = analyzeTraceability({
      ...input,
      sections: [
        input.sections[0]!,
        input.sections[1]!,
        // Drop the sentence that cited N1 downstream.
        section({ ...design, blocks: blocks(design.blocks[0]!.text) }),
      ],
    });

    const row = result.matrix.rows.find((candidate) => candidate.requirementId === "N1")!;
    expect(row.verdict).toBe("gap");
    const systemCell = row.cells.find((cell) => cell.layerKey === "system")!;
    expect(systemCell.covered).toBe(false);
    expect(systemCell.note).toBe("no System content cites N1");

    const finding = result.findings.find((candidate) => candidate.kind === "requirement_gap")!;
    expect(finding.requirementId).toBe("N1");
    expect(finding.layerKey).toBe("system");
    expect(finding.sectionId).toBe("sec-design");
    expect(finding.severity).toBe("gap");
  });

  test("an uncited behavior is flagged as scope creep", () => {
    const input = healthyInput();
    const behavior = input.sections[1]!;
    const result = analyzeTraceability({
      ...input,
      sections: [
        input.sections[0]!,
        section({
          ...behavior,
          blocks: blocks(
            ...behavior.blocks.map((block) => block.text),
            "An org over its ceiling sees an auto-upgrade prompt before the refusal renders.",
          ),
        }),
        input.sections[2]!,
      ],
    });

    const finding = result.findings.find((candidate) => candidate.kind === "scope_creep")!;
    expect(finding.severity).toBe("gap");
    expect(finding.sectionId).toBe("sec-behavior");
    expect(finding.layerKey).toBe("contract");
    expect(finding.detail).toContain("auto-upgrade prompt");
    expect(finding.proposedDiff).toBeNull();

    const row = result.matrix.rows.find((candidate) => candidate.verdict === "scope")!;
    expect(row.requirementId).toBeNull();
    const cell = row.cells.find((candidate) => candidate.layerKey === "contract")!;
    expect(cell.note).toContain("cites no requirement");
    expect(row.cells.find((candidate) => candidate.layerKey === "system")!.note).toBeNull();
  });

  test("uncited machinery below the behavior layer is speculative, not scope creep", () => {
    const input = healthyInput();
    const design = input.sections[2]!;
    const result = analyzeTraceability({
      ...input,
      sections: [
        input.sections[0]!,
        input.sections[1]!,
        section({
          ...design,
          blocks: blocks(
            ...design.blocks.map((block) => block.text),
            "A Redis replica fans the counter out to every edge region on a five second timer.",
          ),
        }),
      ],
    });

    const finding = result.findings.find(
      (candidate) => candidate.kind === "speculative_machinery",
    )!;
    expect(finding.layerKey).toBe("system");
    expect(finding.detail).toContain("traces to no behavior");
  });

  test("a short block is not substantive enough to flag", () => {
    const input = healthyInput();
    const behavior = input.sections[1]!;
    const result = analyzeTraceability({
      ...input,
      sections: [
        input.sections[0]!,
        section({
          ...behavior,
          blocks: blocks(...behavior.blocks.map((block) => block.text), "TBD."),
        }),
        input.sections[2]!,
      ],
    });

    expect(result.findings).toEqual([]);
  });

  test("an n/a section neither covers a requirement nor owes a citation", () => {
    const input = healthyInput();
    const design = input.sections[2]!;
    const result = analyzeTraceability({
      ...input,
      sections: [
        input.sections[0]!,
        input.sections[1]!,
        section({
          ...design,
          state: "n/a",
          blocks: blocks("This layer is out of scope for the change and states no machinery."),
        }),
      ],
    });

    // The section is skipped, so it raises no scope finding of its own...
    expect(result.findings.every((finding) => finding.sectionId !== "sec-design")).toBe(true);
    // ...and it cannot be the coverage for a requirement either.
    const row = result.matrix.rows.find((candidate) => candidate.requirementId === "R1")!;
    expect(row.cells.find((cell) => cell.layerKey === "system")!.covered).toBe(false);
  });

  test("a spec that is only the deepest layer fails structurally", () => {
    const result = analyzeTraceability({
      layers: LAYERS,
      sections: [
        section({ id: "sec-req", key: "requirements", title: "Requirements", layerKey: "intent" }),
        section({ id: "sec-behavior", key: "behavior", title: "Behavior", layerKey: "contract" }),
        section({
          id: "sec-design",
          key: "design",
          title: "Design",
          layerKey: "system",
          blocks: blocks(
            "A Postgres counter row per org backs the ceiling walk and a histogram times it.",
          ),
        }),
      ],
    });

    const kinds = result.findings.map((finding) => finding.kind);
    expect(kinds).toContain("no_requirements");
    expect(kinds).toContain("missing_outer_layer");
    // The ledger layer is reported once, by the more specific finding.
    expect(result.findings.filter((finding) => finding.layerKey === "intent")).toHaveLength(1);
    const outer = result.findings.find((finding) => finding.kind === "missing_outer_layer")!;
    expect(outer.layerKey).toBe("contract");

    // Outside-in then stops at the outermost fatal layer, so the deepest layer
    // is never polished on top of a premise that was never written.
    const stopped = applyOutsideIn(result.findings, LAYERS);
    expect(stopped.stoppedAtLayerKey).toBe("intent");
    expect(stopped.findings.map((entry) => entry.kind)).toEqual(["no_requirements"]);
    expect(stopped.suppressedCount).toBe(2);
  });

  test("a terse requirements ledger is a written layer, not an empty one", () => {
    // Every ledger line here is under the scope-creep threshold, and every one
    // is a valid requirement. The ledger layer must not read as empty.
    const result = analyzeTraceability({
      layers: LAYERS,
      sections: [
        section({
          id: "sec-req",
          key: "requirements",
          title: "Requirements",
          layerKey: "intent",
          blocks: blocks("- R1: Users can log in.", "- N1: Login is fast."),
        }),
        section({
          id: "sec-behavior",
          key: "behavior",
          title: "Behavior",
          layerKey: "contract",
          blocks: blocks("A user signs in with an email and a password (R1, N1)."),
        }),
        section({
          id: "sec-design",
          key: "design",
          title: "Design",
          layerKey: "system",
          blocks: blocks(
            "A session cookie carries the login, and the check is a single indexed read (R1, N1).",
          ),
        }),
      ],
    });

    expect(result.findings.filter((finding) => finding.kind === "missing_outer_layer")).toEqual([]);
    expect(result.findings.every((finding) => finding.severity !== "fatal")).toBe(true);
    // Nothing is suppressed, because nothing was fatal.
    const stopped = applyOutsideIn(result.findings, LAYERS);
    expect(stopped.stoppedAtLayerKey).toBeNull();
    expect(stopped.suppressedCount).toBe(0);
    expect(result.matrix.rows.map((row) => row.requirementId)).toEqual(["R1", "N1"]);
  });

  test("a layer of short sentences is written, so it stops no pass", () => {
    const result = analyzeTraceability({
      layers: LAYERS,
      sections: [
        section({
          id: "sec-req",
          key: "requirements",
          title: "Requirements",
          layerKey: "intent",
          blocks: blocks("- R1: Users can log in."),
        }),
        // Short, but plainly written.
        section({
          id: "sec-behavior",
          key: "behavior",
          title: "Behavior",
          layerKey: "contract",
          blocks: blocks("Sign-in works (R1)."),
        }),
        section({
          id: "sec-design",
          key: "design",
          title: "Design",
          layerKey: "system",
          blocks: blocks("A session cookie carries the login and is checked on each request (R1)."),
        }),
      ],
    });

    expect(result.findings.filter((finding) => finding.severity === "fatal")).toEqual([]);
  });

  test("a truly empty outer layer still stops the pass", () => {
    const result = analyzeTraceability({
      layers: LAYERS,
      sections: [
        section({
          id: "sec-req",
          key: "requirements",
          title: "Requirements",
          layerKey: "intent",
          blocks: blocks("- R1: Users can log in."),
        }),
        // The template's untouched placeholder paragraph.
        section({
          id: "sec-behavior",
          key: "behavior",
          title: "Behavior",
          layerKey: "contract",
          blocks: blocks("   "),
        }),
        section({
          id: "sec-design",
          key: "design",
          title: "Design",
          layerKey: "system",
          blocks: blocks("A session cookie carries the login and is checked on each request (R1)."),
        }),
      ],
    });

    const fatal = result.findings.filter((finding) => finding.severity === "fatal");
    expect(fatal).toHaveLength(1);
    expect(fatal[0]!.kind).toBe("missing_outer_layer");
    expect(fatal[0]!.layerKey).toBe("contract");
  });

  test("a document with no requirements section cannot be traced", () => {
    expect(() =>
      analyzeTraceability({
        layers: LAYERS,
        sections: [section({ id: "sec-design", key: "design", layerKey: "system" })],
      }),
    ).toThrow(TraceabilityInputError);
  });

  test("a section naming an unknown layer is rejected", () => {
    expect(() =>
      analyzeTraceability({
        layers: LAYERS,
        sections: [section({ id: "sec-req", key: "requirements", layerKey: "nowhere" })],
      }),
    ).toThrow(TraceabilityInputError);
  });

  test("a tombstoned requirement leaves the matrix", () => {
    const input = healthyInput();
    const ledger = input.sections[0]!;
    const result = analyzeTraceability({
      ...input,
      sections: [
        section({ ...ledger, blocks: blocks("- R1: [removed]", "- N1: the limiter stays fast") }),
        input.sections[1]!,
        input.sections[2]!,
      ],
    });

    expect(result.matrix.rows.map((row) => row.requirementId)).not.toContain("R1");
    // Content that only cites the removed R1 no longer counts as cited.
    const scope = result.findings.filter((finding) => finding.kind === "scope_creep");
    expect(scope).toHaveLength(1);
    expect(scope[0]!.detail).toContain("named reset time");
  });
});

function finding(overrides: Partial<GapFinding> & Pick<GapFinding, "id" | "layerKey">): GapFinding {
  return {
    kind: "red_team",
    severity: "gap",
    sectionId: "sec",
    sectionTitle: "Section",
    requirementId: null,
    summary: "summary",
    detail: "detail",
    proposedDiff: null,
    ...overrides,
  };
}

describe("applyOutsideIn", () => {
  test("orders findings outside-in when nothing is fatal", () => {
    const result = applyOutsideIn(
      [
        finding({ id: "deep", layerKey: "system" }),
        finding({ id: "outer", layerKey: "intent" }),
        finding({ id: "middle", layerKey: "contract" }),
      ],
      LAYERS,
    );

    expect(result.findings.map((entry) => entry.id)).toEqual(["outer", "middle", "deep"]);
    expect(result.stoppedAtLayerKey).toBeNull();
    expect(result.suppressedCount).toBe(0);
  });

  test("an inter-layer contradiction halts the pass and is reported first", () => {
    const result = applyOutsideIn(
      [
        finding({ id: "nit-1", layerKey: "system" }),
        finding({ id: "nit-2", layerKey: "system" }),
        finding({
          id: "contradiction",
          layerKey: "contract",
          severity: "fatal",
          summary: "Failure modes assume a 30s cache, but Behavior promises a sharper reset",
        }),
        finding({ id: "contract-nit", layerKey: "contract" }),
      ],
      LAYERS,
    );

    expect(result.findings[0]!.id).toBe("contradiction");
    expect(result.stoppedAtLayerKey).toBe("contract");
    // The two System nits are withheld: they polish a broken premise.
    expect(result.findings.map((entry) => entry.id)).toEqual(["contradiction", "contract-nit"]);
    expect(result.suppressedCount).toBe(2);
  });

  test("the outermost fatal finding decides where the pass stops", () => {
    const result = applyOutsideIn(
      [
        finding({ id: "deep-fatal", layerKey: "system", severity: "fatal" }),
        finding({ id: "outer-fatal", layerKey: "intent", severity: "fatal" }),
        finding({ id: "contract-gap", layerKey: "contract" }),
      ],
      LAYERS,
    );

    expect(result.findings.map((entry) => entry.id)).toEqual(["outer-fatal"]);
    expect(result.stoppedAtLayerKey).toBe("intent");
    expect(result.suppressedCount).toBe(2);
  });
});
