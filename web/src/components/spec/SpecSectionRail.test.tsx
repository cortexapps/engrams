import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, test, vi } from "vitest";

import type { SpecRail } from "@/hooks/useSpecRead";
import { SpecSectionRail } from "./SpecSectionRail";

const rail: SpecRail = {
  completeness: { complete: 1, total: 3 },
  sections: [
    {
      id: "context",
      templateKey: "context",
      title: "Context",
      state: "settled",
      naReason: null,
      allowNa: false,
      openQuestionCount: 0,
      settledBy: { id: "member-1", name: "Ada" },
      stateChangedAt: "2026-08-13T08:00:00.000Z",
    },
    {
      id: "design",
      templateKey: "design",
      title: "Design",
      state: "proposed",
      naReason: null,
      allowNa: false,
      openQuestionCount: 2,
      settledBy: null,
      stateChangedAt: "2026-08-13T09:00:00.000Z",
    },
    {
      id: "rollout",
      templateKey: "rollout",
      title: "Rollout",
      state: "open",
      naReason: null,
      allowNa: true,
      openQuestionCount: 0,
      settledBy: null,
      stateChangedAt: null,
    },
  ],
};

describe("SpecSectionRail", () => {
  test("renders the flat section list with settle credit", () => {
    render(<SpecSectionRail rail={rail} editable pendingSectionId={null} onAction={() => {}} />);

    expect(screen.getByText("1 of 3 complete")).toBeTruthy();
    expect(screen.getAllByRole("listitem").map((item) => item.textContent)).toEqual([
      expect.stringContaining("Context"),
      expect.stringContaining("Design"),
      expect.stringContaining("Rollout"),
    ]);
    expect(screen.getByText("Settled by Ada")).toBeTruthy();
    expect(screen.getByLabelText("2 open questions")).toBeTruthy();
  });

  test("settles a proposed section with one click", async () => {
    const user = userEvent.setup();
    const onAction = vi.fn();
    render(<SpecSectionRail rail={rail} editable pendingSectionId={null} onAction={onAction} />);

    await user.click(screen.getByRole("button", { name: "Settle" }));
    expect(onAction).toHaveBeenCalledWith({ sectionId: "design", state: "settled" });
  });

  test("drops a proposed section back to open", async () => {
    const user = userEvent.setup();
    const onAction = vi.fn();
    render(<SpecSectionRail rail={rail} editable pendingSectionId={null} onAction={onAction} />);

    await user.click(screen.getByRole("button", { name: "Drop" }));
    expect(onAction).toHaveBeenCalledWith({ sectionId: "design", state: "open" });
  });

  test("reopens a complete section as proposed", async () => {
    const user = userEvent.setup();
    const onAction = vi.fn();
    render(<SpecSectionRail rail={rail} editable pendingSectionId={null} onAction={onAction} />);

    await user.click(screen.getByRole("button", { name: "Revisit" }));
    expect(onAction).toHaveBeenCalledWith({ sectionId: "context", state: "proposed" });
  });

  test("blocks n/a until the person states a reason", async () => {
    const user = userEvent.setup();
    const onAction = vi.fn();
    render(<SpecSectionRail rail={rail} editable pendingSectionId={null} onAction={onAction} />);

    await user.click(screen.getByRole("button", { name: "Mark n/a" }));
    const popover = screen.getByRole("dialog");
    expect(
      within(popover).getByRole("button", { name: "Mark not applicable" }).hasAttribute("disabled"),
    ).toBe(true);

    await user.type(
      within(popover).getByLabelText("Why is this section not applicable?"),
      "No staged rollout is needed.",
    );
    await user.click(within(popover).getByRole("button", { name: "Mark not applicable" }));

    expect(onAction).toHaveBeenCalledWith({
      sectionId: "rollout",
      state: "n/a",
      reason: "No staged rollout is needed.",
    });
  });
});
