import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, test, vi } from "vitest";
import type {
  AlternativesDecidedTranscriptChip,
  AlternativesProposedTranscriptChip,
  SpecAlternativeOption,
  SpecAlternativesStage,
} from "@engrams/spec-document";

import { SpecAlternatives } from "./SpecAlternatives";

const SPEC_ID = "00000000-0000-4000-8000-000000001119";

/** The rail width the canvas keeps for this stage (mock 2f). */
const RAIL_WIDTH = "27rem";

function option(key: string, title: string): SpecAlternativeOption {
  return {
    key,
    title,
    tradeoffs: [
      { sign: "+", text: `${key} keeps one code path` },
      { sign: "-", text: `${key} touches 31 call sites` },
      { sign: "~", text: `${key} needs a meter later` },
    ],
  };
}

const OPTIONS = [
  option("A", "Second org-level bucket"),
  option("B", "Hierarchical limiter"),
  option("C", "Admission control at the scheduler"),
];

const proposal: AlternativesProposedTranscriptChip = {
  kind: "spec_alternatives_proposed",
  specId: SPEC_ID,
  sectionId: "alternatives",
  setId: "set-1",
  options: OPTIONS,
  comparison: {
    provenance: "verified against gateway/limits.rs @ 8f2c1a4",
    rows: [
      {
        axis: "Refusal latency",
        cells: [
          { optionKey: "A", value: "gateway, under 5ms" },
          { optionKey: "B", value: "gateway, 3.1ms p99 measured" },
          { optionKey: "C", value: "post-insert, about 400ms" },
        ],
      },
    ],
  },
  leanKey: "B",
};

const decision: AlternativesDecidedTranscriptChip = {
  kind: "spec_alternatives_decided",
  specId: SPEC_ID,
  sectionId: "alternatives",
  setId: "set-1",
  pickedKey: "B",
  reason: "One code path, and the team tier drops out free later.",
  decidedBy: "author",
};

function renderStage(
  overrides: Partial<Parameters<typeof SpecAlternatives>[0]> & {
    stage?: SpecAlternativesStage;
  } = {},
) {
  const onPick = vi.fn();
  const onHybrid = vi.fn();
  const view = render(
    <div style={{ width: RAIL_WIDTH }}>
      <SpecAlternatives
        stage={overrides.stage ?? { proposal, decision: null }}
        editable={overrides.editable ?? true}
        pending={overrides.pending ?? false}
        error={overrides.error ?? null}
        onPick={overrides.onPick ?? onPick}
        onHybrid={overrides.onHybrid ?? onHybrid}
      />
    </div>,
  );
  return { onHybrid, onPick, view };
}

describe("SpecAlternatives", () => {
  test("renders one card per option with exactly three trade-off lines", () => {
    renderStage();

    const cards = screen.getAllByTestId("spec-alternative-card");
    expect(cards).toHaveLength(3);
    for (const [index, card] of cards.entries()) {
      const source = OPTIONS[index]!;
      expect(within(card).getByRole("heading", { level: 3 }).textContent).toBe(source.title);
      const tradeoffs = within(card).getAllByTestId("spec-alternative-tradeoff");
      expect(tradeoffs).toHaveLength(3);
      // Each line carries the visible sign plus its screen-reader name.
      expect(tradeoffs.map((line) => line.textContent)).toEqual([
        `+gain${source.tradeoffs[0]!.text}`,
        `−cost${source.tradeoffs[1]!.text}`,
        `~caveat${source.tradeoffs[2]!.text}`,
      ]);
    }
  });

  test("carries one provenance caption over the whole surface", () => {
    renderStage();
    expect(screen.getByText("verified against gateway/limits.rs @ 8f2c1a4")).toBeTruthy();
  });

  test("marks the agent's lean without picking it", () => {
    renderStage();

    const lean = screen
      .getAllByTestId("spec-alternative-card")
      .find((card) => card.className.includes("is-lean"));
    expect(lean?.getAttribute("aria-label")).toBe("Option B: Hierarchical limiter");
    expect(screen.queryByRole("status")).toBeNull();
  });

  test("compare replaces the cards, and returns to them", async () => {
    const user = userEvent.setup();
    renderStage();

    await user.click(screen.getByRole("button", { name: "Compare" }));
    expect(screen.queryAllByTestId("spec-alternative-card")).toHaveLength(0);
    const table = screen.getByTestId("spec-alternatives-compare");
    expect(within(table).getByText("gateway, 3.1ms p99 measured")).toBeTruthy();

    await user.click(screen.getByRole("button", { name: "Back to cards" }));
    expect(screen.getAllByTestId("spec-alternative-card")).toHaveLength(3);
    expect(screen.queryByTestId("spec-alternatives-compare")).toBeNull();
  });

  test("a pick sends the option and the author's reason", async () => {
    const user = userEvent.setup();
    const { onPick } = renderStage();

    await user.click(screen.getByRole("button", { name: "Pick B" }));
    const field = screen.getByLabelText(/Why does B win\?/);
    expect(screen.getByRole("button", { name: "Confirm B" }).hasAttribute("disabled")).toBe(true);

    await user.type(field, "One code path.");
    await user.click(screen.getByRole("button", { name: "Confirm B" }));
    expect(onPick).toHaveBeenCalledWith({ optionKey: "B", reason: "One code path." });
  });

  test("asks the agent for a hybrid in the conversation", async () => {
    const user = userEvent.setup();
    const { onHybrid } = renderStage();

    await user.click(screen.getByRole("button", { name: "Reply with a hybrid" }));
    expect(onHybrid).toHaveBeenCalledTimes(1);
  });

  test("a decided stage shows the winner and stops offering the pick", () => {
    renderStage({ stage: { proposal, decision } });

    expect(screen.getByRole("status").textContent).toContain("Picked B · Hierarchical limiter");
    expect(screen.getByRole("status").textContent).toContain(decision.reason);
    expect(screen.queryByRole("button", { name: "Pick B" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Reply with a hybrid" })).toBeNull();
  });

  test("a reader who cannot decide still reads the cards and the compare", async () => {
    const user = userEvent.setup();
    renderStage({ editable: false });

    expect(screen.getAllByTestId("spec-alternative-card")).toHaveLength(3);
    expect(screen.queryByRole("button", { name: "Pick B" })).toBeNull();
    await user.click(screen.getByRole("button", { name: "Compare" }));
    expect(screen.getByTestId("spec-alternatives-compare")).toBeTruthy();
  });

  test("shows a failed pick without losing the cards", () => {
    renderStage({ error: "This set is no longer current." });

    expect(screen.getByText("This set is no longer current.")).toBeTruthy();
    expect(screen.getAllByTestId("spec-alternative-card")).toHaveLength(3);
  });
});
