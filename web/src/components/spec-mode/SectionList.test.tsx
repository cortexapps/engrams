import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, test, vi } from "vitest";
import { readFileSync } from "node:fs";

import { SectionList } from "./SectionList";
import type { SpecSurface, SpecSurfaceSection } from "./spec-surface";

describe("SectionList", () => {
  test("renders every glyph and keeps work that has not been reached muted", () => {
    render(<SectionList surface={surface()} onSelectSection={() => undefined} />);

    expect(screen.getByRole("img", { name: "Settled" }).textContent).toBe("✓");
    expect(screen.getByRole("img", { name: "Proposed" }).textContent).toBe("◐");
    expect(screen.getAllByRole("img", { name: "Open" })[0]?.textContent).toBe("○");
    expect(screen.getByRole("img", { name: "Has open questions" }).textContent).toBe("⚑");
    expect(screen.getByRole("img", { name: "Being read" }).textContent).toBe("●");
    expect(screen.getByText("Settled by Ada")).toBeTruthy();
    expect(screen.getByLabelText("2 open questions")).toBeTruthy();
    expect(screen.getByRole("button", { name: /Rollout/ }).dataset.reached).toBe("false");
  });

  test("sends a row click to the shared scroll anchor", async () => {
    const user = userEvent.setup();
    const scrollToSection = vi.fn();
    render(<SectionList surface={surface()} onSelectSection={scrollToSection} />);

    await user.click(screen.getByRole("button", { name: /Proposed design/ }));

    expect(scrollToSection).toHaveBeenCalledWith("design");
  });

  test("disables the proposal beat when reduced motion is requested", () => {
    const css = readFileSync("src/components/spec-mode/spec-mode.css", "utf8");
    expect(css).toMatch(
      /@media \(prefers-reduced-motion: reduce\)[\s\S]*\.spec-mode-section-glyph\[data-proposed="true"\][\s\S]*animation: none/,
    );
  });
});

function surface(): SpecSurface {
  return {
    sections: [
      section({ id: "problem", title: "Problem", state: "settled" }),
      section({ id: "design", title: "Proposed design", state: "proposed" }),
      section({ id: "api", title: "API", state: "open" }),
      section({ id: "failure", title: "Failure modes", openQuestionCount: 2 }),
      section({ id: "data", title: "Data model", isBeingRead: true }),
      section({ id: "rollout", title: "Rollout", isReached: false }),
    ],
    settledCount: 1,
    totalCount: 6,
    openQuestions: [],
    provenanceRanges: [],
    next: { kind: "draft_section", sectionId: "api", sectionTitle: "API" },
  };
}

function section(overrides: Partial<SpecSurfaceSection>): SpecSurfaceSection {
  const id = overrides.id ?? "section";
  return {
    id,
    templateKey: id,
    title: overrides.title ?? "Section",
    state: "open",
    allowNa: true,
    naReason: null,
    isEmpty: false,
    isReached: true,
    isBeingRead: false,
    openQuestionCount: 0,
    settledBy: null,
    stateChangedAt: null,
    credit:
      overrides.state === "settled"
        ? { by: { id: "user-1", name: "Ada" }, at: "2026-08-13T12:00:00.000Z" }
        : null,
    provenance: [],
    ...overrides,
  };
}
