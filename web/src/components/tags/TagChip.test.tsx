import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";

import { TagChip } from "./TagChip";
import { TagChipRow } from "./TagChipRow";

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

const tag = (name: string) => ({ id: `t-${name}`, name, color: "#1f6feb", description: "" });

describe("TagChipRow", () => {
  it("shows two chips and a +N control that names every tag", () => {
    render(<TagChipRow tags={[tag("Billing"), tag("Ops"), tag("Platform")]} />);
    expect(screen.getByText("Billing")).toBeTruthy();
    expect(screen.getByText("Ops")).toBeTruthy();
    expect(screen.queryByText("Platform")).toBeNull();
    const more = screen.getByRole("button", { name: "All tags: Billing, Ops, Platform" });
    expect(more.textContent).toBe("+1");
  });

  it("shows no +N control when every tag fits", () => {
    render(<TagChipRow tags={[tag("Billing"), tag("Ops")]} />);
    expect(screen.queryByRole("button")).toBeNull();
  });

  it("puts the selected tags first", () => {
    const { container } = render(
      <TagChipRow tags={[tag("a"), tag("b"), tag("c")]} selected={["c"]} />,
    );
    expect([...container.querySelectorAll(".tag-chip")].map((chip) => chip.textContent)).toEqual([
      "c",
      "a",
    ]);
  });

  it("renders nothing without tags", () => {
    const { container } = render(<TagChipRow tags={[]} />);
    expect(container.innerHTML).toBe("");
  });
});
