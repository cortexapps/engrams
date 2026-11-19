import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { TagColorField } from "./TagColorField";

describe("TagColorField", () => {
  it("a quick pick sets its color and shows which one is current", async () => {
    const onChange = vi.fn();
    render(<TagColorField value="#009bab" onChange={onChange} />);
    expect(
      screen.getByRole("button", { name: "Use color #009bab" }).getAttribute("aria-pressed"),
    ).toBe("true");
    expect(
      screen.getByRole("button", { name: "Use color #bf6291" }).getAttribute("aria-pressed"),
    ).toBe("false");
    await userEvent.click(screen.getByRole("button", { name: "Use color #bf6291" }));
    expect(onChange).toHaveBeenCalledWith("#bf6291");
  });

  it("the random button sets a hex color", async () => {
    const onChange = vi.fn();
    render(<TagColorField value="#009bab" onChange={onChange} />);
    await userEvent.click(screen.getByRole("button", { name: "Random color" }));
    expect(onChange).toHaveBeenCalledTimes(1);
    expect(onChange.mock.calls[0]![0]).toMatch(/^#[0-9a-f]{6}$/);
  });

  it("the hex field passes on what the admin types, unchecked", () => {
    const onChange = vi.fn();
    render(<TagColorField value="#009bab" onChange={onChange} />);
    fireEvent.change(screen.getByRole("textbox", { name: "Hex color" }), {
      target: { value: "#12" },
    });
    expect(onChange).toHaveBeenCalledWith("#12");
  });

  it("the color picker sets the color it returns", () => {
    const onChange = vi.fn();
    render(<TagColorField value="#009bab" onChange={onChange} />);
    fireEvent.change(screen.getByLabelText("Pick a color"), { target: { value: "#123456" } });
    expect(onChange).toHaveBeenCalledWith("#123456");
  });
});
