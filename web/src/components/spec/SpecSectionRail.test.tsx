import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, test, vi } from "vitest";

import type { SpecRail } from "@/hooks/useSpecRead";
import { SpecSectionRail } from "./SpecSectionRail";

const rail: SpecRail = {
  completeness: { complete: 1, total: 3 },
  frontierSectionId: "design",
  layers: [
    {
      key: "understand",
      title: "Understand",
      description: "Set the problem boundary.",
      sections: [
        {
          id: "context",
          templateKey: "context",
          title: "Context",
          state: "confirmed",
          naReason: null,
          allowNa: false,
          openQuestionCount: 0,
          provisional: false,
          frontier: false,
        },
      ],
    },
    {
      key: "define",
      title: "Define",
      description: null,
      sections: [
        {
          id: "design",
          templateKey: "design",
          title: "Design",
          state: "drafted",
          naReason: null,
          allowNa: false,
          openQuestionCount: 2,
          provisional: false,
          frontier: true,
        },
        {
          id: "rollout",
          templateKey: "rollout",
          title: "Rollout",
          state: "empty",
          naReason: null,
          allowNa: true,
          openQuestionCount: 0,
          provisional: false,
          frontier: false,
        },
      ],
    },
  ],
};

describe("SpecSectionRail", () => {
  test("groups sections by layer and marks the first incomplete section as the frontier", () => {
    render(<SpecSectionRail rail={rail} editable pendingSectionId={null} onAction={() => {}} />);

    expect(screen.getByText("1 of 3 complete")).toBeTruthy();
    expect(
      screen.getAllByRole("heading", { level: 3 }).map((heading) => heading.textContent),
    ).toEqual(["Understand", "Define"]);
    expect(screen.getByText("Frontier").closest("li")?.textContent).toContain("Design");
    expect(screen.getByLabelText("2 open questions")).toBeTruthy();
  });

  test("shows provisional state for drafted downstream content", () => {
    const provisionalRail: SpecRail = {
      ...rail,
      layers: rail.layers.map((layer) => ({
        ...layer,
        sections: layer.sections.map((section) =>
          section.id === "design" ? { ...section, provisional: true } : section,
        ),
      })),
    };

    render(
      <SpecSectionRail
        rail={provisionalRail}
        editable
        pendingSectionId={null}
        onAction={() => {}}
      />,
    );

    expect(screen.getByText("Provisional").closest("li")?.textContent).toContain("Design");
  });

  test("confirms a drafted section with one click", async () => {
    const user = userEvent.setup();
    const onAction = vi.fn();
    render(<SpecSectionRail rail={rail} editable pendingSectionId={null} onAction={onAction} />);

    await user.click(screen.getByRole("button", { name: "Confirm" }));
    expect(onAction).toHaveBeenCalledWith({ sectionId: "design", state: "confirmed" });
  });

  test("reopens a complete section as drafted", async () => {
    const user = userEvent.setup();
    const onAction = vi.fn();
    render(<SpecSectionRail rail={rail} editable pendingSectionId={null} onAction={onAction} />);

    await user.click(screen.getByRole("button", { name: "Revisit" }));
    expect(onAction).toHaveBeenCalledWith({ sectionId: "context", state: "drafted" });
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
