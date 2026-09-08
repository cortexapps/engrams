import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";

import { GenericField } from "./GenericField";

vi.mock("@/hooks/useProfiles", () => ({
  useProfiles: () => ({ data: { profiles: [] } }),
}));

/** A parent that holds the committed value, the way BlockInspector does. */
function Host({ initial, onCommit }: { initial: unknown; onCommit: (v: unknown) => void }) {
  const [value, setValue] = useState<unknown>(initial);
  return (
    <>
      <GenericField
        spec={{ type: "json", key: "eventKeys", label: "Event keys" }}
        value={value}
        onChange={(next) => {
          setValue(next);
          onCommit(next);
        }}
        pinned={false}
        sessionSources={[]}
        variablePaths={[]}
      />
      <button type="button" onClick={() => setValue(["discarded"])}>
        discard
      </button>
    </>
  );
}

describe("GenericField json", () => {
  it("is typeable keystroke by keystroke; commits on each valid parse; keeps the typed text", () => {
    const onCommit = vi.fn();
    render(<Host initial={undefined} onCommit={onCommit} />);
    const area = screen.getByRole("textbox") as HTMLTextAreaElement;

    // Every intermediate (invalid) keystroke must stay in the textarea.
    const typed = ["[", '["', '["p', '["pu', '["pus', '["push', '["push"', '["push"]'];
    for (const step of typed) {
      fireEvent.change(area, { target: { value: step } });
      expect(area.value).toBe(step);
    }
    expect(onCommit).toHaveBeenLastCalledWith(["push"]);
    // The text is left exactly as typed, not reformatted under the cursor.
    expect(area.value).toBe('["push"]');
    expect(screen.queryByText("Not valid JSON")).toBeNull();

    // While invalid, the flag shows and nothing new is committed.
    onCommit.mockClear();
    fireEvent.change(area, { target: { value: '["push"' } });
    expect(screen.getByText("Not valid JSON")).toBeTruthy();
    expect(onCommit).not.toHaveBeenCalled();
  });

  it("re-syncs to an external change (discard) and to a cleared value", () => {
    const onCommit = vi.fn();
    render(<Host initial={["push"]} onCommit={onCommit} />);
    const area = screen.getByRole("textbox") as HTMLTextAreaElement;
    fireEvent.change(area, { target: { value: '["push", "issues"]' } });
    expect(onCommit).toHaveBeenLastCalledWith(["push", "issues"]);

    fireEvent.click(screen.getByText("discard"));
    expect(area.value).toBe(JSON.stringify(["discarded"], null, 2));

    fireEvent.change(area, { target: { value: "" } });
    expect(onCommit).toHaveBeenLastCalledWith(undefined);
    expect(area.value).toBe("");
  });
});
