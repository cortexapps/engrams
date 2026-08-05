import { afterEach, describe, expect, test } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { IndexedEvent, SessionEvent } from "../events";
import { WorkDock } from "./WorkDock";

afterEach(cleanup);

const AT = "2026-08-05T12:00:00.000Z";

function indexed(events: SessionEvent[]): IndexedEvent[] {
  return events.map((event, idx) => ({ idx, event }));
}

function taskEvents(): SessionEvent[] {
  const create = (id: string, subject: string): SessionEvent => ({
    type: "tool_call_started",
    run_id: "run-1",
    tool_call_id: `create-${id}`,
    tool_name: "TaskCreate",
    args_summary: JSON.stringify({ subject, activeForm: `Doing ${subject}` }),
    at: AT,
  });
  const completed = (id: string, subject: string): SessionEvent => ({
    type: "tool_call_completed",
    run_id: "run-1",
    tool_call_id: `create-${id}`,
    tool_name: "TaskCreate",
    ok: true,
    duration_ms: 1,
    result_summary: `Task #${id} created successfully: ${subject}`,
    at: AT,
  });
  const update = (id: string, status: "in_progress" | "completed"): SessionEvent => ({
    type: "tool_call_started",
    run_id: "run-1",
    tool_call_id: `update-${id}-${status}`,
    tool_name: "TaskUpdate",
    args_summary: JSON.stringify({ taskId: id, status }),
    at: AT,
  });

  return [
    create("1", "Finished task"),
    completed("1", "Finished task"),
    create("2", "Current task"),
    completed("2", "Current task"),
    create("3", "Pending task"),
    completed("3", "Pending task"),
    update("1", "completed"),
    update("2", "in_progress"),
  ];
}

function planEvent(): SessionEvent {
  return {
    type: "tool_call_requested",
    run_id: "run-1",
    tool_call_id: "plan-1",
    name: "exit_plan_mode",
    args_json: JSON.stringify({ plan: "# Proposed plan" }),
    at: AT,
  };
}

describe("WorkDock", () => {
  test("renders null when there are no tasks or plan revisions", () => {
    const { container } = render(<WorkDock events={[]} />);
    expect(container.firstChild).toBeNull();
  });

  test("shows progress and the current subject in the collapsed bar", () => {
    const { container } = render(<WorkDock events={indexed(taskEvents())} />);
    expect(screen.getByText("1 of 3")).toBeTruthy();
    expect(screen.getByText("Current task")).toBeTruthy();
    expect(screen.queryAllByTestId("work-dock-task-row")).toHaveLength(0);
    expect(container.querySelector(".lucide-circle-dot")).toBeTruthy();
  });

  test("uses an idle glyph until every task is complete", () => {
    const pending = taskEvents().slice(0, 2);
    const { container, rerender } = render(<WorkDock events={indexed(pending)} />);
    expect(container.querySelector(".lucide-circle-dashed")).toBeTruthy();
    expect(container.querySelector(".lucide-circle-check")).toBeNull();

    rerender(<WorkDock events={indexed([...pending, taskEvents()[6]!])} />);
    expect(container.querySelector(".lucide-circle-check")).toBeTruthy();

    rerender(<WorkDock events={indexed([planEvent()])} />);
    expect(container.querySelector(".lucide-circle-dashed")).toBeTruthy();
    expect(container.querySelector(".lucide-circle-check")).toBeNull();
  });

  test("shows ordered task rows after expanding", async () => {
    render(<WorkDock events={indexed(taskEvents())} />);
    await userEvent.click(screen.getByRole("button", { expanded: false }));

    expect(screen.getAllByTestId("work-dock-task-row")).toHaveLength(3);
    expect(screen.getAllByTestId("work-dock-task-row")[0]!.textContent).toContain("Current task");
  });

  test("shows the tab switcher only when tasks and a plan both exist", async () => {
    const { unmount } = render(<WorkDock events={indexed(taskEvents())} />);
    await userEvent.click(screen.getByRole("button", { expanded: false }));
    expect(screen.queryByRole("tab")).toBeNull();
    unmount();

    render(<WorkDock events={indexed([...taskEvents(), planEvent()])} />);
    await userEvent.click(screen.getByRole("button", { expanded: false }));
    expect(screen.getByRole("tab", { name: "Tasks" })).toBeTruthy();
    expect(screen.getByRole("tab", { name: "Plan" })).toBeTruthy();
  });
});
