import { useState } from "react";
import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, test, vi } from "vitest";

import { InlineUploadComposer } from "./InlineUploadComposer";
import type { UploadToken } from "./useSessionUploads";

const PATH = "/tmp/uploads/019fe2ff-0464-75f3-bb20-a8c1844579b9/engrams-upload-smoke.txt";

function Harness({ recognizePaths = true }: { recognizePaths?: boolean }) {
  const [value, setValue] = useState("");
  const [tokens, setTokens] = useState<UploadToken[]>([]);
  // Stands in for the transcript's streaming updates: a parent re-render that
  // changes none of the composer's inputs.
  const [, setTick] = useState(0);
  return (
    <>
      <InlineUploadComposer
        value={value}
        tokens={tokens}
        onChange={setValue}
        onFiles={() => []}
        onCanonicalPath={(path) => {
          if (!recognizePaths) return false;
          const parts = path.split("/");
          setTokens([
            {
              id: parts.at(-2)!,
              name: parts.at(-1)!,
              path,
              status: "uploaded",
              progress: 1,
            },
          ]);
          return true;
        }}
        onRemove={() => setTokens([])}
        onRetry={() => {}}
        placeholder="Write a message"
        ariaLabel="Message input"
      />
      <output data-testid="value">{value}</output>
      <button type="button" data-testid="rerender" onClick={() => setTick((t) => t + 1)} />
    </>
  );
}

function pastePath(editor: HTMLElement, value = PATH) {
  fireEvent.paste(editor, { clipboardData: { getData: () => value } });
}

function selectToken() {
  const token = screen
    .getByText("engrams-upload-smoke.txt")
    .closest<HTMLElement>("[data-upload-path]")!;
  fireEvent.mouseDown(token);
}

describe("InlineUploadComposer", () => {
  test("pastes a canonical path as an inline atomic filename token", () => {
    render(<Harness />);
    const editor = screen.getByRole("textbox", { name: "Message input" });

    pastePath(editor, `Read ${PATH} before you answer.`);

    expect(screen.getByText("engrams-upload-smoke.txt")).toBeTruthy();
    expect(editor.textContent).toBe("Read engrams-upload-smoke.txt before you answer.");
    expect(screen.queryByText(PATH)).toBeNull();
    expect(screen.queryByRole("button", { name: /copy|remove/i })).toBeNull();
    expect(screen.getByTestId("value").textContent).toBe(`Read ${PATH} before you answer.`);
  });

  test("leaves an unknown canonical path as text when no session can own it", () => {
    render(<Harness recognizePaths={false} />);
    const editor = screen.getByRole("textbox", { name: "Message input" });

    pastePath(editor, PATH);

    expect(editor.textContent).toBe(PATH);
    expect(editor.querySelector("[data-upload-path]")).toBeNull();
  });

  test("copies a selected token as its literal canonical path", () => {
    render(<Harness />);
    const editor = screen.getByRole("textbox", { name: "Message input" });
    pastePath(editor);
    selectToken();
    const setData = vi.fn();

    fireEvent.copy(editor, { clipboardData: { setData } });

    expect(setData).toHaveBeenCalledWith("text/plain", PATH);
  });

  test("cuts a selected token as its literal path and removes the whole token", () => {
    render(<Harness />);
    const editor = screen.getByRole("textbox", { name: "Message input" });
    pastePath(editor, `Read ${PATH} now`);
    selectToken();
    const setData = vi.fn();

    fireEvent.cut(editor, { clipboardData: { setData } });

    expect(setData).toHaveBeenCalledWith("text/plain", PATH);
    expect(screen.queryByText("engrams-upload-smoke.txt")).toBeNull();
    expect(screen.getByTestId("value").textContent).toBe("Read  now");
  });

  test("backspace removes a selected token as one editing unit", () => {
    render(<Harness />);
    const editor = screen.getByRole("textbox", { name: "Message input" });
    pastePath(editor, `Read ${PATH} now`);
    selectToken();

    fireEvent.keyDown(editor, { key: "Backspace" });

    expect(screen.queryByText("engrams-upload-smoke.txt")).toBeNull();
    expect(screen.getByTestId("value").textContent).toBe("Read  now");
  });

  // A live session re-renders the composer on every streamed token. React 19
  // reassigns `innerHTML` on every commit that re-renders an element holding a
  // `dangerouslySetInnerHTML` prop, and replacing the children of a focused
  // contenteditable throws the caret to offset 0 — so an unchanged re-render
  // used to reset the caret many times a second while the user typed. The
  // editor's nodes must therefore SURVIVE a re-render that changes nothing.
  test("leaves the editor's DOM untouched across a re-render that changes nothing", () => {
    render(<Harness />);
    const editor = screen.getByRole("textbox", { name: "Message input" });
    pastePath(editor, `Read ${PATH} now`);
    const before = [...editor.childNodes];
    expect(before.length).toBeGreaterThan(1);

    // What a streamed token does to this subtree: re-render, same props.
    fireEvent.click(screen.getByTestId("rerender"));
    fireEvent.click(screen.getByTestId("rerender"));

    // Node IDENTITY, not shape: a rewritten `innerHTML` reproduces the same
    // markup with new nodes, and it is the node swap that drops the caret.
    expect(editor.childNodes.length).toBe(before.length);
    for (const [index, node] of before.entries()) {
      expect(editor.childNodes[index]).toBe(node);
    }
  });
});
