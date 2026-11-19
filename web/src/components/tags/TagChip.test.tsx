import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";

import { TagChip } from "./TagChip";
import { TagChipRow } from "./TagChipRow";

describe("TagChip", () => {
  it("draws the name from the stored color, with the full name and description as its tooltip", () => {
    render(
      <TagChip tag={{ name: "Platform", color: "#1f6feb", description: "Owned by platform" }} />,
    );
    const chip = screen.getByText("Platform");
    expect([...chip.classList]).toEqual(expect.arrayContaining(["tag-chip", "rounded-sm"]));
    expect(chip.style.getPropertyValue("--tag-color")).toBe("#1f6feb");
    expect(chip.getAttribute("title")).toBe("Platform: Owned by platform");
  });

  it("shows the full name on hover without a description, and is never interactive", () => {
    render(<TagChip tag={{ name: "Ops", color: "#8250df", description: "" }} />);
    const chip = screen.getByText("Ops");
    expect(chip.getAttribute("title")).toBe("Ops");
    expect(chip.hasAttribute("tabindex")).toBe(false);
    expect(screen.queryByRole("button")).toBeNull();
    expect(screen.queryByRole("link")).toBeNull();
  });
});

const tag = (name: string) => ({ id: `t-${name}`, name, color: "#1f6feb", description: "" });

describe("TagChipRow", () => {
  it("shows two chips and a +N control whose name starts with its visible text and lists every tag", () => {
    render(<TagChipRow tags={[tag("Billing"), tag("Ops"), tag("Platform")]} />);
    expect(screen.getByText("Billing")).toBeTruthy();
    expect(screen.getByText("Ops")).toBeTruthy();
    expect(screen.queryByText("Platform")).toBeNull();
    const more = screen.getByRole("button", { name: "+1 more. All tags: Billing, Ops, Platform" });
    expect(more.textContent).toBe("+1");
  });

  it("with every tag shown, has only the compact +N that a narrow cell uses", () => {
    render(<TagChipRow tags={[tag("Billing"), tag("Ops")]} />);
    const buttons = screen.getAllByRole("button");
    expect(buttons.map((button) => button.getAttribute("aria-label"))).toEqual([
      "+1 more. All tags: Billing, Ops",
    ]);
    expect([...buttons[0]!.classList]).toEqual(
      expect.arrayContaining(["hidden", "@max-[11rem]:inline-flex"]),
    );
  });

  // jsdom has no layout, so this pins the container-query contract: below
  // 11rem the row shows its first chip and a +N that counts every other tag.
  it("adapts to a narrow cell: later chips and the wide +N hide, the compact +N shows", () => {
    const { container } = render(<TagChipRow tags={[tag("a"), tag("b"), tag("c")]} />);
    expect(container.firstElementChild!.classList.contains("@container")).toBe(true);
    const [first, second] = [...container.querySelectorAll(".tag-chip")];
    expect(first!.classList.contains("@max-[11rem]:hidden")).toBe(false);
    // In a narrow cell the first chip may shrink too, so the +N stays inside.
    expect(first!.classList.contains("@max-[11rem]:shrink")).toBe(true);
    expect(second!.classList.contains("@max-[11rem]:hidden")).toBe(true);
    const wide = screen.getByRole("button", { name: "+1 more. All tags: a, b, c" });
    const compact = screen.getByRole("button", { name: "+2 more. All tags: a, b, c" });
    expect(wide.classList.contains("@max-[11rem]:hidden")).toBe(true);
    expect([...compact.classList]).toEqual(
      expect.arrayContaining(["hidden", "@max-[11rem]:inline-flex"]),
    );
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

  // jsdom has no layout, so this pins the class contract for a narrow column:
  // the row may shrink, the first chip keeps its width, the others truncate.
  it("keeps the first chip whole and lets the following chips truncate in a narrow row", () => {
    const { container } = render(<TagChipRow tags={[tag("a"), tag("b"), tag("c")]} />);
    expect(container.firstElementChild!.classList.contains("min-w-0")).toBe(true);
    const [first, second] = [...container.querySelectorAll(".tag-chip")];
    expect(first!.classList.contains("shrink-0")).toBe(true);
    expect(second!.classList.contains("shrink-0")).toBe(false);
    expect(second!.classList.contains("min-w-0")).toBe(true);
  });

  it("renders nothing without tags", () => {
    const { container } = render(<TagChipRow tags={[]} />);
    expect(container.innerHTML).toBe("");
  });
});
