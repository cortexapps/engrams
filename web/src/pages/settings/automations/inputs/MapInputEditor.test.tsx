// The noun-keyed map editor: picker options come from ListInputKeyOptions
// (ledger-observed), a free-typed key is always allowed, rows carry the
// valueShape fields, and path-addressed errors land on their row/field.

import { describe, it, expect, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { MapInputEditor } from "./MapInputEditor";
import { parseInputsSchema } from "@/lib/automation-inputs";
import { REVIEW_INPUTS_SCHEMA } from "@/lib/automation-inputs.fixture";

vi.mock("@/hooks/useAutomationInputs", () => ({
  useInputKeyOptions: () => ({
    data: {
      options: [
        { key: "acme/observed", label: "acme/observed" },
        { key: "engrams/engrams", label: "engrams/engrams" },
      ],
    },
  }),
}));
vi.mock("@/components/ui/select", () => ({
  Select: ({ children }: { children: React.ReactNode }) => (
    <div data-testid="picker">{children}</div>
  ),
  SelectTrigger: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
  SelectValue: () => null,
  SelectContent: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
  SelectItem: ({ children, value }: { children: React.ReactNode; value: string }) => (
    <div data-testid={`option-${value}`}>{children}</div>
  ),
}));

const spec = parseInputsSchema(REVIEW_INPUTS_SCHEMA)[0]!;

describe("MapInputEditor", () => {
  it("lists only picker options not already in the map", () => {
    render(
      <MapInputEditor
        spec={spec}
        value={{ "engrams/engrams": { mode: "auto", autofix: false } }}
        onChange={() => {}}
      />,
    );
    expect(screen.getByTestId("option-acme/observed")).toBeTruthy();
    expect(screen.queryByTestId("option-engrams/engrams")).toBeNull();
  });

  it("adds a free-typed key with the row defaults and refuses duplicates", () => {
    const onChange = vi.fn();
    render(
      <MapInputEditor
        spec={spec}
        value={{ "engrams/engrams": { mode: "auto", autofix: false } }}
        onChange={onChange}
      />,
    );
    const input = screen.getByLabelText("New Repositories key");
    fireEvent.change(input, { target: { value: "acme/fresh" } });
    fireEvent.click(screen.getByRole("button", { name: /add/i }));
    expect(onChange).toHaveBeenCalledWith({
      "engrams/engrams": { mode: "auto", autofix: false },
      "acme/fresh": { mode: "on_request", autofix: false },
    });

    fireEvent.change(input, { target: { value: "engrams/engrams" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(screen.getByRole("alert").textContent).toBe("Already listed");
    expect(onChange).toHaveBeenCalledTimes(1);
  });

  it("edits a row field and removes a row", () => {
    const onChange = vi.fn();
    render(
      <MapInputEditor
        spec={spec}
        value={{
          "engrams/engrams": { mode: "auto", autofix: false },
          "acme/repo": { mode: "on_request", autofix: false },
        }}
        onChange={onChange}
      />,
    );
    fireEvent.click(screen.getByLabelText("acme/repo Autofix"));
    expect(onChange).toHaveBeenLastCalledWith({
      "engrams/engrams": { mode: "auto", autofix: false },
      "acme/repo": { mode: "on_request", autofix: true },
    });
    fireEvent.click(screen.getByLabelText("Remove engrams/engrams"));
    expect(onChange).toHaveBeenLastCalledWith({
      "acme/repo": { mode: "on_request", autofix: false },
    });
  });

  it("renders path-addressed errors on the row and on the field", () => {
    render(
      <MapInputEditor
        spec={spec}
        value={{ "acme/repo": { mode: "auto", autofix: false } }}
        onChange={() => {}}
        errors={[
          { key: "repos", path: "acme/repo", message: "row problem" },
          { key: "repos", path: "acme/repo.mode", message: "mode problem" },
        ]}
      />,
    );
    const alerts = screen.getAllByRole("alert").map((a) => a.textContent);
    expect(alerts).toContain("row problem");
    expect(alerts).toContain("mode problem");
  });
});
