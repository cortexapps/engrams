import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, test, vi } from "vitest";

import { buildMessages } from "@/components/session-thread/buildMessages";
import type { SpecMessage } from "@/hooks/useSpecMessages";
import type { IndexedEvent, SessionEvent } from "@/lib/types";
import { buildSpecThread, type SpecThreadEntry } from "./buildSpecThread";
import { SpecThread } from "./SpecThread";

const AT = "2026-08-13T10:00:00.000Z";

describe("SpecThread", () => {
  test("renders stored human text and never renders the speaker header from the raw event", () => {
    const messages = buildMessages(
      indexed([
        {
          type: "agent_message",
          run_id: "",
          message_id: "echo-1",
          role: "user",
          prompt_id: "prompt-1",
          text: "[speaker: Priya]\nRaw prompt text must stay hidden.",
          at: AT,
        },
      ]),
      "spec-1",
    ).messages;
    const stored = new Map<string, SpecMessage>([
      [
        "prompt-1",
        {
          promptId: "prompt-1",
          author: { id: "priya", name: "Priya" },
          text: "Keep one parent bucket.",
          createdAt: AT,
        },
      ],
    ]);

    render(
      <SpecThread
        entries={buildSpecThread(messages, stored)}
        isRunning={false}
        onActivity={vi.fn()}
      />,
    );

    expect(screen.getByText("Priya")).toBeTruthy();
    expect(screen.getByText("Keep one parent bucket.")).toBeTruthy();
    expect(screen.queryByText(/\[speaker: Priya\]/)).toBeNull();
    expect(screen.queryByText(/Raw prompt text/)).toBeNull();
  });

  test("keeps an unmatched human turn visible without exposing raw event text", () => {
    const messages = buildMessages(
      indexed([
        {
          type: "agent_message",
          run_id: "",
          message_id: "echo-2",
          role: "user",
          text: "[speaker: Unknown]\nHidden raw text.",
          at: AT,
        },
      ]),
      "spec-1",
    ).messages;

    render(
      <SpecThread
        entries={buildSpecThread(messages, new Map())}
        isRunning={false}
        onActivity={vi.fn()}
      />,
    );

    expect(screen.getByText("Collaborator")).toBeTruthy();
    expect(screen.getByText("This message is not available.")).toBeTruthy();
    expect(screen.queryByText(/speaker:/i)).toBeNull();
    expect(screen.queryByText(/Hidden raw text/)).toBeNull();
  });

  test("coalesces document activity and scrolls to its first section", async () => {
    const user = userEvent.setup();
    const onActivity = vi.fn();
    const messages = buildMessages(
      indexed([
        { type: "run_started", run_id: "run-1", prompt_summary: null, at: AT },
        {
          type: "tool_call_started",
          run_id: "run-1",
          tool_call_id: "update-1",
          tool_name: "spec_update_section",
          args_summary: JSON.stringify({ section_id: "problem" }),
          at: AT,
        },
        {
          type: "tool_call_started",
          run_id: "run-1",
          tool_call_id: "update-2",
          tool_name: "spec_update_section",
          args_summary: JSON.stringify({ section_id: "api" }),
          at: AT,
        },
        { type: "run_completed", run_id: "run-1", ok: true, at: AT },
      ]),
      "spec-1",
    ).messages;
    const entries = buildSpecThread(
      messages,
      new Map(),
      new Map([
        ["problem", "Problem"],
        ["api", "API"],
      ]),
    );

    expect(entries.filter((entry) => entry.kind === "document_activity")).toHaveLength(1);
    render(<SpecThread entries={entries} isRunning={false} onActivity={onActivity} />);
    await user.click(screen.getByRole("button", { name: "Go to Problem" }));

    expect(screen.getByText("Updated §Problem, §API")).toBeTruthy();
    expect(onActivity).toHaveBeenCalledWith("problem");
  });

  test("follows appended entries at the bottom and holds position after the reader scrolls up", () => {
    const first = human("one", "First");
    const second = human("two", "Second");
    const third = human("three", "Third");
    const view = render(<SpecThread entries={[first]} isRunning={false} onActivity={vi.fn()} />);
    const thread = screen.getByRole("log");
    Object.defineProperties(thread, {
      clientHeight: { configurable: true, value: 100 },
      scrollHeight: { configurable: true, value: 500 },
      scrollTop: { configurable: true, writable: true, value: 400 },
    });
    const scrollTo = vi.fn();
    Object.defineProperty(thread, "scrollTo", { configurable: true, value: scrollTo });
    fireEvent.scroll(thread);

    view.rerender(<SpecThread entries={[first, second]} isRunning={false} onActivity={vi.fn()} />);
    expect(scrollTo).toHaveBeenCalledWith({ top: 500 });

    scrollTo.mockClear();
    thread.scrollTop = 120;
    fireEvent.scroll(thread);
    view.rerender(
      <SpecThread entries={[first, second, third]} isRunning={false} onActivity={vi.fn()} />,
    );
    expect(scrollTo).not.toHaveBeenCalled();
  });
});

function human(id: string, text: string): SpecThreadEntry {
  return {
    kind: "human",
    id,
    promptId: id,
    author: { id: "person", name: "Person" },
    text,
    createdAt: AT,
  };
}

function indexed(events: SessionEvent[]): IndexedEvent[] {
  return events.map((event, idx) => ({ idx, event }));
}
