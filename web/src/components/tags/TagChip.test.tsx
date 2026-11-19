import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";

import { TagChip } from "./TagChip";

describe("TagChip", () => {
  it("draws the name from the stored color, with the description as its tooltip", () => {
    render(
      <TagChip tag={{ name: "Platform", color: "#1f6feb", description: "Owned by platform" }} />,
    );
    const chip = screen.getByText("Platform");
    expect([...chip.classList]).toEqual(expect.arrayContaining(["tag-chip", "rounded-sm"]));
    expect(chip.style.getPropertyValue("--tag-color")).toBe("#1f6feb");
    expect(chip.getAttribute("title")).toBe("Owned by platform");
  });

  it("has no tooltip without a description and is never interactive", () => {
    render(<TagChip tag={{ name: "Ops", color: "#8250df", description: "" }} />);
    const chip = screen.getByText("Ops");
    expect(chip.hasAttribute("title")).toBe(false);
    expect(chip.hasAttribute("tabindex")).toBe(false);
    expect(screen.queryByRole("button")).toBeNull();
    expect(screen.queryByRole("link")).toBeNull();
  });
});
