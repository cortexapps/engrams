import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";

import { applyCompletion, completionsFor, TemplateField } from "./TemplateField";

const PATHS = [
  "trigger.kind",
  "inputs.repo",
  "event.raw.issue.title",
  "event.raw.issue.number",
  "steps.gate.value",
];

describe("completionsFor", () => {
  it("offers prefix matches first, then paths that contain the fragment", () => {
    expect(completionsFor(PATHS, "event.raw.is")).toEqual([
      "event.raw.issue.title",
      "event.raw.issue.number",
    ]);
    expect(completionsFor(PATHS, "title")).toEqual(["event.raw.issue.title"]);
    expect(completionsFor(PATHS, "")).toEqual(PATHS);
    expect(completionsFor(PATHS, "nothing")).toEqual([]);
  });
});

describe("applyCompletion", () => {
  it("replaces the fragment after the open braces and closes them", () => {
    expect(applyCompletion("Review ${{ ev", 13, "event.raw.issue.title")).toEqual({
      text: "Review ${{ event.raw.issue.title }}",
      caret: "Review ${{ event.raw.issue.title }}".length,
    });
  });

  it("leaves braces the text already closes", () => {
    expect(applyCompletion("${{ ev }} please", 6, "event.raw.issue.title")).toEqual({
      text: "${{ event.raw.issue.title }} please",
      caret: "${{ event.raw.issue.title".length,
    });
  });

  it("does nothing when the caret is not inside open braces", () => {
    expect(applyCompletion("plain text", 5, "inputs.repo")).toEqual({
      text: "plain text",
      caret: 5,
    });
  });
});

function Host({ onChange }: { onChange: (next: string) => void }) {
  const [value, setValue] = useState("Review ");
  return (
    <TemplateField
      value={value}
      onChange={(next) => {
        setValue(next);
        onChange(next);
      }}
      paths={PATHS}
      values={{ "event.raw.issue.title": "Fix the login redirect" }}
      aria-label="Prompt"
    />
  );
}

describe("TemplateField", () => {
  it("opens the list on ${{, narrows as the user types, and Enter inserts the path", () => {
    const onChange = vi.fn();
    render(<Host onChange={onChange} />);
    const input = screen.getByRole("combobox", { name: "Prompt" }) as HTMLInputElement;
    expect(screen.queryByTestId("variable-completion")).toBeNull();

    input.setSelectionRange(7, 7);
    fireEvent.change(input, { target: { value: "Review ${{ ", selectionStart: 11 } });
    expect(screen.getByTestId("variable-completion")).toBeTruthy();
    expect(screen.getAllByRole("option")).toHaveLength(PATHS.length);

    fireEvent.change(input, { target: { value: "Review ${{ issue.ti", selectionStart: 19 } });
    const options = screen.getAllByRole("option");
    expect(options).toHaveLength(1);
    expect(options[0]!.textContent).toContain("event.raw.issue.title");
    // The live preview rides beside the path.
    expect(options[0]!.textContent).toContain("Fix the login redirect");

    fireEvent.keyDown(input, { key: "Enter" });
    expect(onChange).toHaveBeenLastCalledWith("Review ${{ event.raw.issue.title }}");
    expect(screen.queryByTestId("variable-completion")).toBeNull();
  });

  it("the Insert variable button types the opener at the caret and opens the list", () => {
    const onChange = vi.fn();
    render(<Host onChange={onChange} />);
    fireEvent.click(screen.getByRole("button", { name: "Insert variable" }));
    expect(onChange).toHaveBeenLastCalledWith("Review ${{ ");
    expect(screen.getByTestId("variable-completion")).toBeTruthy();
  });

  it("Escape closes the list without changing the text", () => {
    const onChange = vi.fn();
    render(<Host onChange={onChange} />);
    const input = screen.getByRole("combobox", { name: "Prompt" }) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "Review ${{ ", selectionStart: 11 } });
    expect(screen.getByTestId("variable-completion")).toBeTruthy();
    fireEvent.keyDown(input, { key: "Escape" });
    expect(screen.queryByTestId("variable-completion")).toBeNull();
    expect(input.value).toBe("Review ${{ ");
  });
});
