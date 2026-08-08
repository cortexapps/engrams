import { useState } from "react";
import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, test, vi } from "vitest";

import { InlineUploadComposer } from "./InlineUploadComposer";
import type { UploadToken } from "./useSessionUploads";

const PATH = "/tmp/uploads/019fe2ff-0464-75f3-bb20-a8c1844579b9/engrams-upload-smoke.txt";

function Harness({ copy }: { copy: (value: string) => Promise<void> }) {
  const [value, setValue] = useState("");
  const [tokens, setTokens] = useState<UploadToken[]>([]);
  Object.defineProperty(navigator, "clipboard", {
    configurable: true,
    value: { writeText: copy },
  });
  return (
    <InlineUploadComposer
      value={value}
      tokens={tokens}
      onChange={setValue}
      onFiles={() => []}
      onCanonicalPath={(path) => {
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
  );
}

describe("InlineUploadComposer", () => {
  test("pastes a canonical path as an inline filename chip and copies the literal path", async () => {
    const copy = vi.fn(async () => {});
    render(<Harness copy={copy} />);
    const editor = screen.getByRole("textbox", { name: "Message input" });

    fireEvent.paste(editor, {
      clipboardData: { getData: () => `Read ${PATH} before you answer.` },
    });

    expect(screen.getByText("engrams-upload-smoke.txt")).toBeTruthy();
    expect(editor.textContent).toContain("Read ▱engrams-upload-smoke.txt⧉× before you answer.");
    expect(screen.queryByText(PATH)).toBeNull();

    await userEvent.click(screen.getByRole("button", { name: `Copy ${PATH}` }));
    expect(copy).toHaveBeenCalledWith(PATH);
  });
});
