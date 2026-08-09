import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, test, vi } from "vitest";

import {
  SectionStateTranscriptChip,
  type SpecSectionStateChipData,
} from "./SectionStateTranscriptChip";

const chip: SpecSectionStateChipData = {
  kind: "spec_section_state_changed",
  specId: "00000000-0000-4000-8000-000000000001",
  sectionId: "failure-modes",
  sectionTitle: "Failure modes",
  before: { state: "empty", naReason: null },
  after: { state: "drafted", naReason: null },
  provisional: true,
  undo: {
    kind: "restore_section_state",
    specId: "00000000-0000-4000-8000-000000000001",
    sectionId: "failure-modes",
    expected: { state: "drafted", naReason: null },
    restore: { state: "empty", naReason: null },
  },
};

describe("SectionStateTranscriptChip", () => {
  test("renders the state action and sends its exact undo payload", async () => {
    const user = userEvent.setup();
    const onUndo = vi.fn();
    render(<SectionStateTranscriptChip chip={chip} onUndo={onUndo} />);

    expect(screen.getByRole("status", { name: "Failure modes state changed" }).textContent).toMatch(
      /empty → drafted/,
    );
    expect(screen.getByText("provisional")).toBeTruthy();

    await user.click(screen.getByRole("button", { name: "Undo" }));
    expect(onUndo).toHaveBeenCalledOnce();
    expect(onUndo).toHaveBeenCalledWith(chip.undo);
  });

  test("renders the stated n/a reason", () => {
    render(
      <SectionStateTranscriptChip
        chip={{
          ...chip,
          provisional: false,
          after: { state: "n/a", naReason: "The change has no data migration." },
        }}
        onUndo={() => {}}
      />,
    );

    expect(screen.getByText("The change has no data migration.")).toBeTruthy();
    expect(screen.queryByText("provisional")).toBeNull();
  });
});
