import { readFileSync } from "node:fs";
import * as path from "node:path";
import { screen } from "@testing-library/react";
import { describe, expect, test, vi } from "vitest";

import { renderWithProviders } from "@/test-utils";
import type { SpecSurface } from "./spec-surface";
import { SpecMobileView } from "./SpecMobileView";

const surface: SpecSurface = {
  settledCount: 4,
  totalCount: 8,
  openQuestions: [],
  provenanceRanges: [],
  next: { kind: "draft_section", sectionId: "api", sectionTitle: "API" },
  sections: [
    {
      id: "data",
      templateKey: "data",
      title: "Data model",
      state: "settled",
      allowNa: false,
      naReason: null,
      isEmpty: false,
      isReached: true,
      isBeingRead: true,
      openQuestionCount: 1,
      settledBy: { id: "priya", name: "Priya" },
      stateChangedAt: "2026-08-13T18:00:00.000Z",
      credit: { by: { id: "priya", name: "Priya" }, at: "2026-08-13T18:00:00.000Z" },
      provenance: [],
    },
  ],
};

describe("SpecMobileView", () => {
  test("shows status, a read document, presence, and the pinned next action", async () => {
    renderWithProviders(
      <SpecMobileView
        title="Quota design"
        surface={surface}
        presence={[
          {
            kind: "human",
            clientId: 7,
            id: "priya",
            name: "Priya Raman",
            color: "#b85c0a",
            isSelf: false,
            sectionId: "data",
          },
        ]}
        onSend={vi.fn()}
      >
        <div aria-label="Read-only spec canvas">Document</div>
      </SpecMobileView>,
    );

    expect(
      await screen.findByText("4 of 8 settled · ⚑1 · Priya Raman is in §Data model"),
    ).toBeTruthy();
    expect(screen.getByLabelText("Read-only spec canvas")).toBeTruthy();
    expect(screen.getByRole("region", { name: "Next proposal" })).toBeTruthy();
    const primaryAction = screen.getByRole("button", { name: "Do it" });
    expect(primaryAction).toBeTruthy();
    expect(primaryAction.closest(".spec-mode-mobile")).toBeTruthy();
    const css = readFileSync(
      path.join(process.cwd(), "src/components/spec-mode/spec-mode.css"),
      "utf8",
    );
    expect(css).toMatch(/\.spec-mode-mobile button,[\s\S]*?min-height:\s*44px;/);
    expect(screen.getByRole("button", { name: "Reply instead" })).toBeTruthy();
    expect(screen.getByLabelText("Priya Raman is here")).toBeTruthy();
  });
});
