import { screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { renderWithProviders } from "@/test-utils";
import { SpecShell } from "./SpecShell";

vi.mock("@/components/spec/SpecPublishControl", () => ({
  SpecPublishControl: () => <button type="button">Publish</button>,
}));

describe("SpecShell", () => {
  it("renders the four-column frame with document-only vertical scrolling", async () => {
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

    expect(screen.getByRole("navigation", { name: "Primary" })).toBeTruthy();
    expect(screen.getByRole("complementary", { name: "Spec sections" })).toBeTruthy();
    expect(documentRegion).toBeTruthy();
    expect(screen.getByRole("complementary", { name: "Conversation" })).toBeTruthy();
    expect(documentRegion.getAttribute("data-scroll")).toBe("doc");

    expect(viewport.className).toContain("spec-mode-viewport");
    expect(shell.className).toContain("spec-mode-shell");
    expect(documentRegion.className).toContain("spec-mode-document");
    expect(getComputedStyle(viewport).position).toBe("fixed");
    expect(getComputedStyle(viewport).overflowX).toBe("auto");
    expect(getComputedStyle(viewport).overflowY).toBe("hidden");
    expect(getComputedStyle(shell).minWidth).toBe("1240px");
    expect(getComputedStyle(shell).gridTemplateColumns).toBe("52px 236px minmax(0, 1fr) 392px");
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

    expect((await screen.findByLabelText("Presence")).childElementCount).toBe(0);
    expect(screen.queryByRole("button", { name: "Publish" })).toBeNull();
  });
});
