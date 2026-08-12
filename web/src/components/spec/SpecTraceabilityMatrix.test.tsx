import { render, screen, within } from "@testing-library/react";
import { describe, expect, test } from "vitest";
import type { TraceabilityMatrix } from "@engrams/spec-document";

import { SpecTraceabilityMatrix } from "./SpecTraceabilityMatrix";

const matrix: TraceabilityMatrix = {
  layers: [
    { key: "contract", title: "Behavior" },
    { key: "system", title: "Design" },
  ],
  rows: [
    {
      requirementId: "R1",
      label: "org cap",
      verdict: "covered",
      cells: [
        {
          layerKey: "contract",
          covered: true,
          citations: [{ sectionId: "sec-behavior", sectionTitle: "Behavior", blockIndex: 0 }],
          note: null,
        },
        {
          layerKey: "system",
          covered: true,
          citations: [{ sectionId: "sec-design", sectionTitle: "Design", blockIndex: 2 }],
          note: null,
        },
      ],
    },
    {
      requirementId: "R3",
      label: "refusal names reset",
      verdict: "gap",
      cells: [
        {
          layerKey: "contract",
          covered: true,
          citations: [{ sectionId: "sec-behavior", sectionTitle: "Behavior", blockIndex: 0 }],
          note: null,
        },
        {
          layerKey: "system",
          covered: false,
          citations: [],
          note: "no API error shape carries reset_at",
        },
      ],
    },
    {
      requirementId: null,
      label: "auto-upgrade prompt",
      verdict: "scope",
      cells: [
        {
          layerKey: "contract",
          covered: false,
          citations: [],
          note: 'Behavior ¶4 "auto-upgrade prompt" cites no requirement',
        },
        { layerKey: "system", covered: false, citations: [], note: null },
      ],
    },
  ],
};

describe("SpecTraceabilityMatrix", () => {
  test("renders one column per layer below the requirement ledger", () => {
    render(<SpecTraceabilityMatrix matrix={matrix} />);

    const table = screen.getByRole("table", { name: "Traceability matrix" });
    const headers = within(table)
      .getAllByRole("columnheader")
      .map((header) => header.textContent);
    expect(headers).toEqual(["Requirement", "→ Behavior coverage", "→ Design coverage", "Verdict"]);
  });

  test("marks only the uncovered cells amber", () => {
    const { container } = render(<SpecTraceabilityMatrix matrix={matrix} />);

    const gapCells = [...container.querySelectorAll(".spec-gap-cell")].map(
      (cell) => cell.textContent,
    );
    expect(gapCells).toEqual([
      "no API error shape carries reset_at",
      'Behavior ¶4 "auto-upgrade prompt" cites no requirement',
    ]);
  });

  test("shows the citation that covers a requirement", () => {
    render(<SpecTraceabilityMatrix matrix={matrix} />);

    expect(screen.getAllByText("§Behavior ¶1").length).toBeGreaterThan(0);
    expect(screen.getByText("§Design ¶3")).toBeTruthy();
  });

  test("renders the verdict for each row", () => {
    render(<SpecTraceabilityMatrix matrix={matrix} />);

    expect(screen.getByText("✓")).toBeTruthy();
    expect(screen.getByText("gap")).toBeTruthy();
    // The uncited row is the scope-creep detector.
    expect(screen.getByText("scope?")).toBeTruthy();
    expect(screen.getByText("(uncited)")).toBeTruthy();
  });

  test("a spec with no requirements says so instead of rendering an empty table", () => {
    render(<SpecTraceabilityMatrix matrix={{ layers: matrix.layers, rows: [] }} />);

    expect(screen.queryByRole("table")).toBeNull();
    expect(screen.getByText(/states no requirement to trace/)).toBeTruthy();
  });
});
