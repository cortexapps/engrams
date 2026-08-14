import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, test, vi } from "vitest";

import { SpecComposer } from "./SpecComposer";

describe("SpecComposer", () => {
  test("Enter sends and keeps the text until the stored row acknowledges it", async () => {
    const user = userEvent.setup();
    const onSend = vi.fn(async () => ({ promptId: "prompt-1" }));
    const view = render(<SpecComposer acknowledgedPromptIds={new Set()} onSend={onSend} />);
    const composer = screen.getByRole("textbox", { name: "Message the spec collaborators" });

    await user.type(composer, "A paragraph worth keeping.{enter}");

    await waitFor(() => expect(onSend).toHaveBeenCalledWith("A paragraph worth keeping."));
    expect((composer as HTMLTextAreaElement).value).toBe("A paragraph worth keeping.");
    expect(screen.getByRole("status").textContent).toContain("Waiting for the shared conversation");

    view.rerender(<SpecComposer acknowledgedPromptIds={new Set(["prompt-1"])} onSend={onSend} />);
    await waitFor(() => expect((composer as HTMLTextAreaElement).value).toBe(""));
  });

  test("Shift+Enter inserts a newline and does not send", async () => {
    const user = userEvent.setup();
    const onSend = vi.fn(async () => ({ promptId: "prompt-1" }));
    render(<SpecComposer acknowledgedPromptIds={new Set()} onSend={onSend} />);
    const composer = screen.getByRole("textbox", { name: "Message the spec collaborators" });

    await user.type(composer, "First line{shift>}{enter}{/shift}Second line");

    expect(onSend).not.toHaveBeenCalled();
    expect((composer as HTMLTextAreaElement).value).toBe("First line\nSecond line");
  });

  test("a failed send shows Retry and preserves the complete text", async () => {
    const user = userEvent.setup();
    const onSend = vi
      .fn<() => Promise<{ promptId: string }>>()
      .mockRejectedValueOnce(new Error("The session is waking"))
      .mockResolvedValueOnce({ promptId: "prompt-2" });
    render(<SpecComposer acknowledgedPromptIds={new Set()} onSend={onSend} />);
    const composer = screen.getByRole("textbox", { name: "Message the spec collaborators" });
    const paragraph = "Do not lose this paragraph when delivery fails.";

    await user.type(composer, `${paragraph}{enter}`);

    expect((await screen.findByRole("alert")).textContent).toContain(
      "Message not sent: The session is waking",
    );
    expect((composer as HTMLTextAreaElement).value).toBe(paragraph);
    expect((composer as HTMLTextAreaElement).disabled).toBe(false);
    await user.click(screen.getByRole("button", { name: "Retry" }));
    await waitFor(() => expect(onSend).toHaveBeenCalledTimes(2));
    expect(onSend).toHaveBeenLastCalledWith(paragraph);
    expect((composer as HTMLTextAreaElement).value).toBe(paragraph);
  });
});
