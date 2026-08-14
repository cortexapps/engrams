import { screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { renderWithProviders } from "@/test-utils";
import { SpecShell } from "./SpecShell";

vi.mock("./SpecPublishConfirm", () => ({
  SpecPublishConfirm: () => <button type="button">Publish</button>,
}));

describe("SpecShell", () => {
  it("renders the three-column frame with document-only vertical scrolling", async () => {
    renderWithProviders(
      <SpecShell
        specId="spec-1"
        title="Quota design"
        templateName="Engineering spec"
        checkpoints={[]}
        viewerIsOwner={false}
      >
        <p>Document</p>
      </SpecShell>,
    );

    const viewport = await screen.findByTestId("spec-mode-viewport");
    const shell = viewport.firstElementChild as HTMLElement;
    const documentRegion = screen.getByRole("region", { name: "Spec document" });

    // No "Primary" navigation here any more: the app sidebar provides it, and
    // the shell's own spine was a lossy copy of it.
    expect(screen.queryByRole("navigation", { name: "Primary" })).toBeNull();
    expect(screen.getByRole("complementary", { name: "Spec sections" })).toBeTruthy();
    expect(documentRegion).toBeTruthy();
    expect(screen.getByRole("complementary", { name: "Conversation" })).toBeTruthy();
    expect(documentRegion.getAttribute("data-scroll")).toBe("doc");

    expect(viewport.className).toContain("spec-mode-viewport");
    expect(shell.className).toContain("spec-mode-shell");
    expect(documentRegion.className).toContain("spec-mode-document");
    // Fills the app layout rather than the window, so it cannot cover the
    // sidebar beside it.
    expect(getComputedStyle(viewport).position).not.toBe("fixed");
    expect(getComputedStyle(viewport).height).toBe("100%");
    expect(getComputedStyle(viewport).overflowX).toBe("auto");
    expect(getComputedStyle(viewport).overflowY).toBe("hidden");
    expect(getComputedStyle(shell).minWidth).toBe("1188px");
    expect(getComputedStyle(shell).gridTemplateColumns).toBe("236px minmax(0, 1fr) 392px");
    expect(getComputedStyle(documentRegion).overflowY).toBe("auto");
  });

  it("does not render the publish trigger for a non-owner", async () => {
    renderWithProviders(
      <SpecShell
        specId="spec-1"
        title="Quota design"
        templateName="Engineering spec"
        checkpoints={[]}
        viewerIsOwner={false}
      />,
    );

    expect(screen.queryByLabelText(/present$/)).toBeNull();
    expect(screen.queryByRole("button", { name: "Publish" })).toBeNull();
  });
});
